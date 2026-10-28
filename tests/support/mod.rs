//! A hermetic test rig: an in-process SSH server, an echo target, and a TCP
//! proxy that can sever live connections to simulate a dropped link.
//!
//! Everything runs on loopback with generated keys, so the suite needs no sshd
//! and behaves the same on every platform.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, PrivateKey};
use russh::server::{self, Auth, Msg, Server as _, Session};
use russh::{Channel, ChannelOpenFailure};
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// A TCP server that echoes everything back.
pub async fn echo_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let (mut reader, mut writer) = stream.split();
                let _ = tokio::io::copy(&mut reader, &mut writer).await;
            });
        }
    });
    addr
}

/// A TCP server that answers with a fixed banner and then echoes.
pub async fn banner_server(banner: &'static [u8]) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let _ = stream.write_all(banner).await;
                let (mut reader, mut writer) = stream.split();
                let _ = tokio::io::copy(&mut reader, &mut writer).await;
            });
        }
    });
    addr
}

// ---------------------------------------------------------------------------
// SSH server
// ---------------------------------------------------------------------------

pub struct TestSshServer {
    pub addr: SocketAddr,
    /// The key clients should authenticate with; the server accepts any key,
    /// but the client needs a real one to offer.
    pub client_key: PrivateKey,
}

impl TestSshServer {
    pub async fn start() -> Self {
        let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let client_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();

        let config = Arc::new(server::Config {
            keys: vec![host_key],
            inactivity_timeout: None,
            auth_rejection_time: Duration::from_millis(1),
            nodelay: true,
            ..Default::default()
        });

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let mut server = Rig;
            let _ = server.run_on_socket(config, &listener).await;
        });

        Self { addr, client_key }
    }

    /// Write the client key where the config file can point at it.
    pub fn write_client_key(&self, dir: &Path) -> PathBuf {
        let path = dir.join("id_ed25519");
        let pem = self.client_key.to_openssh(LineEnding::LF).unwrap();
        std::fs::write(&path, pem.as_bytes()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        path
    }
}

struct Rig;

impl server::Server for Rig {
    type Handler = RigSession;

    fn new_client(&mut self, _peer: Option<SocketAddr>) -> RigSession {
        RigSession {
            token: CancellationToken::new(),
        }
    }
}

/// One SSH session. Remote-forward listeners are tied to its lifetime, so a
/// dropped session releases the ports it bound.
struct RigSession {
    token: CancellationToken,
}

impl Drop for RigSession {
    fn drop(&mut self) {
        self.token.cancel();
    }
}

impl server::Handler for RigSession {
    type Error = russh::Error;

    async fn auth_publickey(
        &mut self,
        _user: &str,
        _key: &russh::keys::PublicKey,
    ) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }

    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host_to_connect: &str,
        port_to_connect: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let target = format!("{host_to_connect}:{port_to_connect}");
        tokio::spawn(async move {
            match TcpStream::connect(&target).await {
                Ok(mut upstream) => {
                    reply.accept().await;
                    let mut stream = channel.into_stream();
                    let _ = copy_bidirectional(&mut upstream, &mut stream).await;
                }
                Err(_) => reply.reject(ChannelOpenFailure::ConnectFailed).await,
            }
        });
        Ok(())
    }

    async fn tcpip_forward(
        &mut self,
        address: &str,
        port: &mut u32,
        session: &mut Session,
    ) -> Result<bool, Self::Error> {
        let listener = match bind_with_retry(address, *port as u16).await {
            Some(listener) => listener,
            None => return Ok(false),
        };
        let bound = listener.local_addr().map_err(russh::Error::from)?.port() as u32;
        *port = bound;

        let handle = session.handle();
        let address = address.to_owned();
        let token = self.token.clone();
        tokio::spawn(async move {
            loop {
                let (mut socket, peer) = tokio::select! {
                    _ = token.cancelled() => return,
                    accepted = listener.accept() => match accepted {
                        Ok(accepted) => accepted,
                        Err(_) => return,
                    },
                };
                let handle = handle.clone();
                let address = address.clone();
                tokio::spawn(async move {
                    let channel = handle
                        .channel_open_forwarded_tcpip(
                            address,
                            bound,
                            peer.ip().to_string(),
                            peer.port() as u32,
                        )
                        .await;
                    if let Ok(channel) = channel {
                        let mut stream = channel.into_stream();
                        let _ = copy_bidirectional(&mut socket, &mut stream).await;
                    }
                });
            }
        });
        Ok(true)
    }

    async fn cancel_tcpip_forward(
        &mut self,
        _address: &str,
        _port: u32,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

/// A previous session's listener may not have released the port yet.
async fn bind_with_retry(address: &str, port: u16) -> Option<TcpListener> {
    for _ in 0..40 {
        match TcpListener::bind((address, port)).await {
            Ok(listener) => return Some(listener),
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Link breaker
// ---------------------------------------------------------------------------

/// A transparent TCP proxy whose connections can be severed on demand, which is
/// how the tests simulate a dropped SSH link without stopping the server.
pub struct Breaker {
    pub addr: SocketAddr,
    live: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl Breaker {
    pub async fn start(upstream: SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let live: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));

        let tasks = Arc::clone(&live);
        tokio::spawn(async move {
            while let Ok((mut downstream, _)) = listener.accept().await {
                let tasks_inner = Arc::clone(&tasks);
                let task = tokio::spawn(async move {
                    if let Ok(mut up) = TcpStream::connect(upstream).await {
                        let _ = copy_bidirectional(&mut downstream, &mut up).await;
                    }
                    // Drop finished tasks so the list cannot grow without bound.
                    tasks_inner
                        .lock()
                        .unwrap()
                        .retain(|task| !task.is_finished());
                });
                tasks.lock().unwrap().push(task);
            }
        });

        Self { addr, live }
    }

    /// Kill every connection currently going through the proxy.
    pub fn break_all(&self) {
        for task in self.live.lock().unwrap().drain(..) {
            task.abort();
        }
    }
}

// ---------------------------------------------------------------------------
// Polling helpers
// ---------------------------------------------------------------------------

/// Wait for `check` to pass, failing the test with `what` if it never does.
pub async fn wait_for<F, Fut>(what: &str, check: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        if check().await {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
