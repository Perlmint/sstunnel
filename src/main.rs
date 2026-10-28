use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;

use ssh_tunnel::{cli, config, daemon, ipc, paths, service, ssh_config};

use cli::{Cli, Command, DaemonArgs, NameArgs};
use config::{Config, HostResolution, ValueSource};
use ipc::protocol::{ForwardReport, HostReport, Request, Response, Selection, StatusReport};
use ssh_config::SshConfigFile;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match dispatch(cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(cli: Cli) -> Result<ExitCode> {
    let config_path = match cli.config {
        Some(path) => path,
        None => paths::default_config_path()?,
    };

    match cli.command {
        Command::Check => check(&config_path),
        Command::Service { action } => {
            service::run(&action, &config_path)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Daemon(args) => run_daemon(config_path, cli.socket, args),
        #[cfg(target_os = "macos")]
        Command::Tray => {
            let endpoint = client_endpoint(cli.socket, &config_path)?;
            ssh_tunnel::tray::run(config_path, endpoint)?;
            Ok(ExitCode::SUCCESS)
        }
        command => {
            let endpoint = client_endpoint(cli.socket, &config_path)?;
            runtime()?.block_on(run_client(command, endpoint))
        }
    }
}

fn run_daemon(config_path: PathBuf, socket: Option<String>, args: DaemonArgs) -> Result<ExitCode> {
    let config = Config::load(&config_path)?;
    let _logging = init_logging(
        args.log_level
            .as_deref()
            .or(config.daemon.log_level.as_deref()),
        args.log_file.as_deref(),
    )?;
    let endpoint = resolve_endpoint(socket, Some(&config))?;
    runtime()?.block_on(daemon::run(config_path, endpoint, config, &args))?;
    Ok(ExitCode::SUCCESS)
}

async fn run_client(command: Command, endpoint: String) -> Result<ExitCode> {
    let json = matches!(command, Command::Status { json: true });
    let request = match command {
        Command::Status { .. } => Request::Status,
        Command::Enable(args) => Request::SetEnabled {
            names: selection(args),
            enabled: true,
        },
        Command::Disable(args) => Request::SetEnabled {
            names: selection(args),
            enabled: false,
        },
        Command::Toggle(args) => Request::Toggle {
            names: selection(args),
        },
        Command::Restart(args) => Request::Restart {
            names: selection(args),
        },
        Command::Reload => Request::Reload,
        Command::Shutdown => Request::Shutdown,
        // Handled before we get here.
        Command::Daemon(_) | Command::Check | Command::Service { .. } => unreachable!(),
        #[cfg(target_os = "macos")]
        Command::Tray => unreachable!(),
    };

    match ipc::client::request(&endpoint, &request).await? {
        Response::Ok { message } => {
            println!("{message}");
            Ok(ExitCode::SUCCESS)
        }
        Response::Status(report) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print_status(&report);
            }
            Ok(ExitCode::SUCCESS)
        }
        Response::Error { message } => {
            eprintln!("error: {message}");
            Ok(ExitCode::FAILURE)
        }
    }
}

/// Validate the config, and report every host that will not resolve.
fn check(config_path: &Path) -> Result<ExitCode> {
    let config = Config::load(config_path)?;
    let ssh = SshConfigFile::for_config(&config);
    println!(
        "{} parsed: {} host(s), {} forward(s)",
        config_path.display(),
        config.hosts.len(),
        config.forwards.len()
    );
    if let Some(path) = ssh.source() {
        match ssh.error() {
            Some(err) => println!("{}: {err}", path.display()),
            None => println!("consulted {}", path.display()),
        }
    }

    let mut problems = 0;
    println!("\nhosts");
    for (name, host) in &config.hosts {
        match config::resolve_host(host, &ssh) {
            HostResolution::Valid(resolved) => {
                let mut from_ssh = Vec::new();
                if resolved.sources.address == ValueSource::SshConfig {
                    from_ssh.push("hostname");
                }
                if resolved.sources.port == ValueSource::SshConfig {
                    from_ssh.push("port");
                }
                if resolved.sources.user == ValueSource::SshConfig {
                    from_ssh.push("user");
                }
                if resolved.sources.identity == ValueSource::SshConfig {
                    from_ssh.push("identity");
                }
                let note = if from_ssh.is_empty() {
                    String::new()
                } else {
                    format!("  ({} from ssh config)", from_ssh.join(", "))
                };
                println!(
                    "  {name}: {}@{}:{}{note}",
                    resolved.user, resolved.address, resolved.port
                );
            }
            HostResolution::Invalid { reasons } => {
                problems += 1;
                println!("  {name}: unusable");
                for reason in reasons {
                    println!("    - {reason}");
                }
            }
        }
    }

    if problems > 0 {
        eprintln!("\n{problems} host(s) cannot be used as configured");
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
}

fn selection(args: NameArgs) -> Selection {
    Selection::new(args.names, args.all)
}

/// CLI flag first, then the config file, then the per-OS default.
fn resolve_endpoint(socket: Option<String>, config: Option<&Config>) -> Result<String> {
    if let Some(socket) = socket {
        return Ok(socket);
    }
    if let Some(socket) = config.and_then(|config| config.daemon.socket.as_deref()) {
        return Ok(shellexpand::tilde(socket).into_owned());
    }
    paths::default_socket_name()
}

/// Client commands must still work when the config file is missing or broken,
/// so a failed load only costs us the `[daemon] socket` override.
fn client_endpoint(socket: Option<String>, config_path: &Path) -> Result<String> {
    if let Some(socket) = socket {
        return Ok(socket);
    }
    let config = Config::load(config_path).ok();
    resolve_endpoint(None, config.as_ref())
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the async runtime")
}

/// Returns a guard that must stay alive for file logging to keep flushing.
fn init_logging(
    level: Option<&str>,
    file: Option<&Path>,
) -> Result<Option<tracing_appender::non_blocking::WorkerGuard>> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_env("SSH_TUNNEL_LOG")
        .or_else(|_| EnvFilter::try_new(level.unwrap_or("info")))
        .context("invalid log filter")?;

    let (file_layer, guard) = match file {
        Some(path) => {
            paths::ensure_parent_dir(path)?;
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .with_context(|| format!("opening {}", path.display()))?;
            let (writer, guard) = tracing_appender::non_blocking(file);
            (
                Some(fmt::layer().with_ansi(false).with_writer(writer)),
                Some(guard),
            )
        }
        None => (None, None),
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(file_layer)
        .init();
    Ok(guard)
}

// ---------------------------------------------------------------------------
// Status rendering
// ---------------------------------------------------------------------------

fn print_status(report: &StatusReport) {
    println!(
        "config  {}  (loaded {} ago, up {})",
        report.config_path,
        ago(report.config_loaded_at),
        ago(report.started_at),
    );

    println!("\nhosts");
    let mut rows = vec![vec![
        "NAME".into(),
        "ADDRESS".into(),
        "USER".into(),
        "STATE".into(),
        "FORWARDS".into(),
    ]];
    for host in &report.hosts {
        rows.push(vec![
            host.name.clone(),
            host_address(host),
            host.user.clone().unwrap_or_else(|| "-".into()),
            host_state(&host.state),
            host.active_forwards.to_string(),
        ]);
    }
    print_table(&rows);

    println!("\nforwards");
    let mut rows = vec![vec![
        "NAME".into(),
        "KIND".into(),
        "LISTEN".into(),
        "TARGET".into(),
        "STATE".into(),
        "CONNS".into(),
        "IN".into(),
        "OUT".into(),
    ]];
    for forward in &report.forwards {
        rows.push(vec![
            forward.name.clone(),
            forward.kind.clone(),
            forward.listen.clone(),
            forward.target.clone().unwrap_or_else(|| "-".into()),
            forward_state(forward),
            format!("{}/{}", forward.stats.active, forward.stats.total),
            bytes(forward.stats.bytes_in),
            bytes(forward.stats.bytes_out),
        ]);
    }
    print_table(&rows);
}

/// Show what the alias resolved to, and the alias itself when it was rewritten.
fn host_address(host: &HostReport) -> String {
    match host.address.split(':').next() {
        Some(address) if address != host.alias => format!("{} -> {}", host.alias, host.address),
        _ => host.address.clone(),
    }
}

fn host_state(state: &ipc::protocol::HostState) -> String {
    use ipc::protocol::HostState::*;
    match state {
        Idle => "idle".into(),
        Connecting => "connecting".into(),
        Connected => "connected".into(),
        Reconnecting { attempts, error } if *attempts > 0 => {
            format!("reconnecting (try {attempts}: {})", first_line(error))
        }
        Reconnecting { .. } => "reconnecting".into(),
        Invalid { reasons } => format!("invalid ({})", first_line(&reasons.join("; "))),
    }
}

fn forward_state(forward: &ForwardReport) -> String {
    use ipc::protocol::ForwardState::*;
    match &forward.state {
        Disabled if forward.desired => "starting".into(),
        Disabled => "off".into(),
        Failed { error } => format!("failed ({})", first_line(error)),
        other => other.label().into(),
    }
}

/// Errors can be long; the table only has room for the gist of one.
fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or(text).trim();
    const LIMIT: usize = 40;
    match line.char_indices().nth(LIMIT) {
        Some((cut, _)) => format!("{}...", &line[..cut]),
        None => line.to_owned(),
    }
}

fn print_table(rows: &[Vec<String>]) {
    let columns = rows.first().map(Vec::len).unwrap_or(0);
    let widths: Vec<usize> = (0..columns)
        .map(|column| rows.iter().map(|row| row[column].len()).max().unwrap_or(0))
        .collect();

    for row in rows {
        let mut line = String::from("  ");
        for (index, cell) in row.iter().enumerate() {
            if index + 1 == columns {
                line.push_str(cell);
            } else {
                line.push_str(&format!("{cell:<width$}  ", width = widths[index]));
            }
        }
        println!("{}", line.trim_end());
    }
}

fn bytes(count: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut value = count as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{count} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn ago(unix_secs: u64) -> String {
    let now = ipc::protocol::unix_secs(std::time::SystemTime::now());
    let seconds = now.saturating_sub(unix_secs);
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m", seconds / 60),
        3600..=86399 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86400),
    }
}
