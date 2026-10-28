//! `-D`: a local SOCKS5 proxy whose CONNECTs become direct-tcpip channels.
//!
//! Only CONNECT is implemented; BIND and UDP ASSOCIATE are out of scope and are
//! refused with the proper SOCKS reply code.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Result, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

use super::forward::{ForwardStats, pump};
use super::host::Session;

const VERSION: u8 = 0x05;
const AUTH_NONE: u8 = 0x00;
const AUTH_UNACCEPTABLE: u8 = 0xFF;
const CMD_CONNECT: u8 = 0x01;
const ATYP_IPV4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_IPV6: u8 = 0x04;

/// SOCKS5 reply codes (RFC 1928 §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Reply {
    Success = 0x00,
    GeneralFailure = 0x01,
    ConnectionRefused = 0x05,
    CommandNotSupported = 0x07,
    AddressNotSupported = 0x08,
}

/// A rejected request, carrying the code to send back before hanging up.
#[derive(Debug)]
pub struct Rejected {
    pub reply: Reply,
    pub reason: String,
}

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for Rejected {}

pub async fn handle_connection(
    mut local: TcpStream,
    peer: SocketAddr,
    session: Session,
    stats: Arc<ForwardStats>,
    token: CancellationToken,
) -> Result<()> {
    let _guard = stats.open();
    negotiate_auth(&mut local).await?;

    let target = match read_connect_request(&mut local).await {
        Ok(target) => target,
        Err(rejected) => {
            let _ = send_reply(&mut local, rejected.reply).await;
            bail!("socks request rejected: {rejected}");
        }
    };

    let remote = match super::local::open_channel(&session, &target, peer).await {
        Ok(remote) => remote,
        Err(err) => {
            let _ = send_reply(&mut local, Reply::ConnectionRefused).await;
            return Err(err);
        }
    };
    send_reply(&mut local, Reply::Success).await?;

    tokio::select! {
        _ = token.cancelled() => Ok(()),
        result = pump(local, remote, &stats) => Ok(result?),
    }
}

/// Method-selection handshake. Only "no authentication" is offered.
pub async fn negotiate_auth<S>(stream: &mut S) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut head = [0u8; 2];
    stream.read_exact(&mut head).await?;
    if head[0] != VERSION {
        bail!("unsupported SOCKS version {}", head[0]);
    }
    let mut methods = vec![0u8; head[1] as usize];
    stream.read_exact(&mut methods).await?;

    if !methods.contains(&AUTH_NONE) {
        stream.write_all(&[VERSION, AUTH_UNACCEPTABLE]).await?;
        bail!("the client offered no acceptable authentication method");
    }
    stream.write_all(&[VERSION, AUTH_NONE]).await?;
    Ok(())
}

/// Read one CONNECT request and return its target.
pub async fn read_connect_request<S>(stream: &mut S) -> Result<(String, u16), Rejected>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut head = [0u8; 4];
    read_exact(stream, &mut head).await?;
    let [version, command, _reserved, address_type] = head;

    if version != VERSION {
        return Err(Rejected {
            reply: Reply::GeneralFailure,
            reason: format!("unsupported SOCKS version {version}"),
        });
    }
    if command != CMD_CONNECT {
        return Err(Rejected {
            reply: Reply::CommandNotSupported,
            reason: format!("unsupported SOCKS command {command:#04x}; only CONNECT is supported"),
        });
    }

    let host = match address_type {
        ATYP_IPV4 => {
            let mut octets = [0u8; 4];
            read_exact(stream, &mut octets).await?;
            std::net::Ipv4Addr::from(octets).to_string()
        }
        ATYP_IPV6 => {
            let mut octets = [0u8; 16];
            read_exact(stream, &mut octets).await?;
            std::net::Ipv6Addr::from(octets).to_string()
        }
        ATYP_DOMAIN => {
            let mut length = [0u8; 1];
            read_exact(stream, &mut length).await?;
            let mut name = vec![0u8; length[0] as usize];
            read_exact(stream, &mut name).await?;
            String::from_utf8(name).map_err(|_| Rejected {
                reply: Reply::AddressNotSupported,
                reason: "the domain name is not valid UTF-8".into(),
            })?
        }
        other => {
            return Err(Rejected {
                reply: Reply::AddressNotSupported,
                reason: format!("unsupported address type {other:#04x}"),
            });
        }
    };

    let mut port = [0u8; 2];
    read_exact(stream, &mut port).await?;
    Ok((host, u16::from_be_bytes(port)))
}

/// Reply with a fixed bound address; clients that need the real one are out of
/// scope for a forwarding proxy.
pub async fn send_reply<S>(stream: &mut S, reply: Reply) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let frame = [VERSION, reply as u8, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0];
    stream.write_all(&frame).await?;
    stream.flush().await
}

async fn read_exact<S: AsyncRead + Unpin>(stream: &mut S, buf: &mut [u8]) -> Result<(), Rejected> {
    stream
        .read_exact(buf)
        .await
        .map(|_| ())
        .map_err(|err| Rejected {
            reply: Reply::GeneralFailure,
            reason: format!("truncated SOCKS request: {err}"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    #[tokio::test]
    async fn accepts_the_no_auth_method() {
        let (mut client, mut server) = duplex(64);
        let task = tokio::spawn(async move { negotiate_auth(&mut server).await });

        client.write_all(&[0x05, 0x02, 0x00, 0x02]).await.unwrap();
        let mut reply = [0u8; 2];
        client.read_exact(&mut reply).await.unwrap();

        assert_eq!(reply, [0x05, 0x00]);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn rejects_a_client_without_the_no_auth_method() {
        let (mut client, mut server) = duplex(64);
        let task = tokio::spawn(async move { negotiate_auth(&mut server).await });

        client.write_all(&[0x05, 0x01, 0x02]).await.unwrap();
        let mut reply = [0u8; 2];
        client.read_exact(&mut reply).await.unwrap();

        assert_eq!(reply, [0x05, 0xFF]);
        assert!(task.await.unwrap().is_err());
    }

    async fn request(bytes: &[u8]) -> Result<(String, u16), Rejected> {
        let (mut client, mut server) = duplex(256);
        let task = tokio::spawn(async move { read_connect_request(&mut server).await });
        client.write_all(bytes).await.unwrap();
        task.await.unwrap()
    }

    #[tokio::test]
    async fn reads_every_address_type() {
        let ipv4 = request(&[0x05, 0x01, 0x00, 0x01, 127, 0, 0, 1, 0x1F, 0x90])
            .await
            .unwrap();
        assert_eq!(ipv4, ("127.0.0.1".to_string(), 8080));

        let mut domain = vec![0x05, 0x01, 0x00, 0x03, 11];
        domain.extend_from_slice(b"example.com");
        domain.extend_from_slice(&443u16.to_be_bytes());
        assert_eq!(
            request(&domain).await.unwrap(),
            ("example.com".to_string(), 443)
        );

        let mut ipv6 = vec![0x05, 0x01, 0x00, 0x04];
        ipv6.extend_from_slice(&std::net::Ipv6Addr::LOCALHOST.octets());
        ipv6.extend_from_slice(&80u16.to_be_bytes());
        assert_eq!(request(&ipv6).await.unwrap(), ("::1".to_string(), 80));
    }

    #[tokio::test]
    async fn rejects_unsupported_commands_and_address_types() {
        // BIND
        let bind = request(&[0x05, 0x02, 0x00, 0x01, 127, 0, 0, 1, 0, 80])
            .await
            .unwrap_err();
        assert_eq!(bind.reply, Reply::CommandNotSupported);

        let atyp = request(&[0x05, 0x01, 0x00, 0x09, 0, 0]).await.unwrap_err();
        assert_eq!(atyp.reply, Reply::AddressNotSupported);

        let version = request(&[0x04, 0x01, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await
            .unwrap_err();
        assert_eq!(version.reply, Reply::GeneralFailure);
    }

    #[tokio::test]
    async fn encodes_replies() {
        let (mut client, mut server) = duplex(64);
        send_reply(&mut server, Reply::ConnectionRefused)
            .await
            .unwrap();
        let mut frame = [0u8; 10];
        client.read_exact(&mut frame).await.unwrap();
        assert_eq!(frame, [0x05, 0x05, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
    }
}
