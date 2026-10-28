//! `-L`: accept locally, open a direct-tcpip channel to a target the SSH server
//! can reach.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

use super::forward::{ForwardStats, pump};
use super::host::Session;

pub async fn handle_connection(
    local: TcpStream,
    peer: SocketAddr,
    session: Session,
    target: (String, u16),
    stats: Arc<ForwardStats>,
    token: CancellationToken,
) -> Result<()> {
    let _guard = stats.open();
    let remote = open_channel(&session, &target, peer).await?;
    tokio::select! {
        _ = token.cancelled() => Ok(()),
        result = pump(local, remote, &stats) => Ok(result?),
    }
}

/// Open a direct-tcpip channel, reporting the target in any error.
pub async fn open_channel(
    session: &Session,
    target: &(String, u16),
    peer: SocketAddr,
) -> Result<russh::ChannelStream<russh::client::Msg>> {
    let channel = session
        .channel_open_direct_tcpip(
            target.0.clone(),
            u32::from(target.1),
            peer.ip().to_string(),
            u32::from(peer.port()),
        )
        .await
        .with_context(|| {
            format!(
                "the server refused a connection to {}:{}",
                target.0, target.1
            )
        })?;
    Ok(channel.into_stream())
}
