//! Control channel: a named pipe on Windows, a unix domain socket elsewhere.

pub mod client;
pub mod protocol;
pub mod server;

use anyhow::{Context, Result};
use interprocess::local_socket::Name;

/// Turn a configured endpoint string into an `interprocess` name.
pub fn to_name(endpoint: &str) -> Result<Name<'static>> {
    #[cfg(windows)]
    {
        use interprocess::local_socket::{GenericNamespaced, ToNsName};
        endpoint
            .to_owned()
            .to_ns_name::<GenericNamespaced>()
            .with_context(|| format!("invalid pipe name `{endpoint}`"))
    }
    #[cfg(unix)]
    {
        use interprocess::local_socket::{GenericFilePath, ToFsName};
        std::path::PathBuf::from(endpoint)
            .to_fs_name::<GenericFilePath>()
            .with_context(|| format!("invalid socket path `{endpoint}`"))
    }
}

/// A human-readable hint for "the daemon does not seem to be running".
pub fn not_running_hint(endpoint: &str) -> String {
    format!(
        "no daemon is listening on {endpoint}\n\
         start one with `ssh-tunnel daemon`, or `ssh-tunnel service install`"
    )
}
