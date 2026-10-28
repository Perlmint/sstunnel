//! Reading `~/.ssh/config`, so a host entry does not have to repeat what
//! OpenSSH already knows.
//!
//! Nothing in here is ever fatal: a missing or unreadable file simply resolves
//! nothing, and the reason travels with the lookup so the host that needed it
//! can explain itself.

use std::io::BufReader;
use std::path::{Path, PathBuf};

use ssh2_config::{ParseRule, SshConfig};
use tracing::{debug, warn};

/// Real configs are full of directives this parser does not implement
/// (`ControlMaster`, `ControlPath`, `ServerAliveCountMax`, …). Refusing to read
/// the file over one of those would be useless, so accept and ignore them.
const PARSE_RULES: ParseRule =
    ParseRule::ALLOW_UNKNOWN_FIELDS.union(ParseRule::ALLOW_UNSUPPORTED_FIELDS);

/// What `~/.ssh/config` says about one alias.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SshConfigMatch {
    pub host_name: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub identity: Vec<PathBuf>,
    /// We cannot honour this, so its presence makes a host invalid.
    pub proxy_jump: Option<String>,
}

/// A parsed `~/.ssh/config`, or the reason there is nothing to consult.
#[derive(Debug, Default)]
pub struct SshConfigFile {
    parsed: Option<SshConfig>,
    source: Option<PathBuf>,
    /// Set when the file exists but could not be read or parsed.
    error: Option<String>,
}

impl SshConfigFile {
    /// Load `path`, or `~/.ssh/config` when none is given.
    pub fn load(path: Option<&Path>) -> Self {
        let Some(path) = path.map(Path::to_path_buf).or_else(default_path) else {
            return Self::default();
        };

        if !path.exists() {
            debug!(path = %path.display(), "no ssh config to read");
            return Self {
                source: Some(path),
                ..Self::default()
            };
        }

        match parse(&path) {
            Ok(parsed) => {
                debug!(path = %path.display(), "read ssh config");
                Self {
                    parsed: Some(parsed),
                    source: Some(path),
                    error: None,
                }
            }
            Err(err) => {
                // Logged once here; hosts that needed it also report it.
                warn!(path = %path.display(), "{err}");
                Self {
                    parsed: None,
                    source: Some(path),
                    error: Some(err),
                }
            }
        }
    }

    /// Load whatever the agent's `[daemon] ssh_config` setting points at,
    /// falling back to ~/.ssh/config.
    pub fn for_config(config: &crate::config::Config) -> Self {
        match &config.daemon.ssh_config {
            Some(path) => {
                let path = shellexpand::tilde(path).into_owned();
                Self::load(Some(Path::new(&path)))
            }
            None => Self::load(None),
        }
    }

    /// An empty lookup table, for hosts that opt out.
    pub fn disabled() -> Self {
        Self::default()
    }

    /// The file consulted, for error messages.
    pub fn source(&self) -> Option<&Path> {
        self.source.as_deref()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Resolve `alias` the way `ssh <alias>` would: the first `Host` block to
    /// set a directive wins, and `IdentityFile` accumulates across blocks.
    pub fn lookup(&self, alias: &str) -> SshConfigMatch {
        let Some(parsed) = &self.parsed else {
            return SshConfigMatch::default();
        };
        let params = parsed.query(alias);
        SshConfigMatch {
            host_name: params.host_name,
            port: params.port,
            user: params.user,
            identity: params.identity_file.unwrap_or_default(),
            // Only the first hop matters for the "we cannot do this" message.
            proxy_jump: params.proxy_jump.and_then(|hops| hops.first().cloned()),
        }
    }
}

fn parse(path: &Path) -> Result<SshConfig, String> {
    let file = std::fs::File::open(path).map_err(|err| format!("cannot read: {err}"))?;
    SshConfig::default()
        .parse(&mut BufReader::new(file), PARSE_RULES)
        .map_err(|err| format!("cannot parse: {err}"))
}

fn default_path() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|dirs| dirs.home_dir().join(".ssh").join("config"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(contents: &str) -> (tempfile::TempDir, SshConfigFile) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(&path, contents).unwrap();
        let file = SshConfigFile::load(Some(&path));
        (dir, file)
    }

    #[test]
    fn reads_the_directives_we_care_about() {
        let (_dir, ssh) = load(
            "Host prod\n  HostName bastion.example.com\n  Port 2222\n  User deploy\n  IdentityFile ~/.ssh/id_prod\n",
        );
        let found = ssh.lookup("prod");
        assert_eq!(found.host_name.as_deref(), Some("bastion.example.com"));
        assert_eq!(found.port, Some(2222));
        assert_eq!(found.user.as_deref(), Some("deploy"));
        assert_eq!(found.identity.len(), 1);
        assert!(
            !found.identity[0].to_string_lossy().starts_with('~'),
            "the parser expands ~ in IdentityFile",
        );
    }

    #[test]
    fn an_unmatched_alias_resolves_nothing() {
        let (_dir, ssh) = load("Host prod\n  User deploy\n");
        assert_eq!(ssh.lookup("staging"), SshConfigMatch::default());
    }

    #[test]
    fn wildcard_defaults_merge_and_specific_blocks_win() {
        let (_dir, ssh) =
            load("Host prod\n  User deploy\n\nHost *\n  User fallback\n  Port 2200\n");
        let prod = ssh.lookup("prod");
        assert_eq!(prod.user.as_deref(), Some("deploy"), "the first match wins");
        assert_eq!(prod.port, Some(2200), "unset fields still come from Host *");

        assert_eq!(ssh.lookup("anything").user.as_deref(), Some("fallback"));
    }

    #[test]
    fn identity_files_accumulate_across_blocks() {
        let (_dir, ssh) =
            load("Host prod\n  IdentityFile /keys/a\n\nHost *\n  IdentityFile /keys/b\n");
        let identity = ssh.lookup("prod").identity;
        assert_eq!(
            identity,
            vec![PathBuf::from("/keys/a"), PathBuf::from("/keys/b")]
        );
    }

    #[test]
    fn tolerates_directives_the_parser_does_not_implement() {
        let (_dir, ssh) = load(
            "Host prod\n  User deploy\n  ControlMaster auto\n  ControlPath ~/.ssh/cm-%r@%h:%p\n\
             \n  ServerAliveCountMax 3\n  AddKeysToAgent yes\n  MadeUpDirective nonsense\n",
        );
        assert!(ssh.error().is_none(), "{:?}", ssh.error());
        assert_eq!(ssh.lookup("prod").user.as_deref(), Some("deploy"));
    }

    #[test]
    fn reports_proxy_jump_so_the_host_can_refuse_it() {
        let (_dir, ssh) = load("Host prod\n  ProxyJump gateway\n");
        assert_eq!(ssh.lookup("prod").proxy_jump.as_deref(), Some("gateway"));
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = SshConfigFile::load(Some(&dir.path().join("nope")));
        assert!(ssh.error().is_none());
        assert_eq!(ssh.lookup("prod"), SshConfigMatch::default());
    }

    #[test]
    fn a_malformed_file_reports_but_does_not_panic() {
        let (_dir, ssh) = load("  IdentityFile\nHost\n");
        assert!(
            ssh.error().is_some(),
            "the parse failure should be recorded"
        );
        assert_eq!(ssh.lookup("prod"), SshConfigMatch::default());
    }

    #[test]
    fn opting_out_resolves_nothing() {
        assert_eq!(
            SshConfigFile::disabled().lookup("prod"),
            SshConfigMatch::default()
        );
    }
}
