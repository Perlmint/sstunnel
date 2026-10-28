//! Linux: a systemd user unit.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::{LABEL, executable, output, output_ignoring_failure, write_unit};

const UNIT: &str = "ssh-tunnel.service";

fn unit_path() -> Result<PathBuf> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|dirs| dirs.home_dir().join(".config")))
        .context("could not determine the user's config directory")?;
    Ok(base.join("systemd").join("user").join(UNIT))
}

pub fn install(config_path: &Path) -> Result<()> {
    let exe = executable()?;
    let path = unit_path()?;
    write_unit(&path, &unit_contents(&exe, config_path))?;

    output("systemctl", &["--user", "daemon-reload"])?;
    output("systemctl", &["--user", "enable", "--now", UNIT])?;

    println!("installed {}", path.display());
    println!("logs: journalctl --user -u {UNIT} -f");
    println!(
        "on a headless machine, run `loginctl enable-linger {}` so the unit \
         starts without a login session",
        crate::config::current_username()
    );
    Ok(())
}

pub fn uninstall() -> Result<()> {
    output_ignoring_failure("systemctl", &["--user", "disable", "--now", UNIT]);
    let path = unit_path()?;
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    output_ignoring_failure("systemctl", &["--user", "daemon-reload"]);
    println!("removed {}", path.display());
    Ok(())
}

pub fn start() -> Result<()> {
    output("systemctl", &["--user", "start", UNIT])?;
    println!("{LABEL} started");
    Ok(())
}

pub fn stop() -> Result<()> {
    output("systemctl", &["--user", "stop", UNIT])?;
    println!("{LABEL} stopped");
    Ok(())
}

pub fn status() -> Result<()> {
    // `is-active` exits non-zero when inactive, which is not an error here.
    match output("systemctl", &["--user", "is-active", UNIT]) {
        Ok(state) => println!("{UNIT}: {}", state.trim()),
        Err(_) => println!("{UNIT}: inactive or not installed"),
    }
    Ok(())
}

fn unit_contents(exe: &Path, config: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=SSH tunnel agent\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exe} daemon --config {config}\n\
         ExecReload=/bin/kill -HUP $MAINPID\n\
         Restart=always\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exe = exe.display(),
        config = config.display(),
    )
}
