//! Tests for `foil fanout` and `foil pick`. Like tests/branch.rs, they need
//! branches (user namespaces + overlayfs); skipped where those are blocked,
//! unless FOIL_REQUIRE_BRANCH is set.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Env {
    root: PathBuf,
}

impl Env {
    fn new(test: &str) -> Env {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("foil-fanout-{}-{n}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("ws")).unwrap();
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::write(root.join("ws/answer.txt"), "none\n").unwrap();
        Env { root: root.canonicalize().unwrap() }
    }

    fn foil(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_foil"))
            .args(args)
            .current_dir(self.root.join("ws"))
            .env("FOIL_HOME", self.root.join("home"))
            .output()
            .unwrap()
    }

    fn read(&self, rel: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join("ws").join(rel)).ok()
    }

    fn branches(&self) -> String {
        text(&self.foil(&["list"]).stdout)
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = Command::new("chmod").args(["-R", "u+rwx"]).arg(&self.root).status();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn branches_available() -> bool {
    let env = Env::new("probe");
    let out = env.foil(&["run", "-b", "probe", "--", "true"]);
    if out.status.success() {
        return true;
    }
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("cannot isolate the command") && std::env::var_os("FOIL_REQUIRE_BRANCH").is_none(),
        "branches unavailable: {stderr}"
    );
    eprintln!("skipped: branches unavailable here: {stderr}");
    false
}

/// Attempt i writes its number; attempt 2 also leaves an extra file; only
/// attempts 2 and 3 "pass" (their answer is >= 2).
const ATTEMPT: &str = "echo $FOIL_ATTEMPT > answer.txt; [ $FOIL_ATTEMPT = 2 ] && echo x > extra.txt; ls";
const CHECK: &str = "[ $(cat answer.txt) -ge 2 ]";

#[test]
fn attempts_are_isolated_and_the_best_is_picked() {
    if !branches_available() {
        return;
    }
    let env = Env::new("pick");
    let out = env.foil(&["fanout", "-n", "3", "--name", "t", "--check", CHECK, "--", "sh", "-c", ATTEMPT]);
    let stderr = text(&out.stderr);

    assert!(out.status.success(), "{stderr}");
    // 2 and 3 pass; 3 changed fewer paths (no extra.txt).
    assert!(stderr.contains("best: t-3"), "{stderr}");
    assert!(stderr.contains("t-1") && stderr.contains("fail"), "{stderr}");
    // Nothing reached the workspace; all three branches exist.
    assert_eq!(env.read("answer.txt").as_deref(), Some("none\n"));
    assert_eq!(env.branches().lines().count(), 3);
    // Attempt 3 never saw attempt 2's extra file.
    let log = std::fs::read_to_string(env.root.join("home/fanout/t/attempt-3.log")).unwrap();
    assert!(!log.contains("extra.txt"), "{log}");

    let out = env.foil(&["pick", "t-3"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(env.read("answer.txt").as_deref(), Some("3\n"));
    assert_eq!(env.read("extra.txt"), None);
    assert_eq!(env.branches(), "");
    assert!(!env.root.join("home/fanout/t").exists());
}

#[test]
fn pick_flag_commits_the_winner_right_away() {
    if !branches_available() {
        return;
    }
    let env = Env::new("auto");
    let out = env.foil(&["fanout", "-n", "3", "--name", "t", "--check", CHECK, "--pick", "--", "sh", "-c", ATTEMPT]);

    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(env.read("answer.txt").as_deref(), Some("3\n"));
    assert_eq!(env.branches(), "");
}

#[test]
fn no_passing_attempt_keeps_everything_for_review() {
    if !branches_available() {
        return;
    }
    let env = Env::new("none");
    let out = env.foil(&["fanout", "-n", "2", "--name", "t", "--check", "false", "--pick", "--", "sh", "-c", ATTEMPT]);

    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("no attempt passed"));
    assert_eq!(env.read("answer.txt").as_deref(), Some("none\n"));
    assert_eq!(env.branches().lines().count(), 2);
}

#[test]
fn strict_attempts_get_their_number_too() {
    if !branches_available() {
        return;
    }
    let env = Env::new("strict");
    let out = env.foil(&["fanout", "-n", "2", "--name", "t", "--strict", "--check", CHECK, "--", "sh", "-c", ATTEMPT]);
    let stderr = text(&out.stderr);
    // Strict needs more than branches; where it is unavailable, skip.
    if stderr.contains("cannot isolate") && std::env::var_os("FOIL_REQUIRE_BRANCH").is_none() {
        return;
    }
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("best: t-2"), "{stderr}");
}

#[test]
fn a_hung_attempt_times_out_and_json_reports_every_attempt() {
    if !branches_available() {
        return;
    }
    let env = Env::new("timeout");
    // Attempt 1 hangs; attempts 2 and 3 answer.
    let script = "[ $FOIL_ATTEMPT = 1 ] && exec sleep 60; echo $FOIL_ATTEMPT > answer.txt";
    let started = std::time::Instant::now();
    let out = env.foil(&["fanout", "-n", "3", "--name", "t", "--timeout", "1s", "--check", CHECK, "--json", "--pick", "--", "sh", "-c", script]);
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));

    assert!(out.status.success(), "{stderr}");
    assert!(started.elapsed() < std::time::Duration::from_secs(20), "{stderr}");
    assert!(stderr.contains("timeout"), "{stderr}");
    assert!(stdout.starts_with("{\"base\":\"t\",\"best\":\"t-2\",\"picked\":true,"), "{stdout}");
    assert!(stdout.contains("{\"branch\":\"t-1\",\"status\":124,\"timed_out\":true,\"check\":false,\"changes\":0,"), "{stdout}");
    assert_eq!(stdout.matches("\"branch\":").count(), 3, "{stdout}");
    assert_eq!(env.read("answer.txt").as_deref(), Some("2\n"));
    assert_eq!(env.branches(), "");
}

#[test]
fn fanout_refuses_its_own_branch_options_and_bad_pick_names() {
    let env = Env::new("usage");
    let out = env.foil(&["fanout", "-n", "2", "-b", "x", "--", "true"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out.stderr).contains("leave out -b"));
    assert_eq!(env.foil(&["fanout", "-n", "0", "--", "true"]).status.code(), Some(2));
    let out = env.foil(&["pick", "plain"]);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("not a fanout attempt"));
}
