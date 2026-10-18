//! Resilience of commit (families B and C of the quality plan): crashes at
//! every step, failures half-way, special files, and concurrent commits.
//! Needs branches; skipped where they are unavailable unless
//! FOIL_REQUIRE_BRANCH is set. The crash tests need a debug build, where
//! FOIL_TEST_CRASH_AFTER=N makes foil kill itself after the Nth change.

mod common;

use std::os::unix::process::ExitStatusExt;
use std::path::Path;

use common::{branches_available, copy_tree, differences, snapshot, text, Env};

/// A workspace with a bit of everything.
const SETUP: &str = "mkdir -p src/deep docs old ro && echo a > a.txt && echo c > c.txt \
    && echo deep > src/deep/x.rs && echo doc > docs/readme && echo gone > old/f && echo r > ro/f && chmod 555 ro";

/// Edits touching every kind of change a commit applies: modify, add,
/// delete, replace a directory, a type change, a symlink, a FIFO, a hard
/// link, an xattr, modes, a long name and a read-only directory's content.
const EDITS: &str = "echo changed > a.txt && rm c.txt && echo new > src/deep/y.rs && mkdir -p new/sub \
    && echo n > new/sub/n && rm -rf old && mkdir old && echo again > old/g && rm docs/readme && mkdir docs/readme \
    && echo nested > docs/readme/inside && ln -s a.txt link && mkfifo pipe && echo shared > h1 && ln h1 h2 \
    && python3 -c \"import os; os.setxattr('a.txt', 'user.foil', b'yes')\" && chmod 600 src/deep/x.rs \
    && echo long > $(printf 'L%.0s' $(seq 1 250)) && chmod 755 ro && echo r2 > ro/f && echo r3 > ro/g && chmod 555 ro";

/// The tree a correct commit must produce: the edits applied to a plain copy.
fn expected(env: &Env) -> std::collections::BTreeMap<std::path::PathBuf, common::Node> {
    let reference = env.root.join("reference");
    copy_tree(&env.ws(), &reference);
    let out = Env::sh(&reference, EDITS);
    assert!(out.status.success(), "{}", text(&out.stderr));
    snapshot(&reference)
}

fn prepared(test: &str) -> Env {
    let env = Env::new(test);
    let out = Env::sh(&env.ws(), SETUP);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let out = env.run("t", EDITS);
    assert!(out.status.success(), "{}", text(&out.stderr));
    env
}

fn assert_no_leftovers(ws: &Path) {
    let leftovers = Env::sh(ws, "find . -name '.foil-commit*'");
    assert_eq!(text(&leftovers.stdout), "", "temporary files left in the workspace");
}

#[test]
fn a_commit_applies_every_kind_of_change_exactly() {
    if !branches_available() {
        return;
    }
    let env = prepared("exact");
    let want = expected(&env);
    let out = env.foil(&["commit", "t"]).output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let got = snapshot(&env.ws());
    assert!(got == want, "{}", differences(&got, &want).join("\n"));
    assert_no_leftovers(&env.ws());
}

#[test]
fn a_commit_killed_at_any_step_finishes_when_run_again() {
    if !branches_available() {
        return;
    }
    let mut crashed_at = 0;
    for step in 1.. {
        let env = prepared("crash");
        let want = expected(&env);
        let out = env.foil(&["commit", "t"]).env("FOIL_TEST_CRASH_AFTER", step.to_string()).output().unwrap();
        if out.status.success() {
            // Past the last change: the whole commit ran. Every step before was tested.
            assert!(crashed_at >= 10, "only {crashed_at} steps were tested");
            break;
        }
        assert_eq!(out.status.signal(), Some(libc::SIGKILL), "step {step}: {}", text(&out.stderr));
        crashed_at = step;

        // Half-applied: abort must not pretend the workspace is untouched.
        let out = env.foil(&["abort", "t"]).output().unwrap();
        assert!(!out.status.success(), "step {step}: abort accepted a half-applied commit");
        assert!(text(&out.stderr).contains("interrupted"), "{}", text(&out.stderr));

        // Running commit again finishes the same commit, conflicts or not.
        let out = env.foil(&["commit", "t"]).output().unwrap();
        assert!(out.status.success(), "step {step}: resume failed: {}", text(&out.stderr));
        let got = snapshot(&env.ws());
        assert!(got == want, "step {step}:\n{}", differences(&got, &want).join("\n"));
        assert_no_leftovers(&env.ws());
        assert_eq!(text(&env.foil(&["list"]).output().unwrap().stdout), "", "step {step}: branch left behind");
    }
}

#[test]
fn abort_force_keeps_a_half_applied_workspace_as_it_is() {
    if !branches_available() {
        return;
    }
    let env = prepared("abort-force");
    let out = env.foil(&["commit", "t"]).env("FOIL_TEST_CRASH_AFTER", "3").output().unwrap();
    assert_eq!(out.status.signal(), Some(libc::SIGKILL));
    let before = snapshot(&env.ws());
    let out = env.foil(&["abort", "--force", "t"]).output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(snapshot(&env.ws()) == before);
    assert_eq!(text(&env.foil(&["list"]).output().unwrap().stdout), "");
}

#[test]
fn a_failed_write_keeps_the_branch_and_a_retry_finishes() {
    if !branches_available() {
        return;
    }
    let env = Env::new("eacces");
    assert!(Env::sh(&env.ws(), "mkdir sub && echo old > sub/f && echo a > a").status.success());
    assert!(env.run("t", "echo new > sub/f && echo b > a && echo z > z").status.success());
    // Something outside foil's control blocks a write: a file where the
    // commit's temporary file must go, that can't be removed.
    assert!(Env::sh(&env.ws(), "mkdir sub/.foil-commit.tmp && touch sub/.foil-commit.tmp/x && chmod 500 sub/.foil-commit.tmp").status.success());
    let out = env.foil(&["commit", "t"]).output().unwrap();
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("stopped half-way"), "{}", text(&out.stderr));
    assert_eq!(text(&env.foil(&["list"]).output().unwrap().stdout).lines().count(), 1, "branch must be kept");

    assert!(Env::sh(&env.ws(), "chmod 700 sub/.foil-commit.tmp && rm -r sub/.foil-commit.tmp").status.success());
    let out = env.foil(&["commit", "t"]).output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(std::fs::read_to_string(env.ws().join("sub/f")).unwrap(), "new\n");
    assert_eq!(std::fs::read_to_string(env.ws().join("a")).unwrap(), "b\n");
    assert_eq!(std::fs::read_to_string(env.ws().join("z")).unwrap(), "z\n");
}

#[test]
fn a_socket_in_the_branch_is_refused_before_anything_changes() {
    if !branches_available() {
        return;
    }
    let env = Env::new("socket");
    assert!(Env::sh(&env.ws(), "echo a > a").status.success());
    let script = "echo b > a && python3 -c \"import socket; socket.socket(socket.AF_UNIX).bind('sock')\"";
    assert!(env.run("t", script).status.success());
    let before = snapshot(&env.ws());
    let out = env.foil(&["commit", "t"]).output().unwrap();
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("special file"), "{}", text(&out.stderr));
    assert!(snapshot(&env.ws()) == before, "a refused commit changed the workspace");
    // Nothing was started, so a plain abort is fine.
    assert!(env.foil(&["abort", "t"]).output().unwrap().status.success());
}

#[test]
fn concurrent_commits_to_one_file_let_exactly_one_win() {
    if !branches_available() {
        return;
    }
    let env = Env::new("race");
    assert!(Env::sh(&env.ws(), "echo base > x").status.success());
    const N: usize = 12;
    for i in 0..N {
        let out = env.run(&format!("b{i}"), &format!("echo {i} > x && echo {i} > own{i}"));
        assert!(out.status.success(), "{}", text(&out.stderr));
    }
    // All at once, from as many threads.
    let results: Vec<(usize, std::process::Output)> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..N)
            .map(|i| {
                let env = &env;
                s.spawn(move || (i, env.foil(&["commit", &format!("b{i}")]).output().unwrap()))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let winners: Vec<usize> = results.iter().filter(|(_, o)| o.status.success()).map(|(i, _)| *i).collect();
    assert_eq!(winners.len(), 1, "exactly one commit may change x: {winners:?}");
    let w = winners[0];
    assert_eq!(std::fs::read_to_string(env.ws().join("x")).unwrap(), format!("{w}\n"));
    for (i, out) in &results {
        if *i != w {
            assert!(text(&out.stderr).contains("also changed"), "b{i}: {}", text(&out.stderr));
            assert!(!env.ws().join(format!("own{i}")).exists(), "a refused commit applied a file");
        }
    }
    assert_eq!(text(&env.foil(&["list"]).output().unwrap().stdout).lines().count(), N - 1);
}

#[test]
fn concurrent_commits_of_disjoint_changes_all_land() {
    if !branches_available() {
        return;
    }
    let env = Env::new("disjoint");
    assert!(Env::sh(&env.ws(), "mkdir shared && echo base > shared/base").status.success());
    const N: usize = 16;
    for i in 0..N {
        let out = env.run(&format!("b{i}"), &format!("echo {i} > shared/f{i} && mkdir -p d{i} && echo {i} > d{i}/g"));
        assert!(out.status.success(), "{}", text(&out.stderr));
    }
    std::thread::scope(|s| {
        for i in 0..N {
            let env = &env;
            s.spawn(move || {
                let out = env.foil(&["commit", &format!("b{i}")]).output().unwrap();
                assert!(out.status.success(), "b{i}: {}", text(&out.stderr));
            });
        }
    });
    for i in 0..N {
        assert_eq!(std::fs::read_to_string(env.ws().join(format!("shared/f{i}"))).unwrap(), format!("{i}\n"));
        assert_eq!(std::fs::read_to_string(env.ws().join(format!("d{i}/g"))).unwrap(), format!("{i}\n"));
    }
    assert_eq!(text(&env.foil(&["list"]).output().unwrap().stdout), "");
}

/// A full disk half-way through a commit: the branch and its journal stay,
/// and once there is room again, `foil commit` finishes. Needs passwordless
/// sudo to mount a small tmpfs; skipped without it unless FOIL_REQUIRE_SUDO.
#[test]
fn a_full_disk_mid_commit_is_resumable() {
    if !branches_available() {
        return;
    }
    let sudo = |args: &[&str]| std::process::Command::new("sudo").arg("-n").args(args).output().unwrap();
    if !sudo(&["true"]).status.success() {
        assert!(std::env::var_os("FOIL_REQUIRE_SUDO").is_none(), "passwordless sudo required");
        eprintln!("skipped: needs passwordless sudo");
        return;
    }
    let env = Env::new("enospc");
    let ws = env.ws();
    let ids = format!("size=1m,uid={},gid={}", unsafe { libc::getuid() }, unsafe { libc::getgid() });
    assert!(sudo(&["mount", "-t", "tmpfs", "-o", &ids, "tmpfs", ws.to_str().unwrap()]).status.success());
    struct Unmount(std::path::PathBuf);
    impl Drop for Unmount {
        fn drop(&mut self) {
            let _ = std::process::Command::new("sudo").args(["-n", "umount", "-l"]).arg(&self.0).status();
        }
    }
    let _unmount = Unmount(ws.clone());

    assert!(Env::sh(&ws, "echo a > a && echo z > z").status.success());
    // In sorted order: a (small), big (2 MB, won't fit), z (small).
    assert!(env.run("t", "echo a2 > a && head -c 2000000 /dev/zero > big && echo z2 > z").status.success());
    let out = env.foil(&["commit", "t"]).output().unwrap();
    assert!(!out.status.success(), "a 2 MB file fit in 1 MB?");
    let stderr = text(&out.stderr);
    assert!(stderr.contains("stopped half-way") && stderr.contains("No space"), "{stderr}");
    assert_eq!(std::fs::read_to_string(ws.join("a")).unwrap(), "a2\n", "changes before the failure are applied");

    let room = ids.replace("size=1m", "size=8m");
    assert!(sudo(&["mount", "-o", &format!("remount,{room}"), ws.to_str().unwrap()]).status.success());
    let out = env.foil(&["commit", "t"]).output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(std::fs::metadata(ws.join("big")).unwrap().len(), 2_000_000);
    assert_eq!(std::fs::read_to_string(ws.join("z")).unwrap(), "z2\n");
    assert_no_leftovers(&ws);
}

/// Found by the differential test: a folder the command made unlistable
/// (or a file unreadable) hid the branch from `foil diff` and broke commit.
#[test]
fn folders_and_files_without_permissions_still_diff_and_commit() {
    if !branches_available() {
        return;
    }
    let env = Env::new("noperm");
    assert!(Env::sh(&env.ws(), "mkdir closed && echo a > closed/a && echo s > secret").status.success());
    // The xattr before the chmod: found by the differential test, commit
    // set the mode first and then could not write the xattr.
    let edits = "echo b > closed/b && chmod 000 closed && echo t > secret \
        && python3 -c \"import os; os.setxattr('secret', 'user.k', b'v')\" && chmod 000 secret \
        && mkdir -p new && echo n > new/n && chmod 311 new";
    assert!(env.run("t", edits).status.success());
    let reference = env.root.join("reference");
    copy_tree(&env.ws(), &reference);
    assert!(Env::sh(&reference, edits).status.success());

    let out = env.foil(&["diff", "t"]).output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let listed = text(&out.stdout);
    for path in ["closed/b", "secret", "new/n"] {
        assert!(listed.contains(path), "diff misses {path}:\n{listed}");
    }
    let out = env.foil(&["commit", "t"]).output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let (got, want) = (snapshot(&env.ws()), snapshot(&reference));
    assert!(got == want, "{}", differences(&got, &want).join("\n"));
}
