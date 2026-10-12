//! Tests for `--protect` and `--no-network`. Like branches, they need
//! unprivileged user namespaces; where those are blocked the tests are
//! skipped, unless FOIL_REQUIRE_BRANCH is set, which turns a skip into a failure.
//! Their folders live in Cargo's target dir, not /tmp, which --protect keeps
//! writable.

use std::path::PathBuf;
use std::process::{Command, Output};

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A fresh folder for one test, removed on drop.
struct Dir(PathBuf);

impl Dir {
    fn new(test: &str) -> Dir {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("isolate-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Dir(dir.canonicalize().unwrap())
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = Command::new("chmod").args(["-R", "u+rwx"]).arg(&self.0).status();
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `foil run <options> -- sh -c <script>`.
fn run(options: &[&str], script: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_foil"))
        .arg("run")
        .args(options)
        .args(["--", "sh", "-c", script])
        .output()
        .unwrap()
}

/// Whether isolation works here; skips (or fails, if required) otherwise.
fn available() -> bool {
    let out = run(&["--no-network"], "true");
    if out.status.success() {
        return true;
    }
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("cannot isolate the command") && std::env::var_os("FOIL_REQUIRE_BRANCH").is_none(),
        "isolation unavailable: {stderr}"
    );
    eprintln!("skipped: isolation unavailable here: {stderr}");
    false
}

#[test]
fn protect_makes_everything_else_read_only() {
    if !available() {
        return;
    }
    let dir = Dir::new("ro");
    let file = dir.0.join("f");
    let out = run(&["--protect"], &format!("echo x > {}", file.display()));

    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("Read-only file system"), "{}", text(&out.stderr));
    assert!(!file.exists());
}

#[test]
fn protect_keeps_tmp_and_allowed_paths_writable() {
    if !available() {
        return;
    }
    let dir = Dir::new("allowed");
    let tmp = std::env::temp_dir().join(format!("foil-protect-{}", std::process::id()));
    let script = format!("echo x > {}/f && echo y > {}", dir.0.display(), tmp.display());
    let out = run(&["--protect", "--allow-write", dir.0.to_str().unwrap()], &script);
    let _ = std::fs::remove_file(&tmp);

    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(std::fs::read_to_string(dir.0.join("f")).unwrap(), "x\n");
}

#[test]
fn protect_with_a_branch_lets_the_command_change_only_the_branch() {
    if !available() {
        return;
    }
    let dir = Dir::new("branch");
    let ws = dir.0.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("a.txt"), "base\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_foil"))
        .args(["run", "--branch", "t", "--protect", "--", "sh", "-c"])
        .arg("echo new > a.txt && cat a.txt && ! (echo x > ../outside) 2>/dev/null")
        .current_dir(&ws)
        .env("FOIL_HOME", dir.0.join("home"))
        .output()
        .unwrap();

    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "new\n");
    assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "base\n");
    assert!(!dir.0.join("outside").exists());
}

#[test]
fn allow_write_needs_protect() {
    let out = run(&["--allow-write", "/tmp"], "true");
    assert_eq!(out.status.code(), Some(125));
    assert!(text(&out.stderr).contains("--protect"), "{}", text(&out.stderr));
}

#[test]
fn no_network_leaves_only_loopback() {
    if !available() {
        return;
    }
    // /proc/net/dev lists the interfaces of the reader's network namespace.
    let out = run(&["--no-network"], "tail -n +3 /proc/net/dev | cut -d: -f1 | tr -d ' '");

    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "lo\n");
}

#[test]
fn isolated_command_keeps_its_identity() {
    if !available() {
        return;
    }
    let out = run(&["--no-network", "--protect"], "id -u; id -g; pwd");

    assert!(out.status.success(), "{}", text(&out.stderr));
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(text(&out.stdout), format!("{uid}\n{gid}\n{}\n", cwd.display()));
}
