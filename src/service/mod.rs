//! Registering the daemon with the per-user service manager.
//!
//! Everything here is deliberately *user* scope. A system service runs without
//! the user's ssh-agent, `~/.ssh/known_hosts`, or profile, which is exactly what
//! SSH authentication needs.

#[cfg(target_os = "macos")]
mod launchd;
#[cfg(all(unix, not(target_os = "macos")))]
mod systemd;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "macos")]
use launchd as platform;
#[cfg(all(unix, not(target_os = "macos")))]
use systemd as platform;
#[cfg(windows)]
use windows as platform;

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::cli::ServiceAction;

pub const LABEL: &str = "ssh-tunnel";

pub fn run(action: &ServiceAction, config_path: &Path) -> Result<()> {
    match action {
        #[cfg(any(target_os = "macos", windows))]
        ServiceAction::Install(args) => platform::install(config_path, !args.no_tray),
        #[cfg(not(any(target_os = "macos", windows)))]
        ServiceAction::Install(_) => platform::install(config_path),
        ServiceAction::Uninstall => platform::uninstall(),
        ServiceAction::Start => platform::start(),
        ServiceAction::Stop => platform::stop(),
        ServiceAction::Status => platform::status(),
    }
}

/// The absolute path of the running binary, which the service unit points at.
pub fn executable() -> Result<PathBuf> {
    std::env::current_exe().context("could not determine this executable's path")
}

/// A command that never opens a console window, even from a windowless process.
fn command(program: &str) -> Command {
    #[allow(unused_mut)]
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// Run a command, returning stdout and failing with stderr.
pub fn output(program: &str, args: &[&str]) -> Result<String> {
    let output = command(program)
        .args(args)
        .output()
        .with_context(|| format!("running `{program}`"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if stderr.trim().is_empty() {
            stdout
        } else {
            stderr
        };
        bail!("`{program} {}` failed: {}", args.join(" "), detail.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Run a command, ignoring failure. Used for "stop it if it happens to be running".
pub fn output_ignoring_failure(program: &str, args: &[&str]) {
    let _ = command(program).args(args).output();
}

pub fn write_unit(path: &Path, contents: &str) -> Result<()> {
    crate::paths::ensure_parent_dir(path)?;
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
}
