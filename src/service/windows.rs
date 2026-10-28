//! Windows: a per-user scheduled task that starts at logon.
//!
//! A real Windows Service would run as LocalSystem or a service account, which
//! cannot reach the user's ssh-agent, `known_hosts`, or profile. A logon task
//! runs inside the user's session, where SSH authentication actually works.

use std::path::Path;

use anyhow::Result;

use super::{LABEL, executable, output, output_ignoring_failure};

const TASK: &str = "ssh-tunnel";

pub fn install(config_path: &Path) -> Result<()> {
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

    output(
        "schtasks",
        &[
            "/Create", "/TN", TASK, "/TR", &command, "/SC", "ONLOGON", "/RL", "LIMITED", "/F",
        ],
    )?;
    output_ignoring_failure("schtasks", &["/Run", "/TN", TASK]);

    println!("installed the scheduled task `{TASK}`");
    println!("logs: {}", log_file.display());
    Ok(())
}

pub fn uninstall() -> Result<()> {
    output_ignoring_failure("schtasks", &["/End", "/TN", TASK]);
    output("schtasks", &["/Delete", "/TN", TASK, "/F"])?;
    println!("removed the scheduled task `{TASK}`");
    Ok(())
}

pub fn start() -> Result<()> {
    output("schtasks", &["/Run", "/TN", TASK])?;
    println!("{LABEL} started");
    Ok(())
}

pub fn stop() -> Result<()> {
    output("schtasks", &["/End", "/TN", TASK])?;
    println!("{LABEL} stopped");
    Ok(())
}

pub fn status() -> Result<()> {
    match output("schtasks", &["/Query", "/TN", TASK, "/FO", "LIST"]) {
        Ok(report) => print!("{report}"),
        Err(_) => println!("{TASK}: not installed"),
    }
    Ok(())
}
