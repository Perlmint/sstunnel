//! Per-OS locations for the config file, the IPC endpoint, and runtime state.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// `~/.config/ssh-tunnel` on unix, `%APPDATA%\ssh-tunnel` on Windows.
pub fn config_dir() -> Result<PathBuf> {
    #[cfg(unix)]
    {
        if let Some(xdg) = non_empty_env("XDG_CONFIG_HOME") {
            return Ok(PathBuf::from(xdg).join("ssh-tunnel"));
        }
        Ok(home()?.join(".config").join("ssh-tunnel"))
    }
    #[cfg(not(unix))]
    {
        let dirs = directories::ProjectDirs::from("", "", "ssh-tunnel")
            .context("could not determine the user's config directory")?;
        Ok(dirs.config_dir().to_path_buf())
    }
}

pub fn default_config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

/// Where runtime state (the persisted enable/disable overrides) lives.
pub fn state_dir() -> Result<PathBuf> {
    #[cfg(unix)]
    {
        if let Some(xdg) = non_empty_env("XDG_STATE_HOME") {
            return Ok(PathBuf::from(xdg).join("ssh-tunnel"));
        }
        Ok(home()?.join(".local").join("state").join("ssh-tunnel"))
    }
    #[cfg(not(unix))]
    {
        let dirs = directories::ProjectDirs::from("", "", "ssh-tunnel")
            .context("could not determine the user's data directory")?;
        Ok(dirs.data_local_dir().to_path_buf())
    }
}

pub fn default_state_path() -> Result<PathBuf> {
    Ok(state_dir()?.join("state.json"))
}

pub fn log_dir() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        Ok(home()?.join("Library").join("Logs").join("ssh-tunnel"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(state_dir()?.join("logs"))
    }
}

/// The IPC endpoint: a namespaced pipe name on Windows, a socket path on unix.
///
/// Unix domain socket paths are capped at ~104 bytes, so this deliberately
/// prefers short per-user directories over the state directory.
pub fn default_socket_name() -> Result<String> {
    #[cfg(windows)]
    {
        Ok(format!(
            "ssh-tunnel-{}.sock",
            crate::config::current_username()
        ))
    }
    #[cfg(target_os = "macos")]
    {
        // Per-user and short: /var/folders/../T/ssh-tunnel.sock
        let tmp = non_empty_env("TMPDIR").unwrap_or_else(|| "/tmp".into());
        Ok(format!(
            "{}/ssh-tunnel-{}.sock",
            tmp.trim_end_matches('/'),
            unsafe { libc::getuid() }
        ))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(run) = non_empty_env("XDG_RUNTIME_DIR") {
            return Ok(format!("{}/ssh-tunnel.sock", run.trim_end_matches('/')));
        }
        Ok(format!("/tmp/ssh-tunnel-{}.sock", unsafe {
            libc::getuid()
        }))
    }
}

#[cfg(unix)]
fn home() -> Result<PathBuf> {
    directories::BaseDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .context("could not determine the user's home directory")
}

#[allow(dead_code)]
fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Create `path`'s parent directory, with 0700 permissions on unix.
pub fn ensure_parent_dir(path: &std::path::Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() || parent.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}
