//! The russh client handler: host key checks, inbound remote-forward channels,
//! and disconnect notification.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use russh::client::ChannelOpenHandle;
use russh::client::{self, Msg, Session};
use russh::keys::PublicKeyOrCertificate;
use russh::{Channel, ChannelOpenFailure};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

use crate::daemon::forward::{ForwardStats, pump};
use crate::ssh::known_hosts::HostKeyPolicy;

/// Where an inbound remote-forward connection should be delivered.
#[derive(Debug, Clone)]
pub struct RemoteRoute {
    pub forward: String,
    /// What we asked the server to bind, updated with the port it allocated.
    pub bind: (String, u32),
    pub target: (String, u16),
    pub stats: Arc<ForwardStats>,
    pub token: CancellationToken,
}

/// Remote-forward routes for one SSH session, shared with the handler.
#[derive(Debug, Default)]
pub struct RouteTable {
    routes: Mutex<Vec<RemoteRoute>>,
}

impl RouteTable {
    pub fn insert(&self, route: RemoteRoute) {
        let mut routes = self.routes.lock().unwrap();
        routes.retain(|existing| existing.forward != route.forward);
        routes.push(route);
    }

    pub fn remove(&self, forward: &str) {
        self.routes
            .lock()
            .unwrap()
            .retain(|route| route.forward != forward);
    }

    /// Servers do not always echo the bind address verbatim (`localhost` vs
    /// `127.0.0.1`), so fall back to matching on the port alone.
    pub fn lookup(&self, address: &str, port: u32) -> Option<RemoteRoute> {
        let routes = self.routes.lock().unwrap();
        routes
            .iter()
            .find(|route| route.bind.0 == address && route.bind.1 == port)
            .or_else(|| routes.iter().find(|route| route.bind.1 == port))
            .cloned()
    }
}

/// Set when the session ends, so the supervisor can start reconnecting.
#[derive(Debug, Default)]
pub struct SessionClosed {
    closed: AtomicBool,
    notify: Notify,
}

impl SessionClosed {
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    pub fn mark(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub async fn notified(&self) {
        self.notify.notified().await;
    }
}

pub struct ClientHandler {
    pub host: String,
    pub policy: HostKeyPolicy,
    pub routes: Arc<RouteTable>,
    pub closed: Arc<SessionClosed>,
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        match self.policy.accept(server_public_key) {
            Ok(accepted) => Ok(accepted),
            Err(err) => {
                // Returning an error here aborts the handshake with a generic
                // message, so log the real reason before rejecting.
                error!(host = %self.host, "{err:#}");
                Ok(false)
            }
        }
    }

    async fn auth_banner(
        &mut self,
        banner: &str,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        debug!(host = %self.host, banner = banner.trim(), "auth banner");
        Ok(())
    }

    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: Channel<Msg>,
        connected_address: &str,
        connected_port: u32,
        originator_address: &str,
        originator_port: u32,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let Some(route) = self.routes.lookup(connected_address, connected_port) else {
            warn!(
                host = %self.host,
                %connected_address,
                connected_port,
                "rejecting a forwarded connection with no matching forward",
            );
            reply
                .reject(ChannelOpenFailure::AdministrativelyProhibited)
                .await;
            return Ok(());
        };

        debug!(
            host = %self.host,
            forward = %route.forward,
            origin = format!("{originator_address}:{originator_port}"),
            "inbound remote-forward connection",
        );

        // Serve off the session loop so one slow target cannot stall the session.
        tokio::spawn(async move {
            if let Err(err) = serve_forwarded(channel, reply, &route).await {
                warn!(forward = %route.forward, %err, "remote forward connection failed");
            }
        });
        Ok(())
    }

    async fn disconnected(
        &mut self,
        reason: client::DisconnectReason<Self::Error>,
    ) -> Result<(), Self::Error> {
        match &reason {
            client::DisconnectReason::ReceivedDisconnect(_) => {
                debug!(host = %self.host, "the server closed the session")
            }
            client::DisconnectReason::Error(err) => {
                debug!(host = %self.host, %err, "the session ended with an error")
            }
        }
        self.closed.mark();
        Ok(())
    }
}

/// Dial the local target, accept the channel, and pump bytes.
async fn serve_forwarded(
    channel: Channel<Msg>,
    reply: ChannelOpenHandle,
    route: &RemoteRoute,
) -> anyhow::Result<()> {
    let target = format!("{}:{}", route.target.0, route.target.1);
    let local = match TcpStream::connect(&target).await {
        Ok(stream) => stream,
        Err(err) => {
            reply.reject(ChannelOpenFailure::ConnectFailed).await;
            return Err(anyhow::Error::from(err).context(format!("connecting to {target}")));
        }
    };
    reply.accept().await;

    let _guard = route.stats.open();
    let remote = channel.into_stream();
    tokio::select! {
        _ = route.token.cancelled() => Ok(()),
        result = pump(local, remote, &route.stats) => Ok(result?),
    }
}
