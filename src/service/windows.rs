//! Windows: per-user scheduled tasks that start at logon, one for the daemon
//! and an optional one for the tray icon.
//!
//! A real Windows Service would run as LocalSystem or a service account, which
//! cannot reach the user's ssh-agent, `known_hosts`, or profile. A logon task
//! runs inside the user's session, where SSH authentication actually works.

use std::path::Path;

use anyhow::Result;

use super::{LABEL, executable, output, output_ignoring_failure};

const TASK: &str = "ssh-tunnel";
const TRAY_TASK: &str = "ssh-tunnel-tray";

pub fn install(config_path: &Path, tray: bool) -> Result<()> {
    let exe = executable()?;
    let logs = crate::paths::log_dir()?;
    std::fs::create_dir_all(&logs)?;
    let log_file = logs.join("ssh-tunnel.log");

    // A scheduled task has nowhere to send stderr, so log to a file instead.
    let command = format!(
        "\"{}\" daemon --config \"{}\" --log-file \"{}\"",
        exe.display(),
        config_path.display(),
        log_file.display(),
    );
    register(TASK, &command)?;

    if tray {
        let command = format!(
            "\"{}\" tray --config \"{}\"",
            exe.display(),
            config_path.display(),
        );
        register(TRAY_TASK, &command)?;
    } else {
        remove(TRAY_TASK);
    }

    println!("logs: {}", log_file.display());
    Ok(())
}

pub fn uninstall() -> Result<()> {
    remove(TRAY_TASK);
    output_ignoring_failure("schtasks", &["/End", "/TN", TASK]);
    output("schtasks", &["/Delete", "/TN", TASK, "/F"])?;
    println!("removed the scheduled task `{TASK}`");
    Ok(())
}

pub fn start() -> Result<()> {
    output("schtasks", &["/Run", "/TN", TASK])?;
    // Only changes anything when the tray was quit; absent with --no-tray.
    output_ignoring_failure("schtasks", &["/Run", "/TN", TRAY_TASK]);
    println!("{LABEL} started");
    Ok(())
}

/// Stops the daemon only: the tray icon stays up to offer starting it again.
pub fn stop() -> Result<()> {
    output("schtasks", &["/End", "/TN", TASK])?;
    println!("{LABEL} stopped");
    Ok(())
}

pub fn status() -> Result<()> {
    for task in [TASK, TRAY_TASK] {
        match output("schtasks", &["/Query", "/TN", task, "/FO", "LIST"]) {
            Ok(report) => print!("{report}"),
            Err(_) => println!("{task}: not installed"),
        }
    }
    Ok(())
}

/// Create (or replace) a logon task, and start it now.
fn register(task: &str, command: &str) -> Result<()> {
    output(
        "schtasks",
        &[
            "/Create", "/TN", task, "/TR", command, "/SC", "ONLOGON", "/RL", "LIMITED", "/F",
        ],
    )?;
    output_ignoring_failure("schtasks", &["/Run", "/TN", task]);
    println!("installed the scheduled task `{task}`");
    Ok(())
}

/// Stop and delete a task, if it exists.
fn remove(task: &str) {
    output_ignoring_failure("schtasks", &["/End", "/TN", task]);
    output_ignoring_failure("schtasks", &["/Delete", "/TN", task, "/F"]);
}
