//! macOS: a LaunchAgent in the user's GUI domain.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::{LABEL, executable, output, output_ignoring_failure, write_unit};

const AGENT_LABEL: &str = "dev.perlmint.ssh-tunnel";

fn plist_path() -> Result<PathBuf> {
    let home = directories::BaseDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .context("could not determine the user's home directory")?;
    Ok(home
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{AGENT_LABEL}.plist")))
}

fn domain_target() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}

fn service_target() -> String {
    format!("{}/{AGENT_LABEL}", domain_target())
}

pub fn install(config_path: &Path) -> Result<()> {
    let exe = executable()?;
    let logs = crate::paths::log_dir()?;
    std::fs::create_dir_all(&logs).with_context(|| format!("creating {}", logs.display()))?;

    let plist = plist_path()?;
    write_unit(&plist, &plist_contents(&exe, config_path, &logs))?;

    // Replace any previous registration.
    output_ignoring_failure("launchctl", &["bootout", &service_target()]);
    output(
        "launchctl",
        &["bootstrap", &domain_target(), &plist.to_string_lossy()],
    )?;
    output_ignoring_failure("launchctl", &["enable", &service_target()]);

    println!("installed {}", plist.display());
    println!("logs: {}", logs.display());
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let plist = plist_path()?;
    output_ignoring_failure("launchctl", &["bootout", &service_target()]);
    if plist.exists() {
        std::fs::remove_file(&plist).with_context(|| format!("removing {}", plist.display()))?;
    }
    println!("removed {}", plist.display());
    Ok(())
}

pub fn start() -> Result<()> {
    output("launchctl", &["kickstart", &service_target()])?;
    println!("{LABEL} started");
    Ok(())
}

pub fn stop() -> Result<()> {
    output("launchctl", &["kill", "SIGTERM", &service_target()])?;
    println!("{LABEL} sent SIGTERM");
    Ok(())
}

pub fn status() -> Result<()> {
    match output("launchctl", &["print", &service_target()]) {
        Ok(report) => {
            let state = report
                .lines()
                .find_map(|line| line.trim().strip_prefix("state = "))
                .unwrap_or("registered");
            println!("{AGENT_LABEL}: {state}");
        }
        Err(_) => println!("{AGENT_LABEL}: not installed"),
    }
    Ok(())
}

fn plist_contents(exe: &Path, config: &Path, logs: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{AGENT_LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{exe}</string>
		<string>daemon</string>
		<string>--config</string>
		<string>{config}</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>ProcessType</key>
	<string>Background</string>
	<key>StandardOutPath</key>
	<string>{logs}/ssh-tunnel.log</string>
	<key>StandardErrorPath</key>
	<string>{logs}/ssh-tunnel.log</string>
</dict>
</plist>
"#,
        exe = exe.display(),
        config = config.display(),
        logs = logs.display(),
    )
}
