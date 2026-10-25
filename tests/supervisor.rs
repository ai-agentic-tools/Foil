//! End-to-end tests: each runs the real `foil` binary in its own process, so
//! signals aimed at foil never hit the multi-threaded test harness.

use std::io::{BufRead, BufReader};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn foil() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_foil"));
    cmd.args(["run", "--"]);
    cmd
}

fn first_line(child: &mut Child) -> String {
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    line.trim().to_owned()
}

fn is_alive(pid: &str) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => {
            let state = stat[stat.rfind(')').unwrap() + 2..].chars().next();
            !matches!(state, Some('Z' | 'X'))
        }
        Err(_) => false,
    }
}

#[test]
fn propagates_exit_code() {
    let status = foil().args(["sh", "-c", "exit 42"]).status().unwrap();
    assert_eq!(status.code(), Some(42));
}

#[test]
fn propagates_death_by_signal() {
    let status = foil().args(["sh", "-c", "kill -TERM $$"]).status().unwrap();
    assert_eq!(status.signal(), Some(libc::SIGTERM));
}

#[test]
fn missing_command_exits_127() {
    let status = foil().arg("foil-no-such-command").status().unwrap();
    assert_eq!(status.code(), Some(127));
}

#[test]
fn forwards_sigterm_to_child() {
    let mut child = foil()
        .args(["sh", "-c", "trap 'exit 7' TERM; echo ready; sleep 30 & wait"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    assert_eq!(first_line(&mut child), "ready");

    let start = Instant::now();
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    let status = child.wait().unwrap();

    assert_eq!(status.code(), Some(7));
    // The orphaned `sleep 30` must have been killed, not waited for.
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn reaps_orphaned_and_setsid_grandchildren() {
    let start = Instant::now();
    // output() reads stdout to EOF, so it blocks while any orphan holds the pipe.
    let out = foil()
        .args(["sh", "-c", "sleep 30 & echo $!; setsid sleep 30 & echo $!; exit 0"])
        .output()
        .unwrap();

    assert!(out.status.success());
    assert!(start.elapsed() < Duration::from_secs(5), "orphans survived foil");
    for pid in String::from_utf8(out.stdout).unwrap().split_whitespace() {
        assert!(!Path::new(&format!("/proc/{pid}")).exists(), "pid {pid} leaked");
    }
}

#[test]
fn child_dies_when_foil_is_sigkilled() {
    let mut child = foil()
        .args(["sh", "-c", "echo $$; exec sleep 30"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = first_line(&mut child);

    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGKILL) };
    child.wait().unwrap();

    let deadline = Instant::now() + Duration::from_secs(2);
    while is_alive(&pid) {
        assert!(Instant::now() < deadline, "pid {pid} outlived foil");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn timeout_stops_the_command_and_everything_it_started() {
    // The command ignores SIGTERM and leaves a background sleeper: SIGKILL
    // after the grace period gets the command, the cleanup gets the rest.
    let started = Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_foil"))
        .args(["run", "--timeout", "300ms", "--", "sh", "-c", "trap '' TERM; sleep 60 & echo $!; wait"])
        .env("FOIL_GRACE_MS", "200")
        .output()
        .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(out.status.code(), Some(124), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("stopped after --timeout"));
    assert!(elapsed < Duration::from_secs(5), "took {elapsed:?}");
    let sleeper = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    assert!(!sleeper.is_empty() && !is_alive(&sleeper), "sleeper {sleeper} survived");
}

#[test]
fn a_command_that_finishes_in_time_keeps_its_status() {
    let out = Command::new(env!("CARGO_BIN_EXE_foil"))
        .args(["run", "--timeout", "30s", "--", "sh", "-c", "exit 3"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    let bad = Command::new(env!("CARGO_BIN_EXE_foil")).args(["run", "--timeout", "soon", "--", "true"]).output().unwrap();
    assert_eq!(bad.status.code(), Some(2));
}
