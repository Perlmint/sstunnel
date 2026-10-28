//! The newline-delimited JSON protocol spoken over the local socket.

use serde::{Deserialize, Serialize};

/// Which forwards a command applies to.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Selection {
    All,
    Names(Vec<String>),
}

impl Selection {
    pub fn new(names: Vec<String>, all: bool) -> Self {
        if all {
            Selection::All
        } else {
            Selection::Names(names)
        }
    }

    /// Resolve against every known forward name, reporting names that do not exist.
    pub fn resolve<'a>(&self, known: impl Iterator<Item = &'a str>) -> Result<Vec<String>, String> {
        match self {
            Selection::All => Ok(known.map(str::to_owned).collect()),
            Selection::Names(names) => {
                let known: Vec<&str> = known.collect();
                let unknown: Vec<&str> = names
                    .iter()
                    .map(String::as_str)
                    .filter(|n| !known.contains(n))
                    .collect();
                if !unknown.is_empty() {
                    return Err(format!("no such forward: {}", unknown.join(", ")));
                }
                Ok(names.clone())
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// Liveness probe.
    Ping,
    Status,
    SetEnabled {
        names: Selection,
        enabled: bool,
    },
    Toggle {
        names: Selection,
    },
    Restart {
        names: Selection,
    },
    /// Re-read the config file now.
    Reload,
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    Ok { message: String },
    Status(Box<StatusReport>),
    Error { message: String },
}

impl Response {
    pub fn ok(message: impl Into<String>) -> Self {
        Response::Ok {
            message: message.into(),
        }
    }

    pub fn error(message: impl std::fmt::Display) -> Self {
        Response::Error {
            message: message.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusReport {
    pub config_path: String,
    /// Unix seconds.
    pub config_loaded_at: u64,
    pub started_at: u64,
    pub hosts: Vec<HostReport>,
    pub forwards: Vec<ForwardReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostReport {
    pub name: String,
    /// The alias as written in `hostname`, which is also the ~/.ssh/config key.
    pub alias: String,
    /// The resolved `address:port`, or just the alias when it did not resolve.
    pub address: String,
    /// `None` when no username could be resolved.
    pub user: Option<String>,
    /// Where each resolved value came from. `None` for an invalid host.
    pub sources: Option<crate::config::HostSources>,
    pub state: HostState,
    pub active_forwards: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum HostState {
    /// No forward needs this host right now.
    Idle,
    Connecting,
    Connected,
    Reconnecting {
        attempts: u32,
        error: String,
    },
    /// The host cannot be used as configured; every reason found.
    Invalid {
        reasons: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForwardReport {
    pub name: String,
    pub host: String,
    pub kind: String,
    pub listen: String,
    pub target: Option<String>,
    /// What the config plus runtime overrides say it should be.
    pub desired: bool,
    pub state: ForwardState,
    pub stats: Stats,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ForwardState {
    Disabled,
    Starting,
    /// Local/dynamic: bound and accepting. Remote: the server is listening.
    Listening {
        addr: String,
    },
    /// Bound locally, but the SSH session is down.
    WaitingForHost,
    Failed {
        error: String,
    },
}

impl ForwardState {
    pub fn label(&self) -> &'static str {
        match self {
            ForwardState::Disabled => "off",
            ForwardState::Starting => "starting",
            ForwardState::Listening { .. } => "up",
            ForwardState::WaitingForHost => "waiting",
            ForwardState::Failed { .. } => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Stats {
    pub active: u64,
    pub total: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

pub fn unix_secs(time: std::time::SystemTime) -> u64 {
    time.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
