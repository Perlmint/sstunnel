//! Shared pieces of every forward: counters and the byte pump.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::ipc::protocol::Stats;

/// Live counters for one forward. Cheap enough to update per connection.
#[derive(Debug, Default)]
pub struct ForwardStats {
    active: AtomicU64,
    total: AtomicU64,
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
}

impl ForwardStats {
    pub fn snapshot(&self) -> Stats {
        Stats {
            active: self.active.load(Ordering::Relaxed),
            total: self.total.load(Ordering::Relaxed),
            bytes_in: self.bytes_in.load(Ordering::Relaxed),
            bytes_out: self.bytes_out.load(Ordering::Relaxed),
        }
    }

    /// Count a connection for as long as the returned guard lives.
    pub fn open(self: &Arc<Self>) -> ConnectionGuard {
        self.active.fetch_add(1, Ordering::Relaxed);
        self.total.fetch_add(1, Ordering::Relaxed);
        ConnectionGuard {
            stats: Arc::clone(self),
        }
    }
}

pub struct ConnectionGuard {
    stats: Arc<ForwardStats>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.stats.active.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Copy in both directions until either side closes.
///
/// `local` is always the OS socket and `remote` always the SSH channel, so the
/// byte counters mean the same thing for every kind of forward. Counting
/// happens as bytes move rather than at close, so `status` is useful while a
/// long-lived connection is still open.
pub async fn pump<L, R>(local: L, mut remote: R, stats: &Arc<ForwardStats>) -> std::io::Result<()>
where
    L: AsyncRead + AsyncWrite + Unpin,
    R: AsyncRead + AsyncWrite + Unpin,
{
    let mut local = Counted {
        inner: local,
        stats: Arc::clone(stats),
    };
    tokio::io::copy_bidirectional(&mut local, &mut remote).await?;
    Ok(())
}

/// Wraps the local side of a connection to tally bytes in each direction.
struct Counted<S> {
    inner: S,
    stats: Arc<ForwardStats>,
}

impl<S: AsyncRead + Unpin> AsyncRead for Counted<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let result = Pin::new(&mut this.inner).poll_read(cx, buf);
        if matches!(result, Poll::Ready(Ok(()))) {
            let read = buf.filled().len().saturating_sub(before);
            this.stats
                .bytes_out
                .fetch_add(read as u64, Ordering::Relaxed);
        }
        result
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Counted<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(written)) = &result {
            this.stats
                .bytes_in
                .fetch_add(*written as u64, Ordering::Relaxed);
        }
        result
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

// ---------------------------------------------------------------------------
// Task lifecycle
// ---------------------------------------------------------------------------

use anyhow::Result;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::host::HostHandle;
use crate::config::{ForwardConfig, ForwardKind};
use crate::ipc::protocol::ForwardState;

/// A running forward. Dropping the handle does not stop it; call `stop`.
pub struct ForwardHandle {
    /// The definition this task was started from, so a reload can tell whether
    /// the forward needs restarting.
    pub def: ForwardConfig,
    pub stats: Arc<ForwardStats>,
    state: watch::Receiver<ForwardState>,
    token: CancellationToken,
}

impl ForwardHandle {
    pub fn spawn(def: ForwardConfig, host: HostHandle, parent: &CancellationToken) -> Result<Self> {
        let token = parent.child_token();
        let stats = Arc::new(ForwardStats::default());
        let (state_tx, state_rx) = watch::channel(ForwardState::Starting);
        let listen = def.listen_addr()?;

        match def.kind {
            ForwardKind::Local => {
                let target = def.target_addr()?;
                let ctx = super::listen::Listen {
                    name: def.name.clone(),
                    listen,
                    host,
                    stats: Arc::clone(&stats),
                    state: state_tx,
                    token: token.clone(),
                };
                tokio::spawn(super::listen::run(
                    ctx,
                    move |local, peer, session, stats, token| {
                        super::local::handle_connection(
                            local,
                            peer,
                            session,
                            target.clone(),
                            stats,
                            token,
                        )
                    },
                ));
            }
            ForwardKind::Dynamic => {
                let ctx = super::listen::Listen {
                    name: def.name.clone(),
                    listen,
                    host,
                    stats: Arc::clone(&stats),
                    state: state_tx,
                    token: token.clone(),
                };
                tokio::spawn(super::listen::run(ctx, super::dynamic::handle_connection));
            }
            ForwardKind::Remote => {
                let ctx = super::remote::Remote {
                    name: def.name.clone(),
                    listen,
                    target: def.target_addr()?,
                    host,
                    stats: Arc::clone(&stats),
                    state: state_tx,
                    token: token.clone(),
                };
                tokio::spawn(super::remote::run(ctx));
            }
        }

        Ok(Self {
            def,
            stats,
            state: state_rx,
            token,
        })
    }

    pub fn state(&self) -> ForwardState {
        self.state.borrow().clone()
    }

    pub fn stop(&self) {
        self.token.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn counts_bytes_while_they_flow() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

        let stats = Arc::new(ForwardStats::default());
        let (mut client, local) = duplex(64);
        let (remote, mut peer) = duplex(64);

        let counting = {
            let stats = Arc::clone(&stats);
            tokio::spawn(async move { pump(local, remote, &stats).await })
        };

        client.write_all(b"ping").await.unwrap();
        let mut seen = [0u8; 4];
        peer.read_exact(&mut seen).await.unwrap();
        peer.write_all(b"pongpong").await.unwrap();
        let mut back = [0u8; 8];
        client.read_exact(&mut back).await.unwrap();

        // Both sides are still open: the counts must already be visible.
        assert_eq!(stats.snapshot().bytes_out, 4);
        assert_eq!(stats.snapshot().bytes_in, 8);

        drop(client);
        drop(peer);
        let _ = counting.await;
    }

    #[test]
    fn counts_connections_and_releases_them() {
        let stats = Arc::new(ForwardStats::default());
        let first = stats.open();
        let _second = stats.open();
        assert_eq!(stats.snapshot().active, 2);
        assert_eq!(stats.snapshot().total, 2);
        drop(first);
        assert_eq!(stats.snapshot().active, 1);
        assert_eq!(stats.snapshot().total, 2, "totals never go down");
    }
}
