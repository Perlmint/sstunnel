//! Config file watching.
//!
//! The parent directory is watched rather than the file itself: editors save by
//! writing a temp file and renaming it over the original, which replaces the
//! inode and silently breaks a watch on the file.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use notify::RecursiveMode;
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::daemon::Command;

const DEBOUNCE: Duration = Duration::from_millis(500);

/// Watching stops when this is dropped.
pub struct ConfigWatcher {
    _debouncer: Debouncer<notify::RecommendedWatcher, RecommendedCache>,
}

pub fn watch(config_path: &Path, commands: mpsc::Sender<Command>) -> Result<ConfigWatcher> {
    let directory = config_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| Path::new(".").to_path_buf());
    let file_name = config_path
        .file_name()
        .context("the config path has no file name")?
        .to_owned();

    let mut debouncer = new_debouncer(DEBOUNCE, None, move |result: DebounceEventResult| {
        match result {
            Ok(events) => {
                let touched = events.iter().any(|event| {
                    event
                        .paths
                        .iter()
                        .any(|path| path.file_name() == Some(&file_name))
                });
                if !touched {
                    return;
                }
                debug!("the config file changed");
                // A full channel already has a reload queued, and reloads are
                // idempotent, so dropping this one loses nothing.
                let _ = commands.try_send(Command::ConfigChanged);
            }
            Err(errors) => {
                for error in errors {
                    warn!(%error, "config watch error");
                }
            }
        }
    })
    .context("starting the config watcher")?;

    debouncer
        .watch(&directory, RecursiveMode::NonRecursive)
        .with_context(|| format!("watching {}", directory.display()))?;

    Ok(ConfigWatcher {
        _debouncer: debouncer,
    })
}
