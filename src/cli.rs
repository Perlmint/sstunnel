//! Command-line surface. Runtime options override the config file.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "ssh-tunnel",
    version,
    about = "Keeps SSH tunnels open in the background"
)]
pub struct Cli {
    /// Config file path [default: ~/.config/ssh-tunnel/config.toml]
    #[arg(long, short, global = true, env = "SSH_TUNNEL_CONFIG")]
    pub config: Option<PathBuf>,

    /// IPC socket path (unix) or pipe name (windows)
    #[arg(long, global = true, env = "SSH_TUNNEL_SOCKET")]
    pub socket: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the agent in the foreground (what the service manager invokes)
    Daemon(DaemonArgs),
    /// Show hosts and forwards
    Status {
        /// Emit JSON instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Turn forwards on
    Enable(NameArgs),
    /// Turn forwards off
    Disable(NameArgs),
    /// Flip forwards between on and off
    Toggle(NameArgs),
    /// Tear a forward down and bring it back up
    Restart(NameArgs),
    /// Re-read the config file now
    Reload,
    /// Stop the running daemon
    Shutdown,
    /// Validate the config file and exit
    Check,
    /// Register the daemon with the OS service manager
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

#[derive(Debug, Args)]
pub struct DaemonArgs {
    /// Tracing filter, e.g. "info" or "ssh_tunnel=debug"
    #[arg(long, env = "SSH_TUNNEL_LOG")]
    pub log_level: Option<String>,
    /// Also append logs to this file (service managers without stderr capture)
    #[arg(long)]
    pub log_file: Option<PathBuf>,
    /// Force these forwards on at startup, whatever the config says
    #[arg(long = "enable", value_name = "NAME")]
    pub enable: Vec<String>,
    /// Force these forwards off at startup, whatever the config says
    #[arg(long = "disable", value_name = "NAME")]
    pub disable: Vec<String>,
    /// Do not watch the config file for changes
    #[arg(long)]
    pub no_watch: bool,
    /// Do not read or write the persisted runtime state
    #[arg(long)]
    pub no_persist_state: bool,
}

#[derive(Debug, Args)]
pub struct NameArgs {
    /// Forward names
    #[arg(value_name = "NAME", required_unless_present = "all")]
    pub names: Vec<String>,
    /// Apply to every forward
    #[arg(long)]
    pub all: bool,
}

#[derive(Debug, Subcommand)]
pub enum ServiceAction {
    /// Install and start the per-user service
    Install,
    /// Stop and remove the service
    Uninstall,
    /// Start the installed service
    Start,
    /// Stop the installed service
    Stop,
    /// Report whether the service is installed and running
    Status,
}
