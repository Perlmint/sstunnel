//! Authentication: the ssh-agent first, then any configured key files.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use russh::client::Handle;
use russh::keys::PrivateKeyWithHashAlg;
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::{AgentClient, AgentStream};
use tracing::{debug, warn};

use crate::config::ResolvedHost;
use crate::ssh::handler::ClientHandler;

type BoxedAgent = AgentClient<Box<dyn AgentStream + Send + Unpin>>;

/// Authenticate `session`, trying every configured method in turn.
///
/// Returns the name of the method that worked, or an error listing what was
/// attempted — a background service has no user to prompt, so a clear log line
/// is the only diagnostic available.
pub async fn authenticate(
    session: &mut Handle<ClientHandler>,
    host: &ResolvedHost,
    host_name: &str,
) -> Result<String> {
    let user = host.user.clone();
    let rsa_hash = session.best_supported_rsa_hash().await?.flatten();
    let mut attempts: Vec<String> = Vec::new();

    if host.use_agent {
        match try_agent(session, &user, rsa_hash, host_name).await {
            Ok(Some(method)) => return Ok(method),
            Ok(None) => attempts.push("ssh-agent (no identity accepted)".into()),
            Err(err) => attempts.push(format!("ssh-agent ({err:#})")),
        }
    }

    for path in &host.identity {
        match try_key_file(session, &user, rsa_hash, host, path).await {
            Ok(true) => return Ok(format!("publickey ({})", path.display())),
            Ok(false) => attempts.push(format!("{} (rejected)", path.display())),
            Err(err) => attempts.push(format!("{} ({err:#})", path.display())),
        }
    }

    if attempts.is_empty() {
        bail!("no authentication methods are configured for host `{host_name}`");
    }
    bail!(
        "authentication failed for {user}@{}: {}",
        host.address,
        attempts.join("; ")
    )
}

async fn try_agent(
    session: &mut Handle<ClientHandler>,
    user: &str,
    rsa_hash: Option<russh::keys::HashAlg>,
    host_name: &str,
) -> Result<Option<String>> {
    let mut agent = connect_agent().await?;
    let identities = agent
        .request_identities()
        .await
        .context("listing agent identities")?;
    debug!(
        host = host_name,
        count = identities.len(),
        "ssh-agent identities"
    );

    for identity in identities {
        let result = match &identity {
            AgentIdentity::PublicKey { key, .. } => {
                session
                    .authenticate_publickey_with(user, key.clone(), rsa_hash, &mut agent)
                    .await
            }
            AgentIdentity::Certificate { certificate, .. } => {
                session
                    .authenticate_certificate_with(user, certificate.clone(), rsa_hash, &mut agent)
                    .await
            }
        };
        match result {
            Ok(result) if result.success() => {
                return Ok(Some(format!("ssh-agent ({})", describe(&identity))));
            }
            Ok(_) => debug!(identity = %describe(&identity), "agent identity rejected"),
            Err(err) => warn!(identity = %describe(&identity), %err, "agent signing failed"),
        }
    }
    Ok(None)
}

async fn try_key_file(
    session: &mut Handle<ClientHandler>,
    user: &str,
    rsa_hash: Option<russh::keys::HashAlg>,
    host: &ResolvedHost,
    path: &std::path::Path,
) -> Result<bool> {
    let passphrase =
        match &host.identity_passphrase_env {
            Some(var) => Some(std::env::var(var).with_context(|| {
                format!("identity_passphrase_env names `{var}`, which is not set")
            })?),
            None => None,
        };
    let key = russh::keys::load_secret_key(path, passphrase.as_deref())
        .with_context(|| format!("loading {}", path.display()))?;
    let result = session
        .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), rsa_hash))
        .await?;
    Ok(result.success())
}

/// Connect to the platform's ssh-agent, boxing the stream so both branches
/// share one type.
async fn connect_agent() -> Result<BoxedAgent> {
    #[cfg(unix)]
    {
        if std::env::var("SSH_AUTH_SOCK").is_err() {
            bail!("SSH_AUTH_SOCK is not set");
        }
        Ok(AgentClient::connect_env().await?.dynamic())
    }
    #[cfg(windows)]
    {
        const OPENSSH_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";
        match AgentClient::connect_named_pipe(OPENSSH_PIPE).await {
            Ok(agent) => Ok(agent.dynamic()),
            Err(err) => {
                debug!(%err, "the OpenSSH agent pipe is unavailable, trying Pageant");
                Ok(AgentClient::connect_pageant().await?.dynamic())
            }
        }
    }
}

fn describe(identity: &AgentIdentity) -> String {
    match identity {
        AgentIdentity::PublicKey { key, comment } => {
            format!("{} {comment}", key.fingerprint(Default::default()))
        }
        AgentIdentity::Certificate {
            certificate,
            comment,
        } => {
            format!("cert {} {comment}", certificate.key_id())
        }
    }
}
