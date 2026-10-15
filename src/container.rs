//! Running foil inside a container: find out what blocks it, and say how
//! to fix each blocker.
//!
//! Container runtimes switch off, by default, the kernel features foil is
//! built on. Verified with Docker 29 on Linux 7.0 (see contrib/docker):
//!
//! - seccomp: Docker's default filter refuses new namespaces;
//! - AppArmor: Docker's `docker-default` profile refuses mounts;
//! - masked /proc: Docker covers parts of /proc (`/proc/kcore`, `/proc/keys`,
//!   ...), and the kernel then refuses a fresh /proc inside a new user
//!   namespace, which `--strict` needs (even with `subset=pid`);
//! - the container's own filesystem is overlayfs, which overlayfs can't use
//!   as a branch's writable layer: the branch store (FOIL_HOME) must be on
//!   a volume or tmpfs;
//! - an Ubuntu host's user-namespace restriction applies inside containers
//!   too: the host needs foil's AppArmor profile for the path foil has
//!   inside the container (e.g. `sudo foil setup` on the host, with foil at
//!   the same path), or the restriction lifted.
//!
//! - a container's first process is started explicitly "unconfined" by
//!   the runtime, which overrides the host's foil profile; foil re-executes
//!   itself once to get it back (see `regain_apparmor_profile`).
//!
//! Not a blocker: the cgroup box (the cgroup filesystem is read-only in a
//! container). Strict runs still end everything they started, because the
//! kernel kills a PID namespace's processes when its first process exits.

use std::fs;
use std::path::Path;

const OVERLAYFS_SUPER_MAGIC: i64 = 0x794c_7630;

/// The container runtime foil runs under, if any.
pub fn runtime() -> Option<&'static str> {
    if Path::new("/.dockerenv").exists() {
        return Some("Docker");
    }
    if Path::new("/run/.containerenv").exists() {
        return Some("Podman");
    }
    if std::env::var_os("container").is_some() {
        return Some("a container");
    }
    let cgroup = fs::read_to_string("/proc/1/cgroup").unwrap_or_default();
    ["docker", "kubepods", "containerd", "lxc"]
        .iter()
        .any(|k| cgroup.contains(k))
        .then_some("a container")
}

const RESTRICT: &str = "/proc/sys/kernel/apparmor_restrict_unprivileged_userns";
const REEXEC: &str = "FOIL_AA_REEXEC";

fn apparmor_label() -> String {
    fs::read_to_string("/proc/self/attr/apparmor/current")
        .or_else(|_| fs::read_to_string("/proc/self/attr/current"))
        .map(|s| s.trim().to_owned())
        .unwrap_or_default()
}

/// On a host that restricts user namespaces (Ubuntu), a process the
/// container runtime started directly runs "unconfined", even if the host
/// has a foil profile for this path: the runtime overrides the profile for
/// that one exec. A plain exec of ourselves lets the profile attach again.
/// Called first thing in main; returns only if nothing needs doing.
pub fn regain_apparmor_profile() {
    if std::env::var_os(REEXEC).is_some() {
        std::env::remove_var(REEXEC);
        return;
    }
    let restricted = fs::read_to_string(RESTRICT).is_ok_and(|v| v.trim() == "1");
    if !restricted || apparmor_label() != "unconfined" {
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    use std::os::unix::process::CommandExt;
    let _ = std::process::Command::new(exe).args(std::env::args_os().skip(1)).env(REEXEC, "1").exec();
}

/// Whether a child of ours may create a user namespace (seccomp aside, the
/// first thing foil needs). Returns the errno on failure.
fn try_userns() -> Result<(), i32> {
    // SAFETY: the child only calls unshare and _exit.
    unsafe {
        let pid = libc::fork();
        if pid == 0 {
            let rc = libc::unshare(libc::CLONE_NEWUSER);
            let err = if rc == -1 { *libc::__errno_location() } else { 0 };
            libc::_exit(err.min(255));
        }
        if pid == -1 {
            return Err(-1);
        }
        let mut status = 0;
        libc::waitpid(pid, &mut status, 0);
        match libc::WEXITSTATUS(status) {
            0 => Ok(()),
            err => Err(err),
        }
    }
}

/// One thing standing in foil's way, and how to remove it.
pub struct Blocker {
    pub what: &'static str,
    pub blocks: &'static str,
    pub fix: &'static str,
}

/// Every blocker found in this container (empty outside containers).
pub fn blockers(foil_home: &Path) -> Vec<Blocker> {
    if runtime().is_none() {
        return Vec::new();
    }
    let mut found = Vec::new();
    let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
    let filtered = status.lines().any(|l| l.split_whitespace().collect::<Vec<_>>() == ["Seccomp:", "2"]);
    if filtered && try_userns() == Err(libc::EPERM) {
        found.push(Blocker {
            what: "seccomp filter",
            blocks: "everything (new namespaces are refused)",
            fix: "--security-opt seccomp=contrib/docker/foil-seccomp.json (or seccomp=unconfined)",
        });
    }
    let apparmor = apparmor_label();
    if apparmor.starts_with("docker-default") || apparmor.contains("containers-default") {
        found.push(Blocker {
            what: "AppArmor container profile",
            blocks: "branches, --protect, --strict (mounts are refused)",
            fix: "--security-opt apparmor=unconfined",
        });
    }
    if proc_masked() {
        found.push(Blocker {
            what: "masked /proc",
            blocks: "--strict (a fresh /proc is refused)",
            fix: "--security-opt systempaths=unconfined (Kubernetes: procMount: Unmasked)",
        });
    }
    if on_overlayfs(foil_home) {
        found.push(Blocker {
            what: "branch store on the container's overlay filesystem",
            blocks: "branches (overlayfs can't stack on it)",
            fix: "a volume or tmpfs for FOIL_HOME, e.g. --tmpfs /foil -e FOIL_HOME=/foil",
        });
    }
    let restricted = fs::read_to_string(RESTRICT).is_ok_and(|v| v.trim() == "1");
    if restricted && !apparmor.starts_with("foil.") {
        found.push(Blocker {
            what: "the host's user-namespace restriction (Ubuntu)",
            blocks: "everything, unless the host trusts this foil",
            fix: "on the host: sudo foil setup with foil installed at the same path as in the container",
        });
    }
    found
}

/// Whether something is mounted over parts of /proc (container masking).
fn proc_masked() -> bool {
    fs::read_to_string("/proc/self/mountinfo").is_ok_and(|info| {
        info.lines().any(|line| line.split_whitespace().nth(4).is_some_and(|p| p.starts_with("/proc/")))
    })
}

/// Whether `path` (or its nearest existing ancestor) is on overlayfs.
// f_type's integer type differs between architectures, hence the cast below.
#[allow(clippy::unnecessary_cast)]
fn on_overlayfs(path: &Path) -> bool {
    let Some(existing) = path.ancestors().find(|a| a.exists()) else { return false };
    let Ok(c) = std::ffi::CString::new(existing.as_os_str().as_encoded_bytes()) else { return false };
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: statfs fills `stat` completely when it returns 0.
    unsafe { libc::statfs(c.as_ptr(), stat.as_mut_ptr()) == 0 && stat.assume_init().f_type as i64 == OVERLAYFS_SUPER_MAGIC }
}
