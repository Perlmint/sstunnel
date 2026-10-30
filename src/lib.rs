//! An SSH tunnel agent: keeps sessions open, serves forwards declared in a
//! config file, and lets each one be switched on and off at runtime.

pub mod cli;
pub mod config;
pub mod daemon;
pub mod ipc;
pub mod paths;
pub mod service;
pub mod ssh;
pub mod ssh_config;
pub mod state;
#[cfg(any(target_os = "macos", windows))]
pub mod tray;
pub mod watcher;
