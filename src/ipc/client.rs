//! CLI side of the control channel: connect, ask one question, disconnect.

use anyhow::{Context, Result, bail};
use interprocess::local_socket::tokio::Stream;
use interprocess::local_socket::traits::tokio::Stream as _;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::protocol::{Request, Response};

pub async fn request(endpoint: &str, request: &Request) -> Result<Response> {
    let name = super::to_name(endpoint)?;
    let stream = Stream::connect(name)
        .await
        .with_context(|| super::not_running_hint(endpoint))?;

    let mut body = serde_json::to_vec(request)?;
    body.push(b'\n');
    let mut writer = &stream;
    writer.write_all(&body).await?;
    writer.flush().await?;

    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        bail!("the daemon closed the connection without replying");
    }
    Ok(serde_json::from_str(line.trim_end())?)
}

/// True when something is accepting connections on `endpoint`.
pub async fn probe(endpoint: &str) -> bool {
    let Ok(name) = super::to_name(endpoint) else {
        return false;
    };
    Stream::connect(name).await.is_ok()
}
