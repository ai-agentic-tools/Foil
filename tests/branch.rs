//! Tests for workspace branches. They need unprivileged user namespaces and
//! overlayfs in them (Linux >= 5.11). Where those are blocked the tests are
//! skipped, unless FOIL_REQUIRE_BRANCH is set, which turns a skip into a failure.
//! Workspaces live under the system temp dir: a shared folder mounted from
//! another machine (e.g. a VM's view of the host) may not support overlayfs.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// A fresh workspace and branch store for one test, removed on drop.
struct Env {
    root: PathBuf,
}

impl Env {
    fn new(test: &str) -> Env {
        // Tests run in parallel threads of one process; the counter keeps
        // every Env (including each probe) in its own directory.
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("foil-branch-{}-{n}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let ws = root.join("ws");
        std::fs::create_dir_all(ws.join("d")).unwrap();
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::write(ws.join("a.txt"), "base\n").unwrap();
        std::fs::write(ws.join("c.txt"), "doomed\n").unwrap();
        std::fs::write(ws.join("d/old"), "old\n").unwrap();
        Env { root: root.canonicalize().unwrap() }
    }

    fn ws(&self) -> PathBuf {
        self.root.join("ws")
    }

    /// `foil <args>`, run inside the workspace with this test's branch store.
    fn foil(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_foil"));
        cmd.args(args).current_dir(self.ws()).env("FOIL_HOME", self.root.join("home"));
        cmd
    }

    /// `foil run --branch <name> -- sh -c <script>`.
    fn run(&self, branch: &str, script: &str) -> Output {
        self.foil(&["run", "--branch", branch, "--", "sh", "-c", script]).output().unwrap()
    }

    fn read(&self, rel: &str) -> Option<String> {
        std::fs::read_to_string(self.ws().join(rel)).ok()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        // Overlayfs leaves work/work with mode 000; make it removable first.
        let _ = Command::new("chmod").args(["-R", "u+rwx"]).arg(&self.root).status();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Whether branches work here; skips (or fails, if required) otherwise.
fn branches_available() -> bool {
    let env = Env::new("probe");
    let out = env.run("probe", "true");
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

/// The edits most tests make: modify a.txt, add b.txt, delete c.txt.
const EDITS: &str = "echo changed > a.txt; echo new > b.txt; rm c.txt; cat a.txt b.txt";

#[test]
fn command_sees_its_changes_but_the_workspace_does_not() {
    if !branches_available() {
        return;
    }
    let env = Env::new("isolated");
    let out = env.run("t", EDITS);

    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "changed\nnew\n");
    assert_eq!(env.read("a.txt").as_deref(), Some("base\n"));
    assert_eq!(env.read("b.txt"), None);
    assert_eq!(env.read("c.txt").as_deref(), Some("doomed\n"));
}

#[test]
fn diff_lists_added_modified_and_deleted_files() {
    if !branches_available() {
        return;
    }
    let env = Env::new("diff");
    assert!(env.run("t", EDITS).status.success());
    let out = env.foil(&["diff", "t"]).output().unwrap();

    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "M a.txt\nA b.txt\nD c.txt\n");
}

#[test]
fn commit_applies_the_changes_and_removes_the_branch() {
    if !branches_available() {
        return;
    }
    let env = Env::new("commit");
    assert!(env.run("t", EDITS).status.success());
    let out = env.foil(&["commit", "t"]).output().unwrap();

    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(env.read("a.txt").as_deref(), Some("changed\n"));
    assert_eq!(env.read("b.txt").as_deref(), Some("new\n"));
    assert_eq!(env.read("c.txt"), None);
    let list = env.foil(&["list"]).output().unwrap();
    assert_eq!(text(&list.stdout), "", "branch survived its commit");
}

#[test]
fn commit_counts_changes_not_the_folders_holding_them() {
    if !branches_available() {
        return;
    }
    // Editing deep/er/f.txt copies deep/ and deep/er/ up into the branch;
    // only the file changed. A new folder does count.
    let env = Env::new("count");
    std::fs::create_dir_all(env.ws().join("deep/er")).unwrap();
    std::fs::write(env.ws().join("deep/er/f.txt"), "old\n").unwrap();
    assert!(env.run("t", "echo new > deep/er/f.txt; mkdir fresh").status.success());
    let out = env.foil(&["commit", "t"]).output().unwrap();

    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "applied 2 changes\n");
    assert_eq!(env.read("deep/er/f.txt").as_deref(), Some("new\n"));
}

#[test]
fn abort_discards_the_changes() {
    if !branches_available() {
        return;
    }
    let env = Env::new("abort");
    assert!(env.run("t", EDITS).status.success());
    let out = env.foil(&["abort", "t"]).output().unwrap();

    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(env.read("a.txt").as_deref(), Some("base\n"));
    assert_eq!(env.read("b.txt"), None);
    assert!(!env.root.join("home/branches/t").exists(), "branch files left behind");
}

#[test]
fn command_keeps_its_path_and_identity() {
    if !branches_available() {
        return;
    }
    let env = Env::new("identity");
    let out = env.run("t", "pwd; id -u; id -g");

    assert!(out.status.success(), "{}", text(&out.stderr));
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    assert_eq!(text(&out.stdout), format!("{}\n{uid}\n{gid}\n", env.ws().display()));
}

#[test]
fn a_branch_continues_across_runs() {
    if !branches_available() {
        return;
    }
    let env = Env::new("continue");
    assert!(env.run("t", "echo first > notes").status.success());
    let out = env.run("t", "echo second >> notes; cat notes");

    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "first\nsecond\n");
    assert_eq!(env.read("notes"), None);
}

#[test]
fn branches_do_not_see_each_other() {
    if !branches_available() {
        return;
    }
    let env = Env::new("separate");
    assert!(env.run("one", "echo 1 > only-in-one").status.success());
    let out = env.run("two", "test -e only-in-one");

    assert_eq!(out.status.code(), Some(1), "branch two saw branch one's file");
}

#[test]
fn a_replaced_directory_commits_with_only_its_new_contents() {
    if !branches_available() {
        return;
    }
    let env = Env::new("replaced");
    assert!(env.run("t", "rm -rf d && mkdir d && echo n > d/new").status.success());
    let diff = env.foil(&["diff", "t"]).output().unwrap();
    assert_eq!(text(&diff.stdout), "R d\nA d/new\n");

    assert!(env.foil(&["commit", "t"]).status().unwrap().success());
    assert_eq!(env.read("d/new").as_deref(), Some("n\n"));
    assert_eq!(env.read("d/old"), None, "old contents survived the commit");
}

#[test]
fn storage_inside_the_workspace_is_refused() {
    let env = Env::new("nested");
    let out = env
        .foil(&["run", "--branch", "t", "--", "true"])
        .env("FOIL_HOME", env.ws().join(".foil"))
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(125));
    assert!(text(&out.stderr).contains("inside the workspace"), "{}", text(&out.stderr));
}

#[test]
fn a_branch_in_use_cannot_be_used_twice() {
    if !branches_available() {
        return;
    }
    let env = Env::new("busy");
    let mut first = env
        .foil(&["run", "--branch", "t", "--", "sleep", "2"])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let second = env.run("t", "true");
    let abort = env.foil(&["abort", "t"]).output().unwrap();
    first.wait().unwrap();

    assert_eq!(second.status.code(), Some(125));
    assert!(text(&second.stderr).contains("in use"), "{}", text(&second.stderr));
    assert_eq!(abort.status.code(), Some(1), "abort removed a branch in use");
}

#[test]
fn bad_branch_names_are_rejected() {
    let env = Env::new("names");
    for name in ["", ".hidden", "a/b", "../escape"] {
        let out = env.foil(&["run", "--branch", name, "--", "true"]).output().unwrap();
        assert_eq!(out.status.code(), Some(125), "name {name:?} was accepted");
    }
}

#[test]
fn unknown_branch_is_an_error() {
    let env = Env::new("unknown");
    let out = env.foil(&["diff", "nope"]).output().unwrap();

    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("no branch named nope"));
}

#[test]
fn diff_shows_a_patch_and_json() {
    if !branches_available() {
        return;
    }
    let env = Env::new("patch");
    assert!(env.run("t", EDITS).status.success());
    let patch = text(&env.foil(&["diff", "--patch", "t"]).output().unwrap().stdout);
    let json = text(&env.foil(&["diff", "--json", "t"]).output().unwrap().stdout);

    for line in ["--- a/a.txt", "+++ b/a.txt", "-base", "+changed", "+new", "-doomed"] {
        assert!(patch.lines().any(|l| l == line), "missing {line:?} in:\n{patch}");
    }
    assert_eq!(
        json,
        "[{\"kind\":\"M\",\"path\":\"a.txt\"},{\"kind\":\"A\",\"path\":\"b.txt\"},{\"kind\":\"D\",\"path\":\"c.txt\"}]\n"
    );
}

#[test]
fn commit_refuses_files_changed_in_the_workspace_since_unless_forced() {
    if !branches_available() {
        return;
    }
    let env = Env::new("conflict");
    assert!(env.run("t", EDITS).status.success());
    // Kernel timestamps are coarse (a few ms); make "later" unambiguous.
    std::thread::sleep(Duration::from_millis(50));
    std::fs::write(env.ws().join("a.txt"), "outside\n").unwrap();
    std::fs::write(env.ws().join("d/old"), "untouched by the branch\n").unwrap();

    let refused = env.foil(&["commit", "t"]).output().unwrap();
    assert_eq!(refused.status.code(), Some(1));
    let stderr = text(&refused.stderr);
    assert!(stderr.contains("  a.txt") && !stderr.contains("d/old"), "{stderr}");
    assert_eq!(env.read("a.txt").as_deref(), Some("outside\n"));
    assert_eq!(env.read("b.txt"), None, "a refused commit applied something");

    let forced = env.foil(&["commit", "--force", "t"]).output().unwrap();
    assert!(forced.status.success(), "{}", text(&forced.stderr));
    assert_eq!(env.read("a.txt").as_deref(), Some("changed\n"));
}

#[test]
fn leftovers_of_a_killed_foil_are_stopped_before_the_branch_is_committed() {
    if !branches_available() {
        return;
    }
    let env = Env::new("stale");
    let pid_file = env.root.join("writer.pid");
    // A writer in the background, still appending to the branch after the
    // shell that started it (and foil) are gone.
    let script = format!(
        "(while :; do echo tick >> log; sleep 0.02; done) & echo $! > {}; wait",
        pid_file.display()
    );
    let mut foil = env
        .foil(&["run", "--branch", "t", "--", "sh", "-c", &script])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let writer: i32 = loop {
        if let Some(pid) = std::fs::read_to_string(&pid_file).ok().and_then(|p| p.trim().parse().ok()) {
            break pid;
        }
        assert!(std::time::Instant::now() < deadline, "writer never started");
        std::thread::sleep(Duration::from_millis(10));
    };
    // If the test fails, don't leave the writer holding the test's stderr open.
    struct KillOnDrop(i32);
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            // Only while it is still our writer: its PID may since have been
            // reused. In a foil box, or (without one) still running our loop.
            let cgroup = std::fs::read_to_string(format!("/proc/{}/cgroup", self.0)).unwrap_or_default();
            let cmdline = std::fs::read(format!("/proc/{}/cmdline", self.0)).unwrap_or_default();
            let ours = cgroup.contains("/foil-") || String::from_utf8_lossy(&cmdline).contains("echo tick");
            if ours {
                unsafe { libc::kill(self.0, libc::SIGKILL) };
            }
        }
    }
    let _guard = KillOnDrop(writer);
    let boxed = std::fs::read_to_string(format!("/proc/{writer}/cgroup")).is_ok_and(|c| c.contains("/foil-"));
    foil.kill().unwrap(); // SIGKILL: foil gets no chance to clean up
    foil.wait().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let alive = |pid: i32| {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .is_ok_and(|stat| !stat.rsplit(')').next().unwrap_or("").trim_start().starts_with('Z'))
    };
    assert!(alive(writer), "the writer should have outlived foil");
    if !boxed {
        assert!(std::env::var_os("FOIL_REQUIRE_CGROUP").is_none(), "run was not boxed");
        eprintln!("skipped: no cleanup box, so leftovers of a killed foil cannot be found");
        return;
    }

    let out = env.foil(&["commit", "t"]).output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(!alive(writer), "the leftover writer survived the commit");
    let committed = env.read("log").unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(env.read("log").unwrap(), committed, "something still writes to the workspace");
}

#[test]
fn ignored_build_output_is_left_out_of_diff_and_commit_unless_all() {
    if !branches_available() {
        return;
    }
    let env = Env::new("ignored");
    let git = |args: &[&str]| {
        let ok = Command::new("git").args(args).current_dir(env.ws()).stdout(Stdio::null()).status();
        ok.is_ok_and(|s| s.success())
    };
    if !git(&["init", "-q"]) {
        eprintln!("skipped: git not available");
        return;
    }
    std::fs::write(env.ws().join(".gitignore"), "build/\n*.pyc\n").unwrap();
    assert!(git(&["add", "-A"]));
    let edits = "echo changed > a.txt; mkdir build; echo obj > build/out.o; echo x > a.pyc";
    assert!(env.run("t", edits).status.success());

    let diff = env.foil(&["diff", "t"]).output().unwrap();
    assert_eq!(text(&diff.stdout), "M a.txt\n");
    assert!(text(&diff.stderr).contains("3 paths ignored"), "{}", text(&diff.stderr));
    let all = env.foil(&["diff", "--all", "t"]).output().unwrap();
    assert_eq!(text(&all.stdout), "A a.pyc\nM a.txt\nA build\nA build/out.o\n");

    assert!(env.foil(&["commit", "t"]).status().unwrap().success());
    assert_eq!(env.read("a.txt").as_deref(), Some("changed\n"));
    assert!(!env.ws().join("build").exists() && !env.ws().join("a.pyc").exists(), "build output was committed");
}
