//! macOS: resource limits for `foil run`.
//!
//! There are no cgroups. `--memory` becomes a watchdog in the supervisor:
//! it adds up the run's memory every 200 ms and stops the whole run when it
//! goes over (not a hard kernel limit: a fast allocation can overshoot
//! between two looks). `--cpus` and `--pids` have no faithful equivalent
//! and are refused rather than half-done.

use std::ffi::OsString;
use std::io;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::atomic::Ordering;

pub use crate::units::{parse_duration, parse_size};

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Limits {
    pub memory: Option<u64>,
    pub cpus: Option<f64>,
    pub pids: Option<u64>,
}

impl Limits {
    pub fn is_empty(&self) -> bool {
        *self == Limits::default()
    }

    /// Check the limits can be kept here. Unlike Linux, never re-runs foil.
    pub fn enforce(&self, _args: &[OsString], _want_box: bool) -> io::Result<()> {
        if self.cpus.is_some() || self.pids.is_some() {
            return Err(io::Error::other(
                "--cpus and --pids need Linux cgroups; on macOS only --memory (a watchdog) and --timeout are available",
            ));
        }
        Ok(())
    }
}

/// Whether the memory watchdog stopped the run.
pub fn oom_killed() -> bool {
    crate::supervisor::MEMORY_KILLED.load(Ordering::SeqCst)
}

/// Whether `program` is found on PATH.
pub fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

/// `foil session`: on macOS runs start without systemd, so this just runs
/// the command. Only returns on failure.
pub fn session(command: &[OsString]) -> io::Error {
    let Some((program, args)) = command.split_first() else {
        return io::Error::new(io::ErrorKind::InvalidInput, "foil session: no command");
    };
    Command::new(program).args(args).exec()
}
