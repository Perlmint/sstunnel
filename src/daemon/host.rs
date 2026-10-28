//! One task per SSH host: connect, authenticate, keep alive, reconnect.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use rand::RngExt;
use russh::client::{self, Handle};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::config::ResolvedHost;
use crate::ipc::protocol::HostState;
use crate::ssh::auth;
use crate::ssh::handler::{ClientHandler, RouteTable, SessionClosed};
use crate::ssh::known_hosts::HostKeyPolicy;

pub type Session = Arc<Handle<ClientHandler>>;

/// A live host supervisor. Cloning is cheap; every clone talks to one task.
#[derive(Clone)]
pub struct HostHandle {
    pub name: String,
    pub host: ResolvedHost,
    pub routes: Arc<RouteTable>,
    session: watch::Receiver<Option<Session>>,
    state: watch::Receiver<HostState>,
    token: CancellationToken,
}

impl HostHandle {
    /// Start supervising `config`. The task runs until `shutdown` is called.
    pub fn spawn(name: String, host: ResolvedHost, parent: &CancellationToken) -> Self {
        let token = parent.child_token();
        let routes = Arc::new(RouteTable::default());
        let (session_tx, session_rx) = watch::channel(None);
        let (state_tx, state_rx) = watch::channel(HostState::Connecting);

        let handle = Self {
            name: name.clone(),
            host: host.clone(),
            routes: Arc::clone(&routes),
            session: session_rx,
            state: state_rx,
            token: token.clone(),
        };

        tokio::spawn(supervise(name, host, routes, session_tx, state_tx, token));
        handle
    }

    /// The session as of right now, if the host is connected.
    pub fn session(&self) -> Option<Session> {
        self.session.borrow().clone()
    }

    /// Wait until the host is connected. `None` means the supervisor stopped.
    pub async fn wait_for_session(&self) -> Option<Session> {
        let mut rx = self.session.clone();
        loop {
            let current = rx.borrow_and_update().clone();
            if current.is_some() {
                return current;
            }
            rx.changed().await.ok()?;
        }
    }

    /// Wait until `session` is no longer the live one.
    pub async fn wait_until_lost(&self, session: &Session) {
        let mut rx = self.session.clone();
        loop {
            let still_current = rx
                .borrow_and_update()
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, session));
            if !still_current || session.is_closed() {
                return;
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    pub fn state(&self) -> HostState {
        self.state.borrow().clone()
    }

    /// A receiver for state changes, for callers that want to react to them.
    pub fn state_watch(&self) -> watch::Receiver<HostState> {
        self.state.clone()
    }

    pub fn shutdown(&self) {
        self.token.cancel();
    }
}

async fn supervise(
    name: String,
    config: ResolvedHost,
    routes: Arc<RouteTable>,
    session_tx: watch::Sender<Option<Session>>,
    state_tx: watch::Sender<HostState>,
    token: CancellationToken,
) {
    let mut attempts: u32 = 0;
    let mut backoff = config.reconnect.initial;

    while !token.is_cancelled() {
        let _ = state_tx.send(HostState::Connecting);
        match connect(&name, &config, Arc::clone(&routes), &token).await {
            Ok(Some((session, closed))) => {
                attempts = 0;
                backoff = config.reconnect.initial;
                info!(host = %name, "connected");
                let _ = state_tx.send(HostState::Connected);
                let _ = session_tx.send(Some(Arc::clone(&session)));

                wait_until_closed(&session, &closed, &token).await;

                let _ = session_tx.send(None);
                if token.is_cancelled() {
                    break;
                }
                warn!(host = %name, "the session ended, reconnecting");
                let _ = state_tx.send(HostState::Reconnecting {
                    attempts: 0,
                    error: "the session ended".into(),
                });
            }
            // Cancelled mid-connect.
            Ok(None) => break,
            Err(err) => {
                attempts += 1;
                let message = format!("{err:#}");
                error!(host = %name, attempts, "{message}");
                let _ = state_tx.send(HostState::Reconnecting {
                    attempts,
                    error: message,
                });
            }
        }

        if token.is_cancelled() {
            break;
        }
        let delay = jittered(backoff);
        debug!(host = %name, ?delay, "waiting before the next connection attempt");
        tokio::select! {
            _ = token.cancelled() => break,
            _ = tokio::time::sleep(delay) => {}
        }
        backoff = next_backoff(backoff, &config);
    }

    let _ = session_tx.send(None);
    let _ = state_tx.send(HostState::Idle);
    debug!(host = %name, "supervisor stopped");
}

/// `Ok(None)` means the attempt was cancelled rather than failed.
async fn connect(
    name: &str,
    config: &ResolvedHost,
    routes: Arc<RouteTable>,
    token: &CancellationToken,
) -> Result<Option<(Session, Arc<SessionClosed>)>> {
    let policy = HostKeyPolicy::new(config)?;
    let closed = Arc::new(SessionClosed::default());
    let handler = ClientHandler {
        host: name.to_owned(),
        policy,
        routes,
        closed: Arc::clone(&closed),
    };

    let client_config = Arc::new(client::Config {
        keepalive_interval: Some(config.keepalive_interval),
        keepalive_max: config.keepalive_max,
        nodelay: true,
        ..Default::default()
    });

    let address = (config.address.clone(), config.port);
    debug!(host = %name, address = %config.address, port = config.port, "connecting");

    let connecting = client::connect(client_config, address, handler);
    let mut session = tokio::select! {
        _ = token.cancelled() => return Ok(None),
        result = tokio::time::timeout(config.connect_timeout, connecting) => result
            .with_context(|| format!("connecting to {}:{} timed out", config.address, config.port))?
            .with_context(|| format!("connecting to {}:{}", config.address, config.port))?,
    };

    let method = tokio::select! {
        _ = token.cancelled() => return Ok(None),
        result = auth::authenticate(&mut session, config, name) => result?,
    };
    debug!(host = %name, %method, "authenticated");

    Ok(Some((Arc::new(session), closed)))
}

/// Return once the session is no longer usable, or the host is shutting down.
async fn wait_until_closed(
    session: &Session,
    closed: &Arc<SessionClosed>,
    token: &CancellationToken,
) {
    loop {
        if closed.is_closed() || session.is_closed() {
            return;
        }
        tokio::select! {
            _ = token.cancelled() => return,
            _ = closed.notified() => {}
            // `disconnected` is not guaranteed to fire for every failure mode,
            // so poll the handle as a backstop.
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
    }
}

fn next_backoff(current: Duration, config: &ResolvedHost) -> Duration {
    let scaled = current.mul_f64(config.reconnect.factor);
    scaled.min(config.reconnect.max)
}

/// Spread reconnect storms when several hosts drop at once.
fn jittered(delay: Duration) -> Duration {
    let factor = rand::rng().random_range(0.8..1.2);
    delay.mul_f64(factor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HostSources, ReconnectConfig, ValueSource};

    fn config(reconnect: ReconnectConfig) -> ResolvedHost {
        ResolvedHost {
            alias: "h".into(),
            address: "h".into(),
            port: 22,
            user: "tester".into(),
            identity: vec![],
            identity_passphrase_env: None,
            use_agent: true,
            known_hosts: None,
            strict_host_key_checking: Default::default(),
            connect_timeout: Duration::from_secs(1),
            keepalive_interval: Duration::from_secs(1),
            keepalive_max: 3,
            reconnect,
            persistent: false,
            sources: HostSources {
                address: ValueSource::Default,
                port: ValueSource::Default,
                user: ValueSource::Config,
                identity: ValueSource::Default,
            },
        }
    }

    #[test]
    fn backoff_grows_and_then_holds_at_the_maximum() {
        let config = config(ReconnectConfig {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(8),
            factor: 3.0,
        });
        let mut delay = config.reconnect.initial;
        delay = next_backoff(delay, &config);
        assert_eq!(delay, Duration::from_secs(3));
        delay = next_backoff(delay, &config);
        assert_eq!(delay, Duration::from_secs(8), "capped at max");
        assert_eq!(next_backoff(delay, &config), Duration::from_secs(8));
    }

    #[test]
    fn jitter_stays_within_twenty_percent() {
        for _ in 0..100 {
            let delay = jittered(Duration::from_secs(10));
            assert!(delay >= Duration::from_secs(8) && delay <= Duration::from_secs(12));
        }
    }
}
