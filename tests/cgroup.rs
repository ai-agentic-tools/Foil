//! Tests for the per-run cgroup box. They need a delegated cgroup, e.g.
//! `systemd-run --user --scope cargo test`. Without one they are skipped,
//! unless FOIL_REQUIRE_CGROUP is set, which turns a missing box into a failure.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

fn foil() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_foil"));
    cmd.args(["run", "--"]);
    cmd
}

/// The foil box path in `/proc/self/cgroup` output, if the command was boxed.
fn boxed_cgroup(proc_self_cgroup: &str) -> Option<String> {
    proc_self_cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .filter(|path| path.contains("/foil-"))
        .map(str::to_owned)
}

/// Whether foil can create boxes here; skips (or fails, if required) otherwise.
fn boxes_available() -> bool {
    let out = foil().args(["cat", "/proc/self/cgroup"]).output().unwrap();
    if boxed_cgroup(&String::from_utf8_lossy(&out.stdout)).is_some() {
        return true;
    }
    assert!(
        std::env::var_os("FOIL_REQUIRE_CGROUP").is_none(),
        "FOIL_REQUIRE_CGROUP is set but the command was not put in a foil cgroup"
    );
    eprintln!("skipped: no delegated cgroup (run under `systemd-run --user --scope`)");
    false
}

#[test]
fn command_runs_in_its_own_cgroup_which_is_removed_after() {
    if !boxes_available() {
        return;
    }
    let out = foil().args(["cat", "/proc/self/cgroup"]).output().unwrap();
    assert!(out.status.success());
    let cg = boxed_cgroup(&String::from_utf8_lossy(&out.stdout)).unwrap();
    assert!(!Path::new(&format!("/sys/fs/cgroup{cg}")).exists(), "box {cg} was left behind");
}

#[test]
fn kills_setuid_descendants_foil_cannot_signal() {
    if !boxes_available() {
        return;
    }
    if !Command::new("sudo").args(["-n", "true"]).status().is_ok_and(|s| s.success()) {
        eprintln!("skipped: needs passwordless sudo");
        return;
    }
    // A root-owned `sleep` in its own session: an unprivileged foil gets EPERM
    // trying to kill it, and no hangup reaches it. The script exits only once
    // the sleep is running as root, and prints its PID. The PID file lives in
    // a private directory because protected_regular stops even root from
    // writing another user's file in /tmp. The wait gives up after ~3 s.
    let script = r#"d=$(mktemp -d)
sudo -n setsid sh -c "echo \$\$ > $d/pid; exec sleep 30" &
for i in $(seq 300); do [ -s "$d/pid" ] && break; sleep 0.01; done
cat "$d/pid"; rm -rf "$d"
exit 0"#;
    let start = Instant::now();
    // output() reads to EOF, so it blocks while the sleep holds the pipe.
    let out = foil().args(["sh", "-c", script]).output().unwrap();

    assert!(out.status.success());
    assert!(start.elapsed() < Duration::from_secs(5), "root-owned sleep survived foil");
    let pid = String::from_utf8(out.stdout).unwrap();
    assert!(!Path::new(&format!("/proc/{}", pid.trim())).exists(), "pid {} leaked", pid.trim());
}

#[test]
fn removes_cgroups_the_command_nested_inside_its_box() {
    if !boxes_available() {
        return;
    }
    let script = r#"cg=/sys/fs/cgroup$(sed -n 's/^0:://p' /proc/self/cgroup)
mkdir "$cg/inner"
sh -c "echo 0 > $cg/inner/cgroup.procs; exec sleep 30" &
echo "$cg"
exit 0"#;
    let start = Instant::now();
    let out = foil().args(["sh", "-c", script]).output().unwrap();

    assert!(out.status.success());
    assert!(start.elapsed() < Duration::from_secs(5), "nested sleep survived foil");
    let cg = String::from_utf8(out.stdout).unwrap();
    assert!(!Path::new(cg.trim()).exists(), "box {} was left behind", cg.trim());
}

#[test]
fn foil_no_cgroup_disables_the_box() {
    let out = foil()
        .env("FOIL_NO_CGROUP", "1")
        .args(["cat", "/proc/self/cgroup"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(boxed_cgroup(&String::from_utf8_lossy(&out.stdout)), None);
}
