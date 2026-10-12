//! Resource limits for `foil run`: `--memory`, `--cpus`, `--pids`.
//!
//! cgroup v2 enforces limits, but only a cgroup's parent may switch a
//! controller on for it, and only while the parent holds no processes
//! itself. foil's own cgroup usually holds foil (and its shell), so foil
//! cannot limit a box it creates there. Instead it asks systemd: foil runs
//! itself again through `systemd-run --user --scope`, which makes a fresh
//! cgroup ("scope") with the limits set and delegated to us, then execs foil
//! inside it in place: same PID, terminal, stdio and environment. The second
//! foil sees FOIL_SCOPED, checks that the limits really are in force, and
//! carries on as usual, creating its box inside the scope.
//!
//! The same trick gives branch and isolated runs a cleanup box when foil's
//! own cgroup is not delegated (an ordinary login shell): a scope without
//! limits, ~12 ms. Commit safety relies on the box (see `branch.rs`), and an
//! agent session runs for minutes. FOIL_NO_SCOPE=1 turns this off.
//!
//! The limits cover the whole scope: the command, everything it starts, and
//! foil itself (one process, a few MB).

use std::ffi::OsString;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

/// Set in the environment of the foil that runs inside the limited scope.
const SCOPED: &str = "FOIL_SCOPED";

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Limits {
    /// Bytes of memory, swap not included (swap is turned off for the run).
    pub memory: Option<u64>,
    /// CPUs' worth of time, e.g. 1.5.
    pub cpus: Option<f64>,
    /// Processes and threads.
    pub pids: Option<u64>,
}

impl Limits {
    pub fn is_empty(&self) -> bool {
        *self == Limits::default()
    }

    /// Make sure the limits are in force: either verify them (when this foil
    /// already runs in the limited scope) or re-run foil, with `args`, in a
    /// new scope that has them. Returns only in the first case. With no
    /// limits, `want_box` asks for a scope anyway if foil could not create
    /// its cleanup box otherwise, when systemd makes that possible.
    pub fn enforce(&self, args: &[OsString], want_box: bool) -> io::Result<()> {
        if std::env::var_os(SCOPED).is_some() {
            // Keep the marker away from the command.
            std::env::remove_var(SCOPED);
            return self.verify();
        }
        if self.is_empty() {
            let wanted = want_box
                && std::env::var_os("FOIL_NO_SCOPE").is_none()
                && std::env::var_os("FOIL_NO_CGROUP").is_none()
                && !crate::cgroup::can_box()
                && user_session().is_ok()
                && on_path("systemd-run");
            if !wanted {
                return Ok(());
            }
        }
        user_session()?;
        let mut cmd = Command::new("systemd-run");
        cmd.args(["--user", "--scope", "--quiet", "--collect", "-p", "Delegate=yes"]);
        if let Some(bytes) = self.memory {
            cmd.args(["-p", &format!("MemoryMax={bytes}"), "-p", "MemorySwapMax=0"]);
        }
        if let Some(cpus) = self.cpus {
            cmd.args(["-p", &format!("CPUQuota={}%", cpu_quota_percent(cpus))]);
        }
        if let Some(pids) = self.pids {
            cmd.args(["-p", &format!("TasksMax={pids}")]);
        }
        cmd.arg("--").arg(std::env::current_exe()?).args(args).env(SCOPED, "1");
        // exec only returns on failure.
        let err = cmd.exec();
        Err(io::Error::new(
            err.kind(),
            format!("resource limits need systemd-run (systemd's user manager): {err}"),
        ))
    }

    /// Check that the limits are set on foil's own cgroup (the scope).
    fn verify(&self) -> io::Result<()> {
        let own = std::fs::read_to_string("/proc/self/cgroup")?;
        let rel = own.lines().find_map(|l| l.strip_prefix("0::")).unwrap_or("/");
        let dir = Path::new("/sys/fs/cgroup").join(rel.trim_start_matches('/'));
        // An unlimited value reads "max" (cpu.max: "max <period>").
        let limited = |file: &str| std::fs::read_to_string(dir.join(file)).is_ok_and(|v| !v.starts_with("max"));
        let mut missing = Vec::new();
        if self.memory.is_some() && !limited("memory.max") {
            missing.push("memory");
        }
        if self.cpus.is_some() && !limited("cpu.max") {
            missing.push("cpu");
        }
        if self.pids.is_some() && !limited("pids.max") {
            missing.push("pids");
        }
        if missing.is_empty() {
            return Ok(());
        }
        Err(io::Error::other(format!(
            "systemd did not apply the {} limit; is that cgroup controller delegated to your user \
             manager? (see `systemctl show user@$(id -u).service -p DelegateControllers`)",
            missing.join(", ")
        )))
    }
}

/// Whether foil's cgroup (the scope) saw the kernel's OOM killer at work.
pub fn oom_killed() -> bool {
    let own = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
    let rel = own.lines().find_map(|l| l.strip_prefix("0::")).unwrap_or("/");
    let events = Path::new("/sys/fs/cgroup").join(rel.trim_start_matches('/')).join("memory.events");
    std::fs::read_to_string(events).is_ok_and(|text| {
        text.lines()
            .any(|l| l.strip_prefix("oom_kill ").is_some_and(|n| n.trim().parse::<u64>().is_ok_and(|n| n > 0)))
    })
}

/// Whether `program` is found on PATH.
pub fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

/// Fail early, with a clear message, when there is no systemd user manager
/// to talk to (systemd-run itself would print a terse D-Bus error).
fn user_session() -> io::Result<()> {
    let bus = std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some()
        || std::env::var_os("XDG_RUNTIME_DIR").is_some_and(|dir| Path::new(&dir).join("bus").exists());
    if bus {
        Ok(())
    } else {
        Err(io::Error::other(
            "resource limits need a systemd user session (no session bus found); \
             log in normally, or run `loginctl enable-linger $USER` for services",
        ))
    }
}

/// `--cpus` as systemd's CPUQuota percentage of one CPU. Rounded to the
/// nearest percent (1.1 * 100.0 is 110.00000000000001 in floating point, so
/// rounding up would give 111), but never below 1 so a tiny limit is not 0.
fn cpu_quota_percent(cpus: f64) -> u64 {
    ((cpus * 100.0).round() as u64).max(1)
}

/// Parse a memory size: bytes, or a number with K, M, G or T (powers of 1024).
pub fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let (number, unit) = match text.find(|c: char| c.is_ascii_alphabetic()) {
        Some(i) => text.split_at(i),
        None => (text, ""),
    };
    let scale: u64 = match unit.to_ascii_uppercase().trim_end_matches(['B', 'I']) {
        "" => 1,
        "K" => 1 << 10,
        "M" => 1 << 20,
        "G" => 1 << 30,
        "T" => 1 << 40,
        _ => return None,
    };
    let value: f64 = number.trim().parse().ok()?;
    let bytes = value * scale as f64;
    (value.is_finite() && bytes >= 1.0 && bytes < u64::MAX as f64).then_some(bytes as u64)
}

#[cfg(test)]
mod tests {
    use super::{cpu_quota_percent, parse_size};

    #[test]
    fn cpu_quotas() {
        assert_eq!(cpu_quota_percent(1.1), 110);
        assert_eq!(cpu_quota_percent(0.5), 50);
        assert_eq!(cpu_quota_percent(2.0), 200);
        assert_eq!(cpu_quota_percent(0.001), 1);
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("512"), Some(512));
        assert_eq!(parse_size("64K"), Some(64 << 10));
        assert_eq!(parse_size("1.5G"), Some(3 << 29));
        assert_eq!(parse_size("2gib"), Some(2 << 30));
        assert_eq!(parse_size("100MB"), Some(100 << 20));
        assert_eq!(parse_size("0"), None);
        assert_eq!(parse_size("-1G"), None);
        assert_eq!(parse_size("lots"), None);
        assert_eq!(parse_size("5X"), None);
    }
}
