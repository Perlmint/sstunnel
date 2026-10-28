//! The daemon: one event loop owning all desired-versus-running reconciliation.

pub mod dynamic;
pub mod forward;
pub mod host;
pub mod listen;
pub mod local;
pub mod remote;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::SystemTime;

use anyhow::{Context, Result};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::cli::DaemonArgs;
use crate::config::{Config, HostResolution, ResolvedHost};
use crate::ipc::protocol::{
    ForwardReport, ForwardState, HostReport, HostState, Request, Response, Selection, StatusReport,
    unix_secs,
};
use crate::ipc::server::ControlSocket;
use crate::ssh_config::SshConfigFile;
use crate::state::Overrides;
use forward::ForwardHandle;
use host::HostHandle;

/// Everything that can drive the daemon's loop.
pub enum Command {
    Ipc {
        request: Request,
        reply: oneshot::Sender<Response>,
    },
    /// The config file changed on disk.
    ConfigChanged,
    /// A signal, or an IPC shutdown request.
    Shutdown,
}

pub struct Daemon {
    config_path: PathBuf,
    config: Config,
    config_loaded_at: SystemTime,
    started_at: SystemTime,
    overrides: Overrides,
    /// Parsed ~/.ssh/config, re-read on reload.
    ssh_config: SshConfigFile,
    /// Every configured host, resolved against ~/.ssh/config.
    resolved: BTreeMap<String, HostResolution>,
    hosts: HashMap<String, HostHandle>,
    forwards: HashMap<String, ForwardHandle>,
    /// Forwards that could not start, and why. Rebuilt on every `apply`.
    blocked: HashMap<String, String>,
    token: CancellationToken,
    endpoint: String,
}

/// Run the agent in the foreground until told to stop.
pub async fn run(
    config_path: PathBuf,
    endpoint: String,
    config: Config,
    args: &DaemonArgs,
) -> Result<()> {
    // Bind the control socket first: a second daemon should fail immediately
    // rather than after opening tunnels.
    let control = ControlSocket::bind(&endpoint).await?;
    info!(
        config = %config_path.display(),
        control = control.endpoint(),
        "ssh-tunnel starting",
    );

    let state_path = if args.no_persist_state || !config.daemon.persist_state {
        None
    } else {
        Some(crate::paths::default_state_path()?)
    };
    let mut overrides = Overrides::load(state_path);
    for name in &args.disable {
        overrides.set(name, false);
    }
    for name in &args.enable {
        overrides.set(name, true);
    }

    let token = CancellationToken::new();
    let (commands, mut inbox) = mpsc::channel(64);

    tokio::spawn(control.serve(commands.clone(), token.child_token()));

    let watching = !args.no_watch && config.daemon.watch_config;
    let _watcher = if watching {
        match crate::watcher::watch(&config_path, commands.clone()) {
            Ok(watcher) => Some(watcher),
            Err(err) => {
                warn!("config watching is disabled: {err:#}");
                None
            }
        }
    } else {
        None
    };

    tokio::spawn(forward_signals(commands.clone()));

    let ssh_config = SshConfigFile::for_config(&config);

    let mut daemon = Daemon {
        config_path,
        config,
        config_loaded_at: SystemTime::now(),
        started_at: SystemTime::now(),
        overrides,
        ssh_config,
        resolved: BTreeMap::new(),
        hosts: HashMap::new(),
        forwards: HashMap::new(),
        blocked: HashMap::new(),
        token: token.clone(),
        endpoint,
    };
    daemon.apply();

    while let Some(command) = inbox.recv().await {
        match command {
            Command::Ipc { request, reply } => {
                let shutting_down = matches!(request, Request::Shutdown);
                let _ = reply.send(daemon.handle(request));
                if shutting_down {
                    break;
                }
            }
            Command::ConfigChanged => daemon.reload_from_disk(),
            Command::Shutdown => break,
        }
    }

    info!("shutting down");
    daemon.overrides.save_best_effort();
    token.cancel();
    // Give listeners and channels a moment to close cleanly.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    Ok(())
}

impl Daemon {
    fn handle(&mut self, request: Request) -> Response {
        match request {
            Request::Ping => Response::ok("pong"),
            Request::Status => Response::Status(Box::new(self.status())),
            Request::SetEnabled { names, enabled } => self.set_enabled(names, enabled),
            Request::Toggle { names } => self.toggle(names),
            Request::Restart { names } => self.restart(names),
            Request::Reload => match self.reload() {
                Ok(message) => Response::ok(message),
                Err(err) => Response::error(format!("{err:#}")),
            },
            Request::Shutdown => Response::ok("stopping"),
        }
    }

    /// What a forward should be doing: the runtime override wins over the file.
    fn desired(&self, name: &str, configured: bool) -> bool {
        self.overrides.get(name).unwrap_or(configured)
    }

    fn set_enabled(&mut self, names: Selection, enabled: bool) -> Response {
        let selected = match names.resolve(self.config.forwards.iter().map(|f| f.name.as_str())) {
            Ok(selected) => selected,
            Err(err) => return Response::error(err),
        };
        for name in &selected {
            self.overrides.set(name, enabled);
        }
        self.overrides.save_best_effort();
        self.apply();
        Response::ok(format!(
            "{} {} forward(s)",
            if enabled { "enabled" } else { "disabled" },
            selected.len()
        ))
    }

    fn toggle(&mut self, names: Selection) -> Response {
        let selected = match names.resolve(self.config.forwards.iter().map(|f| f.name.as_str())) {
            Ok(selected) => selected,
            Err(err) => return Response::error(err),
        };
        let mut changes = Vec::new();
        for name in &selected {
            let configured = self.config.forward(name).is_some_and(|f| f.enabled);
            let next = !self.desired(name, configured);
            self.overrides.set(name, next);
            changes.push(format!("{name}={}", if next { "on" } else { "off" }));
        }
        self.overrides.save_best_effort();
        self.apply();
        Response::ok(changes.join(" "))
    }

    fn restart(&mut self, names: Selection) -> Response {
        let selected = match names.resolve(self.config.forwards.iter().map(|f| f.name.as_str())) {
            Ok(selected) => selected,
            Err(err) => return Response::error(err),
        };
        for name in &selected {
            self.stop_forward(name);
        }
        // apply() brings back the ones that are still meant to be running.
        self.apply();
        Response::ok(format!("restarted {} forward(s)", selected.len()))
    }

    fn reload(&mut self) -> Result<String> {
        let config = Config::load(&self.config_path)
            .with_context(|| format!("reloading {}", self.config_path.display()))?;
        let unchanged = config == self.config;
        self.config = config;
        // Picked up here rather than by the watcher: ~/.ssh also holds
        // known_hosts, which we write to ourselves under accept-new, so
        // watching that directory would feed the daemon its own reloads.
        self.ssh_config = SshConfigFile::for_config(&self.config);
        self.config_loaded_at = SystemTime::now();
        self.overrides
            .retain_known(self.config.forwards.iter().map(|f| f.name.as_str()));
        self.overrides.save_best_effort();
        self.apply();
        Ok(if unchanged {
            "config re-read, nothing changed".into()
        } else {
            format!("config reloaded: {} forward(s)", self.config.forwards.len())
        })
    }

    /// Reload triggered by the file watcher. A broken config must never tear
    /// down working tunnels, so the previous one stays in force.
    fn reload_from_disk(&mut self) {
        match self.reload() {
            Ok(message) => info!("{message}"),
            Err(err) => error!("keeping the previous config: {err:#}"),
        }
    }

    /// Re-resolve every host against ~/.ssh/config, logging what changed.
    fn resolve_hosts(&mut self) {
        let resolved: BTreeMap<String, HostResolution> = self
            .config
            .hosts
            .iter()
            .map(|(name, host)| {
                (
                    name.clone(),
                    crate::config::resolve_host(host, &self.ssh_config),
                )
            })
            .collect();

        // Only report differences, since `apply` runs on every command.
        for (name, resolution) in &resolved {
            if self.resolved.get(name) == Some(resolution) {
                continue;
            }
            match resolution {
                HostResolution::Invalid { reasons } => {
                    warn!(host = %name, "unusable host: {}", reasons.join("; "))
                }
                HostResolution::Valid(host) => debug!(
                    host = %name,
                    address = %host.address,
                    port = host.port,
                    user = %host.user,
                    "resolved",
                ),
            }
        }
        self.resolved = resolved;
    }

    /// Reconcile running tasks with the desired state. The only place tunnels
    /// start or stop.
    fn apply(&mut self) {
        self.resolve_hosts();

        // 1. Hosts whose resolved settings changed: everything on them must
        //    restart. Comparing the resolved form means a ~/.ssh/config edit
        //    counts, not just an edit to our own file.
        let dirty: Vec<String> = self
            .hosts
            .iter()
            .filter(|(name, handle)| {
                self.resolved.get(*name).and_then(HostResolution::valid) != Some(&handle.host)
            })
            .map(|(name, _)| name.clone())
            .collect();

        // 2. Stop forwards that are gone, changed, no longer wanted, or on a
        //    host that is being rebuilt.
        let stop: Vec<String> = self
            .forwards
            .iter()
            .filter(|(name, handle)| match self.config.forward(name) {
                None => true,
                Some(def) => {
                    *def != handle.def
                        || !self.desired(&def.name, def.enabled)
                        || dirty.contains(&def.host)
                }
            })
            .map(|(name, _)| name.clone())
            .collect();
        for name in stop {
            self.stop_forward(&name);
        }

        // 3. Drop the hosts nothing is using any more.
        for name in dirty {
            if let Some(handle) = self.hosts.remove(&name) {
                debug!(host = %name, "host definition changed, reconnecting");
                handle.shutdown();
            }
        }

        // 4. Start what should be running.
        self.blocked.clear();
        let start: Vec<_> = self
            .config
            .forwards
            .iter()
            .filter(|def| {
                self.desired(&def.name, def.enabled) && !self.forwards.contains_key(&def.name)
            })
            .cloned()
            .collect();
        for def in start {
            if let Err(err) = self.start_forward(&def) {
                let reason = format!("{err:#}");
                error!(forward = %def.name, "{reason}");
                self.blocked.insert(def.name.clone(), reason);
            }
        }

        self.reap_idle_hosts();
    }

    fn start_forward(&mut self, def: &crate::config::ForwardConfig) -> Result<()> {
        let host = self.ensure_host(&def.host)?;
        let handle = ForwardHandle::spawn(def.clone(), host, &self.token)
            .with_context(|| format!("starting forward `{}`", def.name))?;
        info!(forward = %def.name, kind = %def.kind, host = %def.host, "starting");
        self.forwards.insert(def.name.clone(), handle);
        Ok(())
    }

    fn stop_forward(&mut self, name: &str) {
        if let Some(handle) = self.forwards.remove(name) {
            info!(forward = %name, "stopping");
            handle.stop();
        }
    }

    fn ensure_host(&mut self, name: &str) -> Result<HostHandle> {
        if let Some(handle) = self.hosts.get(name) {
            return Ok(handle.clone());
        }
        let host = match self
            .resolved
            .get(name)
            .with_context(|| format!("unknown host `{name}`"))?
        {
            HostResolution::Valid(host) => (**host).clone(),
            HostResolution::Invalid { reasons } => {
                anyhow::bail!("host `{name}` is unusable: {}", reasons.join("; "))
            }
        };
        let handle = HostHandle::spawn(name.to_owned(), host, &self.token);
        self.hosts.insert(name.to_owned(), handle.clone());
        Ok(handle)
    }

    /// Disconnect hosts nothing forwards to, unless they are marked persistent.
    fn reap_idle_hosts(&mut self) {
        let in_use: Vec<String> = self.forwards.values().map(|f| f.def.host.clone()).collect();
        let idle: Vec<String> = self
            .hosts
            .keys()
            .filter(|name| {
                !in_use.contains(name)
                    && !self
                        .config
                        .hosts
                        .get(*name)
                        .is_some_and(|host| host.persistent)
            })
            .cloned()
            .collect();
        for name in idle {
            if let Some(handle) = self.hosts.remove(&name) {
                debug!(host = %name, "no forwards left, disconnecting");
                handle.shutdown();
            }
        }

        // Persistent hosts stay connected even with nothing forwarding.
        let persistent: Vec<String> = self
            .config
            .hosts
            .iter()
            .filter(|(_, host)| host.persistent)
            .map(|(name, _)| name.clone())
            .collect();
        for name in persistent {
            if !self.hosts.contains_key(&name) {
                let _ = self.ensure_host(&name);
            }
        }
    }

    fn status(&self) -> StatusReport {
        let mut per_host: BTreeMap<&str, usize> = BTreeMap::new();
        for forward in self.forwards.values() {
            *per_host.entry(forward.def.host.as_str()).or_default() += 1;
        }

        let hosts = self
            .config
            .hosts
            .iter()
            .map(|(name, config)| {
                let resolution = self.resolved.get(name);
                let resolved: Option<&ResolvedHost> = resolution.and_then(HostResolution::valid);
                HostReport {
                    name: name.clone(),
                    alias: config.hostname.clone(),
                    address: match resolved {
                        Some(host) => format!("{}:{}", host.address, host.port),
                        None => config.hostname.clone(),
                    },
                    user: resolved.map(|host| host.user.clone()),
                    sources: resolved.map(|host| host.sources),
                    state: match resolution {
                        Some(HostResolution::Invalid { reasons }) => HostState::Invalid {
                            reasons: reasons.clone(),
                        },
                        _ => self
                            .hosts
                            .get(name)
                            .map(HostHandle::state)
                            .unwrap_or(HostState::Idle),
                    },
                    active_forwards: per_host.get(name.as_str()).copied().unwrap_or(0),
                }
            })
            .collect();

        let forwards = self
            .config
            .forwards
            .iter()
            .map(|def| {
                let running = self.forwards.get(&def.name);
                ForwardReport {
                    name: def.name.clone(),
                    host: def.host.clone(),
                    kind: def.kind.to_string(),
                    listen: def.listen.clone(),
                    target: def.target.clone(),
                    desired: self.desired(&def.name, def.enabled),
                    state: match (running, self.blocked.get(&def.name)) {
                        (Some(handle), _) => handle.state(),
                        // Desired, but its host would not resolve.
                        (None, Some(reason)) => ForwardState::Failed {
                            error: reason.clone(),
                        },
                        (None, None) => ForwardState::Disabled,
                    },
                    stats: running.map(|f| f.stats.snapshot()).unwrap_or_default(),
                }
            })
            .collect();

        StatusReport {
            config_path: self.config_path.display().to_string(),
            config_loaded_at: unix_secs(self.config_loaded_at),
            started_at: unix_secs(self.started_at),
            hosts,
            forwards,
        }
    }
}

impl Daemon {
    /// Only used by the shutdown path in tests and diagnostics.
    #[allow(dead_code)]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

/// Turn OS signals into a shutdown command.
async fn forward_signals(commands: mpsc::Sender<Command>) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(term) => term,
            Err(err) => {
                warn!(%err, "cannot listen for SIGTERM");
                return;
            }
        };
        tokio::select! {
            _ = term.recv() => debug!("received SIGTERM"),
            result = tokio::signal::ctrl_c() => {
                if result.is_err() { return }
                debug!("received SIGINT");
            }
        }
    }
    #[cfg(not(unix))]
    {
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }
        debug!("received Ctrl-C");
    }
    let _ = commands.send(Command::Shutdown).await;
}
