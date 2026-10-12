//! A per-run cgroup v2 "box" that holds the command and everything it starts.
//!
//! foil creates `<its own cgroup>/foil-<pid>`, and the child moves itself in
//! before exec, so every descendant is born inside. Nothing leaves the box by
//! forking, `setsid` or running a setuid program, and one write to
//! `cgroup.kill` stops all of it, including processes foil may not signal.
//!
//! This needs a delegated cgroup: foil's own cgroup must be writable by the
//! user, as inside a systemd user scope (`systemd-run --user --scope`). When
//! it is not, `Cgroup::create` returns `None` and foil falls back to
//! subreaper-only cleanup. `FOIL_NO_CGROUP=1` forces the fallback.
//!
//! A foil that is SIGKILLed cannot clean up: its command dies with it (see
//! PR_SET_PDEATHSIG in `supervisor.rs`), but what the command started lives
//! on in the box. Such stale boxes are reaped by the next foil that creates a
//! box next to them, or that takes the lock of the branch they ran in.

use std::ffi::CString;
use std::io;
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

const CGROUP_ROOT: &str = "/sys/fs/cgroup";
/// `statfs` magic number of a cgroup v2 filesystem.
const CGROUP2_SUPER_MAGIC: i64 = 0x6367_7270;

pub struct Cgroup {
    dir: PathBuf,
    /// `<dir>/cgroup.procs`, built up front because the child may not allocate.
    procs: CString,
    killed: bool,
}

impl Cgroup {
    /// Create a fresh box under foil's own cgroup, or `None` if that is not possible.
    pub fn create() -> Option<Cgroup> {
        if std::env::var_os("FOIL_NO_CGROUP").is_some() {
            return None;
        }
        if !can_box() {
            return None;
        }
        let dir = box_path()?;
        let parent = dir.parent()?;
        reap_stale_siblings(parent);
        if let Err(err) = std::fs::create_dir(&dir) {
            if err.kind() != io::ErrorKind::AlreadyExists {
                return None;
            }
            // Left behind by an earlier foil that had our PID and was SIGKILLed.
            kill_and_remove(&dir).ok()?;
            std::fs::create_dir(&dir).ok()?;
        }
        let procs = CString::new(dir.join("cgroup.procs").as_os_str().as_bytes()).ok()?;
        Some(Cgroup { dir, procs, killed: false })
    }

    /// The path the child passes to `enter`.
    pub fn entry(&self) -> CString {
        self.procs.clone()
    }

    /// Whether `pid` is inside this box or a cgroup nested inside it.
    pub fn contains(&self, pid: u32) -> bool {
        let Ok(rel) = self.dir.strip_prefix(CGROUP_ROOT) else {
            return false;
        };
        let want = Path::new("/").join(rel);
        std::fs::read_to_string(format!("/proc/{pid}/cgroup")).is_ok_and(|text| {
            text.lines()
                .any(|line| line.strip_prefix("0::").is_some_and(|p| Path::new(p).starts_with(&want)))
        })
    }

    /// Whether any live process is left in the box or its nested cgroups.
    /// A process stops counting before its parent gets SIGCHLD for it.
    pub fn is_populated(&self) -> bool {
        std::fs::read_to_string(self.dir.join("cgroup.events"))
            .is_ok_and(|text| text.lines().any(|line| line == "populated 1"))
    }

    /// Every process in the box and its nested cgroups, at the moment of reading.
    pub fn pids(&self) -> Vec<u32> {
        let mut pids = Vec::new();
        collect_pids(&self.dir, &mut pids);
        pids
    }

    /// SIGKILL every process in the box and in any cgroup nested inside it.
    pub fn kill(&mut self) -> io::Result<()> {
        self.killed = true;
        std::fs::write(self.dir.join("cgroup.kill"), "1")
    }
}

impl Drop for Cgroup {
    fn drop(&mut self) {
        // Best effort: a box that cannot be removed now is reclaimed by a later
        // foil that gets the same PID. On error paths nothing killed it yet.
        if !self.killed {
            let _ = self.kill();
        }
        let _ = remove_tree(&self.dir);
    }
}

/// Whether foil's own cgroup lets it create a box (cgroup v2, delegated).
pub fn can_box() -> bool {
    let Some(dir) = box_path() else { return false };
    let parent = dir.parent().unwrap_or(&dir);
    // Moving a process needs write access to the common ancestor's cgroup.procs.
    is_cgroup2(parent) && writable(&parent.join("cgroup.procs"))
}

/// Where this process's box goes: `<own cgroup>/foil-<pid>`.
pub fn box_path() -> Option<PathBuf> {
    let parent = Path::new(CGROUP_ROOT).join(own_cgroup()?.trim_start_matches('/'));
    Some(parent.join(format!("foil-{}", std::process::id())))
}

/// Kill and remove the box at `dir` if the foil that made it is gone.
/// Returns whether a stale box was found. A live owner is left alone.
pub fn reap_stale(dir: &Path) -> bool {
    let owner = dir.file_name().and_then(|n| n.to_str()?.strip_prefix("foil-")?.parse::<i32>().ok());
    let Some(owner) = owner else { return false };
    if !dir.starts_with(CGROUP_ROOT) || !dir.is_dir() || alive(owner) {
        return false;
    }
    kill_and_remove(dir).is_ok()
}

/// Reap stale boxes in `parent`, i.e. made by foils that ran in the same cgroup.
fn reap_stale_siblings(parent: &Path) {
    for entry in std::fs::read_dir(parent).into_iter().flatten().flatten() {
        if entry.file_name().as_bytes().starts_with(b"foil-") {
            reap_stale(&entry.path());
        }
    }
}

/// Whether a process with this PID exists (EPERM: it exists, not ours).
fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks for existence and permission.
    let exists = unsafe { libc::kill(pid, 0) } == 0;
    exists || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Move the calling process into the box whose `cgroup.procs` is `procs`.
/// Called in the forked child before exec, so it only makes raw syscalls.
pub fn enter(procs: &CString) -> io::Result<()> {
    // SAFETY: `procs` is a valid NUL-terminated path; the buffer is 1 byte long.
    unsafe {
        let fd = libc::open(procs.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
        if fd == -1 {
            return Err(io::Error::last_os_error());
        }
        // "0" means "the process doing this write".
        let written = libc::write(fd, b"0".as_ptr().cast(), 1);
        let err = io::Error::last_os_error();
        libc::close(fd);
        if written != 1 {
            return Err(err);
        }
    }
    Ok(())
}

fn collect_pids(dir: &Path, pids: &mut Vec<u32>) {
    if let Ok(text) = std::fs::read_to_string(dir.join("cgroup.procs")) {
        pids.extend(text.lines().filter_map(|p| p.parse::<u32>().ok()));
    }
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            collect_pids(&entry.path(), pids);
        }
    }
}

fn kill_and_remove(dir: &Path) -> io::Result<()> {
    let _ = std::fs::write(dir.join("cgroup.kill"), "1");
    remove_tree(dir)
}

/// Remove a cgroup and any cgroups the command nested inside it, deepest
/// first. A just-killed cgroup can take a moment to empty, so EBUSY is
/// retried for up to about 100 ms.
fn remove_tree(dir: &Path) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            remove_tree(&entry.path())?;
        }
    }
    for _ in 0..100 {
        match std::fs::remove_dir(dir) {
            Err(err) if err.raw_os_error() == Some(libc::EBUSY) => {
                std::thread::sleep(Duration::from_millis(1))
            }
            other => return other,
        }
    }
    std::fs::remove_dir(dir)
}

/// foil's own cgroup v2 path, relative to the cgroup root (the `0::` line).
fn own_cgroup() -> Option<String> {
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    text.lines().find_map(|line| line.strip_prefix("0::")).map(str::to_owned)
}

// f_type's integer type differs between architectures, hence the cast below.
#[allow(clippy::unnecessary_cast)]
fn is_cgroup2(dir: &Path) -> bool {
    let Ok(path) = CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    let mut stat = MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: statfs fills `stat` completely when it returns 0.
    unsafe {
        libc::statfs(path.as_ptr(), stat.as_mut_ptr()) == 0
            && stat.assume_init().f_type as i64 == CGROUP2_SUPER_MAGIC
    }
}

fn writable(path: &Path) -> bool {
    CString::new(path.as_os_str().as_bytes())
        .is_ok_and(|p| unsafe { libc::access(p.as_ptr(), libc::W_OK) } == 0)
}
