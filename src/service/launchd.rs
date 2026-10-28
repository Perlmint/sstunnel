//! macOS: LaunchAgents in the user's GUI domain, one for the daemon and an
//! optional one for the menu bar item.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::{LABEL, executable, output, output_ignoring_failure, write_unit};

const AGENT_LABEL: &str = "dev.perlmint.ssh-tunnel";
const TRAY_LABEL: &str = "dev.perlmint.ssh-tunnel.tray";

fn plist_path(label: &str) -> Result<PathBuf> {
    let home = directories::BaseDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .context("could not determine the user's home directory")?;
    Ok(home
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{label}.plist")))
}

fn domain_target() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}

fn service_target(label: &str) -> String {
    format!("{}/{label}", domain_target())
}

pub fn install(config_path: &Path, tray: bool) -> Result<()> {
    let exe = executable()?;
    let logs = crate::paths::log_dir()?;
    std::fs::create_dir_all(&logs).with_context(|| format!("creating {}", logs.display()))?;

    register(AGENT_LABEL, &daemon_plist(&exe, config_path, &logs))?;
    if tray {
        register(TRAY_LABEL, &tray_plist(&exe, config_path, &logs))?;
    } else {
        remove(TRAY_LABEL)?;
    }

    println!("logs: {}", logs.display());
    Ok(())
}

pub fn uninstall() -> Result<()> {
    remove(TRAY_LABEL)?;
    remove(AGENT_LABEL)
}

pub fn start() -> Result<()> {
    output("launchctl", &["kickstart", &service_target(AGENT_LABEL)])?;
    // Not running yet (e.g. after Quit) is the only case this changes anything.
    output_ignoring_failure("launchctl", &["kickstart", &service_target(TRAY_LABEL)]);
    println!("{LABEL} started");
    Ok(())
}

/// Stops the daemon only: the menu bar item stays up to offer starting it again.
pub fn stop() -> Result<()> {
    output(
        "launchctl",
        &["kill", "SIGTERM", &service_target(AGENT_LABEL)],
    )?;
    println!("{LABEL} sent SIGTERM");
    Ok(())
}

pub fn status() -> Result<()> {
    for label in [AGENT_LABEL, TRAY_LABEL] {
        match output("launchctl", &["print", &service_target(label)]) {
            Ok(report) => {
                let state = report
                    .lines()
                    .find_map(|line| line.trim().strip_prefix("state = "))
                    .unwrap_or("registered");
                println!("{label}: {state}");
            }
            Err(_) => println!("{label}: not installed"),
        }
    }
    Ok(())
}

/// Write the plist and (re)load it, replacing any previous registration.
fn register(label: &str, contents: &str) -> Result<()> {
    let plist = plist_path(label)?;
    write_unit(&plist, contents)?;
    output_ignoring_failure("launchctl", &["bootout", &service_target(label)]);
    output(
        "launchctl",
        &["bootstrap", &domain_target(), &plist.to_string_lossy()],
    )?;
    output_ignoring_failure("launchctl", &["enable", &service_target(label)]);
    println!("installed {}", plist.display());
    Ok(())
}

fn remove(label: &str) -> Result<()> {
    let plist = plist_path(label)?;
    output_ignoring_failure("launchctl", &["bootout", &service_target(label)]);
    if plist.exists() {
        std::fs::remove_file(&plist).with_context(|| format!("removing {}", plist.display()))?;
        println!("removed {}", plist.display());
    }
    Ok(())
}

fn daemon_plist(exe: &Path, config: &Path, logs: &Path) -> String {
    plist_contents(
        AGENT_LABEL,
        &[exe, Path::new("daemon"), Path::new("--config"), config],
        "<true/>",
        "Background",
        "",
        &logs.join("ssh-tunnel.log"),
    )
}

/// Restarted after a crash, but not after the user picks Quit (a clean exit).
fn tray_plist(exe: &Path, config: &Path, logs: &Path) -> String {
    plist_contents(
        TRAY_LABEL,
        &[exe, Path::new("tray"), Path::new("--config"), config],
        "<dict>\n\t\t<key>SuccessfulExit</key>\n\t\t<false/>\n\t</dict>",
        "Interactive",
        // A menu bar item needs a logged-in GUI session.
        "\t<key>LimitLoadToSessionType</key>\n\t<string>Aqua</string>\n",
        &logs.join("tray.log"),
    )
}

fn plist_contents(
    label: &str,
    args: &[&Path],
    keep_alive: &str,
    process_type: &str,
    extra: &str,
    log: &Path,
) -> String {
    let args: String = args
        .iter()
        .map(|arg| format!("\t\t<string>{}</string>\n", arg.display()))
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
{args}	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	{keep_alive}
	<key>ProcessType</key>
	<string>{process_type}</string>
{extra}	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
</dict>
</plist>
"#,
        log = log.display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plists_are_valid() {
        let dir = tempfile::tempdir().unwrap();
        let (exe, config) = (Path::new("/bin/ssh-tunnel"), Path::new("/c.toml"));
        for (name, contents) in [
            ("daemon.plist", daemon_plist(exe, config, dir.path())),
            ("tray.plist", tray_plist(exe, config, dir.path())),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, contents).unwrap();
            output("plutil", &["-lint", &path.to_string_lossy()]).unwrap();
        }
    }
}
