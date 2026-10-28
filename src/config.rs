//! Configuration model: TOML on disk, validated in memory.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// The whole config file.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub daemon: DaemonConfig,
    #[serde(default)]
    pub hosts: BTreeMap<String, HostConfig>,
    #[serde(default)]
    pub forwards: Vec<ForwardConfig>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    /// IPC socket path (unix) or pipe name (windows). Defaults per-OS.
    pub socket: Option<String>,
    /// Tracing filter, e.g. "info" or "ssh_tunnel=debug".
    pub log_level: Option<String>,
    /// Where to read OpenSSH client config from [default: ~/.ssh/config].
    pub ssh_config: Option<String>,
    /// Watch the config file and reload it on change.
    #[serde(default = "yes")]
    pub watch_config: bool,
    /// Remember runtime enable/disable decisions across daemon restarts.
    #[serde(default = "yes")]
    pub persist_state: bool,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            socket: None,
            log_level: None,
            ssh_config: None,
            watch_config: true,
            persist_state: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    /// The alias to connect to, exactly as you would type it after `ssh`. It is
    /// also the key looked up in ~/.ssh/config, so `HostName` there can point it
    /// at the real address.
    pub hostname: String,
    /// Unset means "whatever ~/.ssh/config says", falling back to 22.
    pub port: Option<u16>,
    /// Unset means "whatever ~/.ssh/config says". A host with no user from
    /// either source is invalid.
    pub user: Option<String>,
    /// Private key file(s). Accepts a single string or a list. Unset means
    /// "whatever ~/.ssh/config says".
    #[serde(default, deserialize_with = "de_paths")]
    pub identity: Vec<PathBuf>,
    /// Name of an environment variable holding the key passphrase. A background
    /// service cannot prompt, so this is the only way to use an encrypted key
    /// without an agent.
    pub identity_passphrase_env: Option<String>,
    #[serde(default = "yes")]
    pub use_agent: bool,
    /// Consult ~/.ssh/config for this host at all.
    #[serde(default = "yes")]
    pub use_ssh_config: bool,
    /// Defaults to ~/.ssh/known_hosts.
    pub known_hosts: Option<PathBuf>,
    #[serde(default)]
    pub strict_host_key_checking: StrictHostKeyChecking,
    #[serde(default = "default_connect_timeout", with = "humantime_serde")]
    pub connect_timeout: Duration,
    #[serde(default = "default_keepalive", with = "humantime_serde")]
    pub keepalive_interval: Duration,
    #[serde(default = "default_keepalive_max")]
    pub keepalive_max: usize,
    #[serde(default)]
    pub reconnect: ReconnectConfig,
    /// Stay connected even when no forward on this host is active.
    #[serde(default)]
    pub persistent: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconnectConfig {
    #[serde(default = "default_backoff_initial", with = "humantime_serde")]
    pub initial: Duration,
    #[serde(default = "default_backoff_max", with = "humantime_serde")]
    pub max: Duration,
    #[serde(default = "default_backoff_factor")]
    pub factor: f64,
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            initial: default_backoff_initial(),
            max: default_backoff_max(),
            factor: default_backoff_factor(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StrictHostKeyChecking {
    /// Unknown host keys are rejected.
    Yes,
    /// Unknown host keys are learned and written to known_hosts. Default.
    #[default]
    AcceptNew,
    /// Any host key is accepted and nothing is written. Insecure.
    No,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ForwardKind {
    /// -L: listen locally, connect from the server.
    #[default]
    Local,
    /// -R: listen on the server, connect from here.
    Remote,
    /// -D: local SOCKS5 proxy.
    Dynamic,
}

impl std::fmt::Display for ForwardKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ForwardKind::Local => "local",
            ForwardKind::Remote => "remote",
            ForwardKind::Dynamic => "dynamic",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForwardConfig {
    /// Unique key used by every runtime command.
    pub name: String,
    /// Key into `[hosts]`.
    pub host: String,
    #[serde(rename = "type", default)]
    pub kind: ForwardKind,
    /// "addr:port", "[::1]:port", or a bare port (implies 127.0.0.1).
    pub listen: String,
    /// "host:port". Required for local/remote, forbidden for dynamic.
    pub target: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
}

impl ForwardConfig {
    pub fn listen_addr(&self) -> Result<(String, u16)> {
        parse_host_port(&self.listen, Some("127.0.0.1"))
            .with_context(|| format!("forward `{}`: invalid `listen`", self.name))
    }

    pub fn target_addr(&self) -> Result<(String, u16)> {
        let target = self
            .target
            .as_deref()
            .with_context(|| format!("forward `{}`: `target` is required", self.name))?;
        parse_host_port(target, None)
            .with_context(|| format!("forward `{}`: invalid `target`", self.name))
    }
}

impl Config {
    /// Read, expand `~`, and validate a config file.
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let mut config: Config =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        config.expand_paths();
        config.validate()?;
        Ok(config)
    }

    fn expand_paths(&mut self) {
        for host in self.hosts.values_mut() {
            for identity in &mut host.identity {
                *identity = expand_tilde(identity);
            }
            if let Some(known_hosts) = &host.known_hosts {
                host.known_hosts = Some(expand_tilde(known_hosts));
            }
        }
    }

    pub fn validate(&self) -> Result<()> {
        let mut seen = HashSet::new();
        for forward in &self.forwards {
            if forward.name.is_empty() {
                bail!("a forward has an empty `name`");
            }
            if !seen.insert(forward.name.as_str()) {
                bail!("duplicate forward name `{}`", forward.name);
            }
            if !self.hosts.contains_key(&forward.host) {
                bail!(
                    "forward `{}` references unknown host `{}`",
                    forward.name,
                    forward.host
                );
            }
            forward.listen_addr()?;
            match forward.kind {
                ForwardKind::Local | ForwardKind::Remote => {
                    forward.target_addr()?;
                }
                ForwardKind::Dynamic => {
                    if forward.target.is_some() {
                        bail!(
                            "forward `{}` is dynamic and must not set `target`",
                            forward.name
                        );
                    }
                }
            }
        }
        for (name, host) in &self.hosts {
            if host.hostname.is_empty() {
                bail!("host `{name}` has an empty `hostname`");
            }
            if host.reconnect.factor < 1.0 {
                bail!("host `{name}`: reconnect.factor must be >= 1.0");
            }
        }
        Ok(())
    }

    pub fn forward(&self, name: &str) -> Option<&ForwardConfig> {
        self.forwards.iter().find(|f| f.name == name)
    }
}

pub fn current_username() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "root".into())
}

pub fn expand_tilde(path: &Path) -> PathBuf {
    PathBuf::from(shellexpand::tilde(&path.to_string_lossy()).into_owned())
}

/// Parse "host:port", "[v6]:port", or a bare port when `default_host` is given.
pub fn parse_host_port(input: &str, default_host: Option<&str>) -> Result<(String, u16)> {
    let input = input.trim();
    if input.is_empty() {
        bail!("empty address");
    }
    if let Some(rest) = input.strip_prefix('[') {
        let (host, rest) = rest
            .split_once(']')
            .context("unterminated `[` in IPv6 address")?;
        let port = rest
            .strip_prefix(':')
            .context("missing `:port` after IPv6 address")?;
        return Ok((host.to_string(), parse_port(port)?));
    }
    match input.rsplit_once(':') {
        Some((host, port)) => {
            if host.contains(':') {
                bail!("IPv6 addresses must be bracketed, e.g. [::1]:8080");
            }
            let host = if host.is_empty() {
                default_host.unwrap_or("0.0.0.0")
            } else {
                host
            };
            Ok((host.to_string(), parse_port(port)?))
        }
        None => {
            let host = default_host.context("expected `host:port`")?;
            Ok((host.to_string(), parse_port(input)?))
        }
    }
}

fn parse_port(text: &str) -> Result<u16> {
    text.parse::<u16>()
        .with_context(|| format!("invalid port `{text}`"))
}

fn de_paths<'de, D>(de: D) -> Result<Vec<PathBuf>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match Option::<OneOrMany>::deserialize(de)? {
        None => Vec::new(),
        Some(OneOrMany::One(one)) => vec![PathBuf::from(one)],
        Some(OneOrMany::Many(many)) => many.into_iter().map(PathBuf::from).collect(),
    })
}

fn yes() -> bool {
    true
}
fn default_keepalive_max() -> usize {
    3
}
fn default_connect_timeout() -> Duration {
    Duration::from_secs(10)
}
fn default_keepalive() -> Duration {
    Duration::from_secs(30)
}
fn default_backoff_initial() -> Duration {
    Duration::from_secs(1)
}
fn default_backoff_max() -> Duration {
    Duration::from_secs(60)
}
fn default_backoff_factor() -> f64 {
    2.0
}

// ---------------------------------------------------------------------------
// Resolution against ~/.ssh/config
// ---------------------------------------------------------------------------

/// Where a resolved value came from, so a management UI can explain itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueSource {
    /// Set explicitly in the agent's own config file.
    Config,
    /// Taken from ~/.ssh/config.
    SshConfig,
    /// Neither said anything; this is the built-in default.
    Default,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSources {
    pub address: ValueSource,
    pub port: ValueSource,
    pub user: ValueSource,
    pub identity: ValueSource,
}

/// A host with every value the supervisor needs, after consulting
/// ~/.ssh/config. Compared field by field on reload to decide what to restart.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedHost {
    /// The alias we matched on: the `hostname` field, as typed.
    pub alias: String,
    /// The address actually dialled, after `HostName`.
    pub address: String,
    pub port: u16,
    pub user: String,
    pub identity: Vec<PathBuf>,
    pub identity_passphrase_env: Option<String>,
    pub use_agent: bool,
    pub known_hosts: Option<PathBuf>,
    pub strict_host_key_checking: StrictHostKeyChecking,
    pub connect_timeout: Duration,
    pub keepalive_interval: Duration,
    pub keepalive_max: usize,
    pub reconnect: ReconnectConfig,
    pub persistent: bool,
    pub sources: HostSources,
}

/// A host either connects, or explains why it cannot.
#[derive(Debug, Clone, PartialEq)]
pub enum HostResolution {
    Valid(Box<ResolvedHost>),
    /// Every problem found, not just the first: a UI wants the whole list.
    Invalid {
        reasons: Vec<String>,
    },
}

impl HostResolution {
    pub fn valid(&self) -> Option<&ResolvedHost> {
        match self {
            HostResolution::Valid(host) => Some(host),
            HostResolution::Invalid { .. } => None,
        }
    }

    pub fn reasons(&self) -> &[String] {
        match self {
            HostResolution::Valid(_) => &[],
            HostResolution::Invalid { reasons } => reasons,
        }
    }
}

/// Fill in what the agent's config leaves out from ~/.ssh/config.
///
/// Precedence is always config file, then ~/.ssh/config, then a built-in
/// default. A host with no username from either source is invalid rather than
/// silently connecting as whoever happens to be running the daemon.
pub fn resolve_host(config: &HostConfig, ssh: &crate::ssh_config::SshConfigFile) -> HostResolution {
    let consulted = config.use_ssh_config;
    let found = if consulted {
        ssh.lookup(&config.hostname)
    } else {
        crate::ssh_config::SshConfigMatch::default()
    };
    let ssh_path = || match ssh.source() {
        Some(path) => path.display().to_string(),
        None => "~/.ssh/config".to_string(),
    };

    let mut reasons = Vec::new();

    let (address, address_source) = match &found.host_name {
        Some(host_name) => (host_name.clone(), ValueSource::SshConfig),
        None => (config.hostname.clone(), ValueSource::Default),
    };

    let (port, port_source) = match (config.port, found.port) {
        (Some(port), _) => (port, ValueSource::Config),
        (None, Some(port)) => (port, ValueSource::SshConfig),
        (None, None) => (22, ValueSource::Default),
    };

    let (user, user_source) = match (&config.user, &found.user) {
        (Some(user), _) => (user.clone(), ValueSource::Config),
        (None, Some(user)) => (user.clone(), ValueSource::SshConfig),
        (None, None) => {
            reasons.push(if consulted {
                format!(
                    "no username: `user` is not set, and {} has no User for `{}`",
                    ssh_path(),
                    config.hostname,
                )
            } else {
                "no username: `user` is not set and use_ssh_config is false".to_string()
            });
            (String::new(), ValueSource::Default)
        }
    };

    let (identity, identity_source) = if !config.identity.is_empty() {
        (config.identity.clone(), ValueSource::Config)
    } else if !found.identity.is_empty() {
        (
            found
                .identity
                .iter()
                .map(|path| expand_tilde(path))
                .collect(),
            ValueSource::SshConfig,
        )
    } else {
        (Vec::new(), ValueSource::Default)
    };

    if !config.use_agent && identity.is_empty() {
        reasons.push(
            "no authentication method: use_agent is false and no identity is set here or in \
             ~/.ssh/config"
                .to_string(),
        );
    }

    if let Some(hop) = &found.proxy_jump {
        reasons.push(format!(
            "ProxyJump `{hop}` is not supported; connecting direct would reach the wrong host",
        ));
    }

    // Only blame the unreadable file if something actually had to fall through
    // to it.
    if let Some(error) = ssh.error() {
        let needed = config.user.is_none() || config.port.is_none() || config.identity.is_empty();
        if consulted && needed && !reasons.is_empty() {
            reasons.push(format!("{}: {error}", ssh_path()));
        }
    }

    if !reasons.is_empty() {
        return HostResolution::Invalid { reasons };
    }

    HostResolution::Valid(Box::new(ResolvedHost {
        alias: config.hostname.clone(),
        address,
        port,
        user,
        identity,
        identity_passphrase_env: config.identity_passphrase_env.clone(),
        use_agent: config.use_agent,
        known_hosts: config.known_hosts.clone(),
        strict_host_key_checking: config.strict_host_key_checking,
        connect_timeout: config.connect_timeout,
        keepalive_interval: config.keepalive_interval,
        keepalive_max: config.keepalive_max,
        reconnect: config.reconnect,
        persistent: config.persistent,
        sources: HostSources {
            address: address_source,
            port: port_source,
            user: user_source,
            identity: identity_source,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config> {
        let mut config: Config = toml::from_str(text)?;
        config.expand_paths();
        config.validate()?;
        Ok(config)
    }

    const HOST: &str = r#"
[hosts.prod]
hostname = "bastion.example.com"
user = "me"
"#;

    #[test]
    fn parses_a_full_config() {
        let config = parse(&format!(
            r#"{HOST}
[[forwards]]
name = "db"
host = "prod"
type = "local"
listen = "127.0.0.1:5432"
target = "db.internal:5432"

[[forwards]]
name = "socks"
host = "prod"
type = "dynamic"
listen = "1080"
enabled = false
"#
        ))
        .unwrap();
        assert_eq!(config.forwards.len(), 2);
        assert_eq!(
            config.forwards[0].listen_addr().unwrap(),
            ("127.0.0.1".into(), 5432)
        );
        assert_eq!(
            config.forwards[1].listen_addr().unwrap(),
            ("127.0.0.1".into(), 1080)
        );
        assert!(!config.forwards[1].enabled);
        assert!(config.forwards[0].enabled, "forwards default to enabled");
        assert_eq!(
            config.hosts["prod"].port, None,
            "unset, so ssh_config may fill it"
        );
        assert_eq!(config.hosts["prod"].user.as_deref(), Some("me"));
    }

    #[test]
    fn rejects_duplicate_names() {
        let err = parse(&format!(
            r#"{HOST}
[[forwards]]
name = "db"
host = "prod"
listen = "5432"
target = "a:1"

[[forwards]]
name = "db"
host = "prod"
listen = "5433"
target = "a:1"
"#
        ))
        .unwrap_err();
        assert!(err.to_string().contains("duplicate forward name"), "{err}");
    }

    #[test]
    fn rejects_unknown_host_reference() {
        let err = parse(&format!(
            r#"{HOST}
[[forwards]]
name = "db"
host = "staging"
listen = "5432"
target = "a:1"
"#
        ))
        .unwrap_err();
        assert!(err.to_string().contains("unknown host"), "{err}");
    }

    #[test]
    fn rejects_local_forward_without_target() {
        let err = parse(&format!(
            r#"{HOST}
[[forwards]]
name = "db"
host = "prod"
listen = "5432"
"#
        ))
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("`target` is required"),
            "{err:#}"
        );
    }

    #[test]
    fn rejects_dynamic_forward_with_target() {
        let err = parse(&format!(
            r#"{HOST}
[[forwards]]
name = "socks"
host = "prod"
type = "dynamic"
listen = "1080"
target = "a:1"
"#
        ))
        .unwrap_err();
        assert!(err.to_string().contains("must not set `target`"), "{err}");
    }

    #[test]
    fn rejects_unknown_keys() {
        let err = parse(&format!("{HOST}typo = true\n")).unwrap_err();
        assert!(format!("{err:#}").contains("typo"), "{err:#}");
    }

    #[test]
    fn parses_durations_and_identity_forms() {
        let config = parse(
            r#"
[hosts.a]
hostname = "a"
identity = "~/.ssh/id_ed25519"
keepalive_interval = "45s"
reconnect = { initial = "500ms", max = "2m", factor = 1.5 }

[hosts.b]
hostname = "b"
identity = ["/k1", "/k2"]
"#,
        )
        .unwrap();
        assert_eq!(
            config.hosts["a"].keepalive_interval,
            Duration::from_secs(45)
        );
        assert_eq!(config.hosts["a"].reconnect.max, Duration::from_secs(120));
        assert_eq!(config.hosts["a"].identity.len(), 1);
        assert!(
            !config.hosts["a"].identity[0]
                .to_string_lossy()
                .starts_with('~')
        );
        assert_eq!(config.hosts["b"].identity.len(), 2);
    }

    // ---- resolution against ~/.ssh/config -------------------------------

    use crate::ssh_config::SshConfigFile;

    /// Build one host from a TOML body, as it would appear under `[hosts.h]`.
    fn host(body: &str) -> HostConfig {
        let config: Config = toml::from_str(&format!("[hosts.h]\n{body}")).unwrap();
        config.hosts["h"].clone()
    }

    fn ssh_config(contents: &str) -> (tempfile::TempDir, SshConfigFile) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(&path, contents).unwrap();
        let file = SshConfigFile::load(Some(&path));
        (dir, file)
    }

    const PROD: &str = "Host prod\n  HostName bastion.example.com\n  Port 2222\n  \
                        User deploy\n  IdentityFile /keys/prod\n";

    #[test]
    fn fills_missing_values_from_ssh_config() {
        let (_dir, ssh) = ssh_config(PROD);
        let resolved = resolve_host(&host(r#"hostname = "prod""#), &ssh);
        let resolved = resolved.valid().expect("should resolve");

        assert_eq!(resolved.alias, "prod");
        assert_eq!(resolved.address, "bastion.example.com");
        assert_eq!(resolved.port, 2222);
        assert_eq!(resolved.user, "deploy");
        assert_eq!(resolved.identity, vec![PathBuf::from("/keys/prod")]);
        assert_eq!(resolved.sources.address, ValueSource::SshConfig);
        assert_eq!(resolved.sources.port, ValueSource::SshConfig);
        assert_eq!(resolved.sources.user, ValueSource::SshConfig);
        assert_eq!(resolved.sources.identity, ValueSource::SshConfig);
    }

    #[test]
    fn our_config_wins_over_ssh_config() {
        let (_dir, ssh) = ssh_config(PROD);
        let resolved = resolve_host(
            &host(
                r#"hostname = "prod"
port = 2022
user = "gyusun"
identity = ["/keys/mine"]"#,
            ),
            &ssh,
        );
        let resolved = resolved.valid().expect("should resolve");

        assert_eq!(resolved.user, "gyusun");
        assert_eq!(resolved.port, 2022);
        assert_eq!(resolved.identity, vec![PathBuf::from("/keys/mine")]);
        assert_eq!(resolved.sources.user, ValueSource::Config);
        // HostName has no override in our config, so it still applies.
        assert_eq!(resolved.address, "bastion.example.com");
    }

    #[test]
    fn an_unmatched_alias_is_used_as_the_address() {
        let (_dir, ssh) = ssh_config(PROD);
        let resolved = resolve_host(&host("hostname = \"10.0.0.5\"\nuser = \"root\""), &ssh);
        let resolved = resolved.valid().expect("a literal address needs no match");

        assert_eq!(resolved.address, "10.0.0.5");
        assert_eq!(resolved.port, 22);
        assert_eq!(resolved.sources.address, ValueSource::Default);
    }

    #[test]
    fn a_host_with_no_username_anywhere_is_invalid() {
        let (_dir, ssh) = ssh_config("Host other\n  User someone\n");
        let resolution = resolve_host(&host(r#"hostname = "prod""#), &ssh);

        let reasons = resolution.reasons();
        assert_eq!(reasons.len(), 1, "{reasons:?}");
        assert!(reasons[0].contains("no username"), "{}", reasons[0]);
        assert!(
            reasons[0].contains("prod"),
            "names the alias: {}",
            reasons[0]
        );
        assert!(
            reasons[0].contains("config"),
            "names the file: {}",
            reasons[0]
        );
    }

    #[test]
    fn a_host_with_no_authentication_method_is_invalid() {
        let (_dir, ssh) = ssh_config("Host prod\n  User deploy\n");
        let resolution = resolve_host(&host("hostname = \"prod\"\nuse_agent = false"), &ssh);
        assert!(
            resolution.reasons()[0].contains("no authentication method"),
            "{:?}",
            resolution.reasons(),
        );
    }

    #[test]
    fn an_identity_from_ssh_config_satisfies_the_auth_check() {
        let (_dir, ssh) = ssh_config(PROD);
        let resolution = resolve_host(&host("hostname = \"prod\"\nuse_agent = false"), &ssh);
        assert!(resolution.valid().is_some(), "{:?}", resolution.reasons());
    }

    #[test]
    fn proxy_jump_is_refused_rather_than_silently_ignored() {
        let (_dir, ssh) = ssh_config("Host prod\n  User deploy\n  ProxyJump gateway\n");
        let resolution = resolve_host(&host(r#"hostname = "prod""#), &ssh);
        assert!(
            resolution.reasons()[0].contains("ProxyJump"),
            "{:?}",
            resolution.reasons(),
        );
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let (_dir, ssh) = ssh_config("Host prod\n  ProxyJump gateway\n");
        let resolution = resolve_host(&host("hostname = \"prod\"\nuse_agent = false"), &ssh);
        assert_eq!(resolution.reasons().len(), 3, "{:?}", resolution.reasons());
    }

    #[test]
    fn opting_out_ignores_ssh_config_entirely() {
        let (_dir, ssh) = ssh_config(PROD);
        let resolution = resolve_host(&host("hostname = \"prod\"\nuse_ssh_config = false"), &ssh);
        assert!(
            resolution.reasons()[0].contains("use_ssh_config is false"),
            "{:?}",
            resolution.reasons(),
        );

        let resolved = resolve_host(
            &host("hostname = \"prod\"\nuse_ssh_config = false\nuser = \"me\""),
            &ssh,
        );
        let resolved = resolved.valid().unwrap();
        assert_eq!(resolved.address, "prod", "HostName must not apply");
        assert_eq!(resolved.port, 22);
    }

    #[test]
    fn an_unreadable_ssh_config_is_reported_with_the_host_that_needed_it() {
        let (_dir, ssh) = ssh_config("  IdentityFile\nHost\n");
        let resolution = resolve_host(&host(r#"hostname = "prod""#), &ssh);
        let reasons = resolution.reasons();
        assert!(
            reasons.iter().any(|r| r.contains("no username")),
            "{reasons:?}"
        );
        assert!(
            reasons.iter().any(|r| r.contains("cannot parse")),
            "{reasons:?}"
        );
    }

    #[test]
    fn a_complete_host_never_consults_ssh_config() {
        let (_dir, ssh) = ssh_config("  IdentityFile\nHost\n");
        let resolved = resolve_host(
            &host(
                r#"hostname = "10.0.0.5"
port = 22
user = "root"
identity = ["/keys/mine"]"#,
            ),
            &ssh,
        );
        assert!(
            resolved.valid().is_some(),
            "a broken ssh config must not break a self-contained host: {:?}",
            resolved.reasons(),
        );
    }

    #[test]
    fn parses_address_forms() {
        assert_eq!(
            parse_host_port("1080", Some("127.0.0.1")).unwrap(),
            ("127.0.0.1".into(), 1080)
        );
        assert_eq!(
            parse_host_port("0.0.0.0:22", None).unwrap(),
            ("0.0.0.0".into(), 22)
        );
        assert_eq!(
            parse_host_port("[::1]:80", None).unwrap(),
            ("::1".into(), 80)
        );
        assert_eq!(
            parse_host_port(":80", Some("127.0.0.1")).unwrap(),
            ("127.0.0.1".into(), 80)
        );
        assert!(parse_host_port("::1:80", None).is_err());
        assert!(parse_host_port("host:99999", None).is_err());
        assert!(parse_host_port("1080", None).is_err());
    }
}
