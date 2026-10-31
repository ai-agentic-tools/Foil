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
const EDITS_BEFORE: &str = "echo changed > a.txt && rm c.txt && echo new > src/deep/y.rs && mkdir -p new/sub \
    && echo n > new/sub/n && rm -rf old && mkdir old && echo again > old/g && rm docs/readme && mkdir docs/readme \
    && echo nested > docs/readme/inside && ln -s a.txt link && mkfifo pipe && echo shared > h1 && ln h1 h2";
const EDITS_AFTER: &str = "chmod 600 src/deep/x.rs \
    && echo long > $(printf 'L%.0s' $(seq 1 250)) && chmod 755 ro && echo r2 > ro/f && echo r3 > ro/g && chmod 555 ro";

/// The edits, an extended attribute included.
fn edits() -> String {
    format!("{EDITS_BEFORE} && {} && {EDITS_AFTER}", common::setx("a.txt", "user.foil", "yes"))
}

/// The tree a correct commit must produce: the edits applied to a plain copy.
fn expected(env: &Env) -> std::collections::BTreeMap<std::path::PathBuf, common::Node> {
    let reference = env.root.join("reference");
    copy_tree(&env.ws(), &reference);
    let out = Env::sh(&reference, &edits());
    assert!(out.status.success(), "{}", text(&out.stderr));
    snapshot(&reference)
}

fn prepared(test: &str) -> Env {
    let env = Env::new(test);
    let out = Env::sh(&env.ws(), SETUP);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let out = env.run("t", &edits());
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
    let env = Env::new("eio");
    assert!(Env::sh(&env.ws(), "mkdir sub && echo old > sub/f && echo a > a").status.success());
    assert!(env.run("t", "echo new > sub/f && echo b > a && echo z > z").status.success());
    // The disk fails on the second change (debug builds: FOIL_TEST_FAIL_AFTER).
    let out = env.foil(&["commit", "t"]).env("FOIL_TEST_FAIL_AFTER", "2").output().unwrap();
    assert!(!out.status.success());
    let stderr = text(&out.stderr);
    assert!(stderr.contains("stopped half-way") && stderr.contains("Input/output error"), "{stderr}");
    assert_eq!(text(&env.foil(&["list"]).output().unwrap().stdout).lines().count(), 1, "branch must be kept");

    let out = env.foil(&["commit", "t"]).output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(std::fs::read_to_string(env.ws().join("sub/f")).unwrap(), "new\n");
    assert_eq!(std::fs::read_to_string(env.ws().join("a")).unwrap(), "b\n");
    assert_eq!(std::fs::read_to_string(env.ws().join("z")).unwrap(), "z\n");
    assert_no_leftovers(&env.ws());
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
#[cfg(target_os = "linux")] // a tmpfs mount; Linux only
fn a_full_disk_mid_commit_is_resumable() {
    if !branches_available() {
        return;
    }
    let sudo = |args: &[&str]| std::process::Command::new("sudo").arg("-n").args(args).output().unwrap();
    // No sudo at all (a container) counts as no passwordless sudo.
    let has_sudo = std::process::Command::new("sudo").args(["-n", "true"]).output().is_ok_and(|o| o.status.success());
    if !has_sudo {
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
    let edits = format!(
        "echo b > closed/b && chmod 000 closed && echo t > secret && {} && chmod 000 secret \
         && mkdir -p new && echo n > new/n && chmod 311 new",
        common::setx("secret", "user.k", "v")
    );
    let edits = edits.as_str();
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

/// Found by CI: a workspace inside a folder an enclosing git repository
/// ignores (here `ws/`) had every change dropped by commit, silently.
#[test]
fn a_workspace_inside_an_ignored_folder_still_commits() {
    if !branches_available() {
        return;
    }
    let env = Env::new("ignored-ws");
    assert!(Env::sh(&env.root, "git init -q . && printf 'ws/\\nhome/\\n' > .gitignore").status.success());
    assert!(Env::sh(&env.ws(), "echo base > a.txt").status.success());
    assert!(env.run("t", "echo changed > a.txt && echo new > b.txt").status.success());
    let out = env.foil(&["commit", "t"]).output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(std::fs::read_to_string(env.ws().join("a.txt")).unwrap(), "changed\n");
    assert_eq!(std::fs::read_to_string(env.ws().join("b.txt")).unwrap(), "new\n");
}

/// A workspace change made right after the branch was created must still
/// count as a conflict. Timestamps can lag by a clock tick (coarse
/// timestamps), so this changes the file the moment the branch exists, many
/// times over, to land inside that window.
#[test]
fn a_change_right_after_the_branch_is_made_is_still_a_conflict() {
    if !branches_available() {
        return;
    }
    let mut missed = Vec::new();
    for i in 0..40 {
        let env = Env::new("tick");
        std::fs::write(env.ws().join("x"), "base\n").unwrap();
        let mut run = env.foil(&["run", "-b", "t", "--", "sh", "-c", "sleep 0.2; echo branch > x"]).spawn().unwrap();
        let marker = env.root.join("home/branches/t/workspace");
        while !marker.exists() {
            std::hint::spin_loop();
        }
        std::fs::write(env.ws().join("x"), "outside\n").unwrap();
        assert!(run.wait().unwrap().success());
        let out = env.foil(&["commit", "t"]).output().unwrap();
        if out.status.success() {
            missed.push(i);
        }
    }
    assert!(missed.is_empty(), "the outside change was overwritten without a conflict in rounds {missed:?}");
}

/// Found by the differential test: a folder whose only change was its own
/// extended attribute was committed but not shown by `foil diff`.
#[test]
fn a_folder_metadata_change_is_shown_by_diff() {
    if !branches_available() {
        return;
    }
    let env = Env::new("dir-meta");
    assert!(Env::sh(&env.ws(), "mkdir d && echo x > d/f && mkdir m").status.success());
    let edits = format!("{} && chmod 700 m", common::setx("d", "user.k", "v"));
    let edits = edits.as_str();
    assert!(env.run("t", edits).status.success());
    let listed = text(&env.foil(&["diff", "t"]).output().unwrap().stdout);
    assert!(listed.lines().any(|l| l == "M d"), "{listed}");
    assert!(listed.lines().any(|l| l == "M m"), "{listed}");
}

/// Found by the differential test: two names of one read-only file came out
/// writable after commit (the real mode was remembered per name).
#[test]
fn hard_links_keep_their_mode_through_commit() {
    if !branches_available() {
        return;
    }
    let env = Env::new("link-mode");
    assert!(Env::sh(&env.ws(), "mkdir sub").status.success());
    let edits = "echo v > b && ln b sub/b && chmod 444 b";
    assert!(env.run("t", edits).status.success());
    let reference = env.root.join("reference");
    copy_tree(&env.ws(), &reference);
    assert!(Env::sh(&reference, edits).status.success());
    let out = env.foil(&["commit", "t"]).output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let (got, want) = (snapshot(&env.ws()), snapshot(&reference));
    assert!(got == want, "{}", differences(&got, &want).join("\n"));
}

/// A folder swapped for a symlink in the middle of a commit must not send
/// the commit's remaining writes outside the workspace (the commit works
/// from open folders and never follows symlinks; debug builds can pause a
/// commit with FOIL_TEST_PAUSE_AFTER / FOIL_TEST_PAUSE_FILE).
#[test]
fn a_folder_swapped_for_a_symlink_mid_commit_cannot_redirect_writes() {
    if !branches_available() {
        return;
    }
    let env = Env::new("swap");
    let outside = env.root.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    assert!(Env::sh(&env.ws(), "mkdir d && echo keep > d/keep").status.success());
    assert!(env.run("t", "echo 1 > d/x1 && echo 2 > d/x2 && echo 3 > d/x3").status.success());

    let go = env.root.join("go");
    let reached = go.with_extension("reached");
    let mut commit = env
        .foil(&["commit", "t"])
        .env("FOIL_TEST_PAUSE_AFTER", "1")
        .env("FOIL_TEST_PAUSE_FILE", &go)
        .spawn()
        .unwrap();
    while !reached.exists() {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    // Mid-commit: the real folder moves away and a symlink to `outside` takes its place.
    std::fs::rename(env.ws().join("d"), env.ws().join("d.moved")).unwrap();
    std::os::unix::fs::symlink(&outside, env.ws().join("d")).unwrap();
    std::fs::write(&go, b"").unwrap();
    let _ = commit.wait();

    let leaked: Vec<_> = std::fs::read_dir(&outside).unwrap().flatten().map(|e| e.file_name()).collect();
    assert!(leaked.is_empty(), "the commit wrote outside the workspace: {leaked:?}");
    // It went on writing into the real folder it had open, wherever that moved.
    for f in ["x2", "x3"] {
        assert!(env.ws().join("d.moved").join(f).exists(), "{f} was not written");
    }
}

/// `foil diff` while a run still uses the branch must not change the modes
/// of folders the running command closed; it explains instead, and works
/// once the run has ended.
#[test]
fn diff_of_a_branch_in_use_leaves_closed_folders_alone() {
    if !branches_available() {
        return;
    }
    let env = Env::new("diff-busy");
    let ready = env.root.join("ready");
    let script = format!("mkdir closed && echo x > closed/f && chmod 000 closed && touch {} && sleep 2", ready.display());
    let mut run = env.foil(&["run", "-b", "t", "--", "sh", "-c", &script]).spawn().unwrap();
    while !ready.exists() {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // Where the branch keeps the folder: overlayfs's upper layer, or the macOS clone.
    let upper = env.root.join(if cfg!(target_os = "macos") { "home/branches/t/tree/closed" } else { "home/branches/t/upper/closed" });
    let mode = |p: &std::path::Path| std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(p).unwrap().permissions()) & 0o7777;
    let out = env.foil(&["diff", "t"]).output().unwrap();
    assert!(!out.status.success(), "{}", text(&out.stdout));
    assert!(text(&out.stderr).contains("is in use"), "{}", text(&out.stderr));
    assert_eq!(mode(&upper), 0, "diff changed a folder the running command closed");
    assert!(run.wait().unwrap().success());
    let out = env.foil(&["diff", "t"]).output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("closed/f"), "{}", text(&out.stdout));
    assert_eq!(mode(&upper), 0, "diff left the folder open");
}
