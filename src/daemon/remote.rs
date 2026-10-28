//! `-R`: ask the server to listen, and serve what it sends back.
//!
//! The route must be registered before the request goes out, because the server
//! may open a channel the instant it starts listening.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

use super::forward::ForwardStats;
use super::host::HostHandle;
use crate::ipc::protocol::ForwardState;
use crate::ssh::handler::RemoteRoute;

/// How long to wait before re-requesting a forward the server refused.
const RETRY_DELAY: Duration = Duration::from_secs(5);

pub struct Remote {
    pub name: String,
    pub listen: (String, u16),
    pub target: (String, u16),
    pub host: HostHandle,
    pub stats: Arc<ForwardStats>,
    pub state: watch::Sender<ForwardState>,
    pub token: CancellationToken,
}

pub async fn run(ctx: Remote) {
    while !ctx.token.is_cancelled() {
        let _ = ctx.state.send(ForwardState::WaitingForHost);

        let session = tokio::select! {
            _ = ctx.token.cancelled() => break,
            session = ctx.host.wait_for_session() => match session {
                Some(session) => session,
                None => break,
            },
        };

        // Register first: an inbound channel can arrive before tcpip_forward returns.
        let mut route = RemoteRoute {
            forward: ctx.name.clone(),
            bind: (ctx.listen.0.clone(), u32::from(ctx.listen.1)),
            target: ctx.target.clone(),
            stats: Arc::clone(&ctx.stats),
            token: ctx.token.clone(),
        };
        ctx.host.routes.insert(route.clone());

        match session
            .tcpip_forward(ctx.listen.0.clone(), u32::from(ctx.listen.1))
            .await
        {
            Ok(allocated) => {
                // Port 0 means "server, pick one"; otherwise keep what we asked for.
                let port = if ctx.listen.1 == 0 {
                    allocated
                } else {
                    u32::from(ctx.listen.1)
                };
                route.bind.1 = port;
                ctx.host.routes.insert(route);

                let addr = format!("{}:{}", ctx.listen.0, port);
                info!(forward = %ctx.name, %addr, "the server is listening");
                let _ = ctx.state.send(ForwardState::Listening { addr });

                tokio::select! {
                    _ = ctx.token.cancelled() => {}
                    _ = ctx.host.wait_until_lost(&session) => {
                        debug!(forward = %ctx.name, "the session went away, will re-request");
                    }
                }

                // Best effort: the session is often already gone by now.
                let _ = session
                    .cancel_tcpip_forward(ctx.listen.0.clone(), port)
                    .await;
            }
            Err(err) => {
                let message = format!(
                    "the server refused to listen on {}:{}: {err} \
                     (a non-loopback bind needs GatewayPorts on the server)",
                    ctx.listen.0, ctx.listen.1
                );
                error!(forward = %ctx.name, "{message}");
                ctx.host.routes.remove(&ctx.name);
                let _ = ctx.state.send(ForwardState::Failed { error: message });
                tokio::select! {
                    _ = ctx.token.cancelled() => break,
                    _ = tokio::time::sleep(RETRY_DELAY) => {}
                }
            }
        }
    }

    ctx.host.routes.remove(&ctx.name);
    let _ = ctx.state.send(ForwardState::Disabled);
    debug!(forward = %ctx.name, "stopped");
}
