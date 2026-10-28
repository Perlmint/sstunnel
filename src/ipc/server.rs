//! Daemon side of the control channel.

use std::sync::Arc;

use anyhow::{Context, Result};
use interprocess::local_socket::ListenerOptions;
use interprocess::local_socket::tokio::Stream;
use interprocess::local_socket::traits::tokio::Listener as _;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::protocol::{Request, Response};
use crate::daemon::Command;

/// A bound control endpoint. Dropping it releases the socket.
pub struct ControlSocket {
    listener: interprocess::local_socket::tokio::Listener,
    endpoint: String,
}

impl ControlSocket {
    /// Bind the endpoint, clearing a socket left behind by a crashed daemon.
    pub async fn bind(endpoint: &str) -> Result<Self> {
        match Self::try_bind(endpoint) {
            Ok(listener) => Ok(listener),
            Err(err) if is_addr_in_use(&err) => {
                if super::client::probe(endpoint).await {
                    anyhow::bail!("another ssh-tunnel daemon is already listening on {endpoint}");
                }
                debug!(endpoint, "clearing a stale control socket");
                #[cfg(unix)]
                let _ = std::fs::remove_file(endpoint);
                Self::try_bind(endpoint)
                    .with_context(|| format!("binding control socket {endpoint}"))
            }
            Err(err) => Err(err).with_context(|| format!("binding control socket {endpoint}")),
        }
    }

    fn try_bind(endpoint: &str) -> Result<Self> {
        let name = super::to_name(endpoint)?;
        let listener = ListenerOptions::new().name(name).create_tokio()?;
        restrict_to_owner(endpoint);
        Ok(Self {
            listener,
            endpoint: endpoint.to_owned(),
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Accept until `token` is cancelled, forwarding requests to the daemon.
    pub async fn serve(self, commands: mpsc::Sender<Command>, token: CancellationToken) {
        let commands = Arc::new(commands);
        loop {
            let stream = tokio::select! {
                _ = token.cancelled() => break,
                accepted = self.listener.accept() => match accepted {
                    Ok(stream) => stream,
                    Err(err) => {
                        warn!(%err, "control socket accept failed");
                        continue;
                    }
                },
            };
            let commands = Arc::clone(&commands);
            tokio::spawn(async move {
                if let Err(err) = handle_connection(stream, &commands).await {
                    debug!(%err, "control connection ended");
                }
            });
        }
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.endpoint);
    }
}

/// One request, one response, then close.
async fn handle_connection(stream: Stream, commands: &mpsc::Sender<Command>) -> Result<()> {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        return Ok(());
    }
    let response = match serde_json::from_str::<Request>(line.trim_end()) {
        Ok(request) => dispatch(request, commands).await,
        Err(err) => Response::error(format!("malformed request: {err}")),
    };

    let mut body = serde_json::to_vec(&response)?;
    body.push(b'\n');
    let mut writer = &stream;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

async fn dispatch(request: Request, commands: &mpsc::Sender<Command>) -> Response {
    if matches!(request, Request::Ping) {
        return Response::ok("pong");
    }
    let (reply, wait) = oneshot::channel();
    if commands
        .send(Command::Ipc { request, reply })
        .await
        .is_err()
    {
        return Response::error("the daemon is shutting down");
    }
    wait.await
        .unwrap_or_else(|_| Response::error("the daemon dropped the request"))
}

fn is_addr_in_use(err: &anyhow::Error) -> bool {
    err.downcast_ref::<std::io::Error>()
        .is_some_and(|err| err.kind() == std::io::ErrorKind::AddrInUse)
}

/// Windows named pipes already default to a per-user DACL; unix sockets do not.
fn restrict_to_owner(endpoint: &str) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(err) = std::fs::set_permissions(endpoint, std::fs::Permissions::from_mode(0o600))
        {
            warn!(endpoint, %err, "could not restrict control socket permissions");
        }
    }
    #[cfg(not(unix))]
    let _ = endpoint;
}
