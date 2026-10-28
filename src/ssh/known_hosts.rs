//! Server host key verification, mirroring OpenSSH's StrictHostKeyChecking.

use std::path::PathBuf;

use anyhow::{Result, bail};
use russh::keys::{self, PublicKeyOrCertificate};
use tracing::{info, warn};

use crate::config::{ResolvedHost, StrictHostKeyChecking};

#[derive(Debug, Clone)]
pub struct HostKeyPolicy {
    pub mode: StrictHostKeyChecking,
    pub path: PathBuf,
    pub host: String,
    pub port: u16,
}

impl HostKeyPolicy {
    pub fn new(host: &ResolvedHost) -> Result<Self> {
        let path = match &host.known_hosts {
            Some(path) => path.clone(),
            None => default_known_hosts()?,
        };
        Ok(Self {
            mode: host.strict_host_key_checking,
            path,
            host: host.address.clone(),
            port: host.port,
        })
    }

    /// Decide whether to trust the key the server presented.
    pub fn accept(&self, key: &PublicKeyOrCertificate) -> Result<bool> {
        if self.mode == StrictHostKeyChecking::No {
            warn!(host = %self.host, "accepting the host key unchecked (strict_host_key_checking = \"no\")");
            return Ok(true);
        }

        let key = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key,
            PublicKeyOrCertificate::Certificate(_) => bail!(
                "host `{}` presented a certificate host key, which is only accepted with \
                 strict_host_key_checking = \"no\"",
                self.host
            ),
        };

        match keys::check_known_hosts_path(&self.host, self.port, key, &self.path) {
            Ok(true) => Ok(true),
            Ok(false) => match self.mode {
                StrictHostKeyChecking::AcceptNew => {
                    keys::known_hosts::learn_known_hosts_path(
                        &self.host, self.port, key, &self.path,
                    )?;
                    info!(
                        host = %self.host,
                        fingerprint = %key.fingerprint(Default::default()),
                        "learned a new host key",
                    );
                    Ok(true)
                }
                _ => bail!(
                    "host `{}` is not in {} (fingerprint {}); add it, or set \
                     strict_host_key_checking = \"accept-new\"",
                    self.host,
                    self.path.display(),
                    key.fingerprint(Default::default()),
                ),
            },
            // A changed key is rejected under every mode but "no".
            Err(keys::Error::KeyChanged { line }) => bail!(
                "the host key for `{}` does not match {}:{} — this may be a \
                 man-in-the-middle attack",
                self.host,
                self.path.display(),
                line,
            ),
            Err(err) => Err(err.into()),
        }
    }
}

fn default_known_hosts() -> Result<PathBuf> {
    let base = directories::BaseDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .ok_or_else(|| anyhow::anyhow!("could not determine the user's home directory"))?;
    Ok(base.join(".ssh").join("known_hosts"))
}
