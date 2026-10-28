//! Shared machinery for forwards that bind a local port (local and dynamic).
//!
//! The listener stays bound across SSH reconnects: dropping and re-binding
//! would let another process steal the port and would turn every reconnect into
//! a burst of connection-refused errors for clients.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::forward::ForwardStats;
use super::host::{HostHandle, Session};
use crate::ipc::protocol::{ForwardState, HostState};

/// How long to wait before trying a failed bind again.
const REBIND_DELAY: Duration = Duration::from_secs(5);

pub struct Listen {
    pub name: String,
    pub listen: (String, u16),
    pub host: HostHandle,
    pub stats: Arc<ForwardStats>,
    pub state: watch::Sender<ForwardState>,
    pub token: CancellationToken,
}

/// Bind, then serve accepted connections with `handler` until cancelled.
pub async fn run<F, Fut>(ctx: Listen, handler: F)
where
    F: Fn(TcpStream, SocketAddr, Session, Arc<ForwardStats>, CancellationToken) -> Fut
        + Clone
        + Send
        + Sync
        + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    while !ctx.token.is_cancelled() {
        let listener = match TcpListener::bind((ctx.listen.0.as_str(), ctx.listen.1)).await {
            Ok(listener) => listener,
            Err(err) => {
                let message = format!("cannot bind {}:{}: {err}", ctx.listen.0, ctx.listen.1);
                error!(forward = %ctx.name, "{message}");
                let _ = ctx.state.send(ForwardState::Failed { error: message });
                tokio::select! {
                    _ = ctx.token.cancelled() => break,
                    _ = tokio::time::sleep(REBIND_DELAY) => continue,
                }
            }
        };

        let addr = listener
            .local_addr()
            .map(|addr| addr.to_string())
            .unwrap_or_else(|_| format!("{}:{}", ctx.listen.0, ctx.listen.1));
        info!(forward = %ctx.name, %addr, "listening");

        // Reflect the host's connectivity in this forward's state while bound.
        let mirror = ctx.token.child_token();
        tokio::spawn(mirror_host_state(
            ctx.host.clone(),
            ctx.state.clone(),
            addr,
            mirror.clone(),
        ));

        accept_loop(&listener, &ctx, &handler).await;
        mirror.cancel();
    }

    let _ = ctx.state.send(ForwardState::Disabled);
    debug!(forward = %ctx.name, "stopped");
}

async fn accept_loop<F, Fut>(listener: &TcpListener, ctx: &Listen, handler: &F)
where
    F: Fn(TcpStream, SocketAddr, Session, Arc<ForwardStats>, CancellationToken) -> Fut
        + Clone
        + Send
        + Sync
        + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    loop {
        let (stream, peer) = tokio::select! {
            _ = ctx.token.cancelled() => return,
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
                Err(err) => {
                    // Back off briefly so a persistent error (EMFILE) cannot spin.
                    warn!(forward = %ctx.name, %err, "accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
        };

        let Some(session) = ctx.host.session() else {
            debug!(forward = %ctx.name, host = %ctx.host.name, %peer, "refusing a connection: the host is not connected");
            continue;
        };

        let handler = handler.clone();
        let stats = Arc::clone(&ctx.stats);
        let token = ctx.token.child_token();
        let name = ctx.name.clone();
        tokio::spawn(async move {
            if let Err(err) = handler(stream, peer, session, stats, token).await {
                debug!(forward = %name, %peer, "{err:#}");
            }
        });
    }
}

async fn mirror_host_state(
    host: HostHandle,
    state: watch::Sender<ForwardState>,
    addr: String,
    token: CancellationToken,
) {
    let mut rx = host.state_watch();
    loop {
        let host_state = rx.borrow_and_update().clone();
        let _ = state.send(match host_state {
            HostState::Connected => ForwardState::Listening { addr: addr.clone() },
            _ => ForwardState::WaitingForHost,
        });
        tokio::select! {
            _ = token.cancelled() => return,
            changed = rx.changed() => if changed.is_err() { return },
        }
    }
}
