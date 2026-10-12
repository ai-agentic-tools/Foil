//! Tests for the grace period: processes the command leaves behind get a
//! SIGTERM and FOIL_GRACE_MS to exit before SIGKILL. They pass with or
//! without a cgroup box, so CI runs them in both modes.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn foil(grace_ms: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_foil"));
    cmd.env("FOIL_GRACE_MS", grace_ms).args(["run", "--"]);
    cmd
}

/// A fresh private directory; the script gets it as `$1`.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("foil-grace-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir(&dir).unwrap();
    dir
}

/// Leaves behind a helper that writes `$1/cleaned` when it gets SIGTERM.
/// The command exits only once the helper's trap is in place.
const POLITE_HELPER: &str = r#"(trap 'echo cleaned > "$1/cleaned"; exit 0' TERM
: > "$1/ready"
while :; do sleep 0.05; done) >/dev/null &
while [ ! -e "$1/ready" ]; do sleep 0.01; done
exit 0"#;

/// Leaves behind a `sleep` that ignores SIGTERM (an ignored signal stays
/// ignored across exec) and prints its PID.
const STUBBORN_HELPER: &str = r#"sh -c 'trap "" TERM; : > "$1/ready"; exec sleep 30' sh "$1" >/dev/null &
while [ ! -e "$1/ready" ]; do sleep 0.01; done
echo $!
exit 0"#;

#[test]
fn leftover_gets_sigterm_and_time_to_clean_up() {
    let dir = scratch("polite");
    let status = foil("5000")
        .args(["sh", "-c", POLITE_HELPER, "sh"])
        .arg(&dir)
        .stdout(Stdio::null())
        .status()
        .unwrap();

    assert!(status.success());
    let cleaned = std::fs::read_to_string(dir.join("cleaned")).unwrap_or_default();
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(cleaned.trim(), "cleaned", "helper did not get to clean up");
}

#[test]
fn leftover_that_ignores_sigterm_is_killed_after_the_grace() {
    let dir = scratch("stubborn");
    let start = Instant::now();
    let out = foil("300")
        .args(["sh", "-c", STUBBORN_HELPER, "sh"])
        .arg(&dir)
        .output()
        .unwrap();
    let elapsed = start.elapsed();
    std::fs::remove_dir_all(&dir).unwrap();

    assert!(out.status.success());
    assert!(elapsed >= Duration::from_millis(300), "SIGKILL came before the grace ended");
    assert!(elapsed < Duration::from_secs(5), "stubborn sleep survived the grace");
    let pid = String::from_utf8(out.stdout).unwrap();
    assert!(!Path::new(&format!("/proc/{}", pid.trim())).exists(), "pid {} leaked", pid.trim());
}

#[test]
fn zero_grace_kills_leftovers_at_once() {
    let dir = scratch("zero");
    let status = foil("0")
        .args(["sh", "-c", POLITE_HELPER, "sh"])
        .arg(&dir)
        .stdout(Stdio::null())
        .status()
        .unwrap();

    assert!(status.success());
    let cleaned = dir.join("cleaned").exists();
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(!cleaned, "helper got a SIGTERM although the grace is 0");
}

#[test]
fn no_leftovers_means_no_wait() {
    let start = Instant::now();
    let status = foil("10000").arg("true").status().unwrap();

    assert!(status.success());
    assert!(start.elapsed() < Duration::from_secs(2), "foil waited with nothing left");
}

#[test]
fn signal_to_foil_cuts_the_grace_short() {
    let dir = scratch("hurry");
    let mut child = foil("30000")
        .args(["sh", "-c", STUBBORN_HELPER, "sh"])
        .arg(&dir)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    // The PID line means the command is about to exit; give it time to do so,
    // so the SIGTERM lands during the grace and is not forwarded to it.
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    std::thread::sleep(Duration::from_millis(200));

    let start = Instant::now();
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    let status = child.wait().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();

    // foil still exits the way the command did.
    assert!(status.success());
    assert!(start.elapsed() < Duration::from_secs(5), "foil sat out the whole grace");
    assert!(!Path::new(&format!("/proc/{}", line.trim())).exists(), "pid {} leaked", line.trim());
}

#[test]
fn invalid_grace_falls_back_to_the_default_with_a_warning() {
    let out = foil("soon").arg("true").output().unwrap();

    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("ignoring FOIL_GRACE_MS"), "stderr was: {stderr}");
}
