//! macOS: which processes belong to a run.
//!
//! Linux puts each run in a cgroup and asks the cgroup. macOS has nothing
//! like it, and a process that detaches (double fork, `setsid`) is adopted
//! by launchd, losing every link to the run in the process tree. But every
//! process of a run carries the run's Seatbelt profile, and the kernel
//! answers "would process P be allowed to do X?" (`sandbox_check`). So each
//! run's profile denies reading one file of its own (`<id>.a`) and allows
//! its twin (`<id>.b`). A process is in the run if the first is denied to
//! it and the second allowed: unsandboxed processes may read both, and
//! processes with sandboxes of their own (system daemons, app-store apps)
//! are denied both, or allowed both.

use std::ffi::{c_char, c_int, c_void, CString};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

extern "C" {
    // libsystem_sandbox; `type` is SANDBOX_FILTER_PATH, followed by the path.
    fn sandbox_check(pid: libc::pid_t, operation: *const c_char, kind: c_int, ...) -> c_int;
}

const SANDBOX_FILTER_PATH: c_int = 1;
/// Don't log every denied check to the system log.
const SANDBOX_CHECK_NO_REPORT: c_int = 0x4000_0000;

/// The two marker files that identify one run's processes.
#[derive(Debug, Clone)]
pub struct Marker {
    pub denied: PathBuf,
    pub allowed: PathBuf,
}

impl Marker {
    /// New marker files in `$FOIL_HOME/runs/`.
    pub fn create() -> io::Result<Marker> {
        let dir = crate::branch::home()?.join("runs");
        std::fs::create_dir_all(&dir)?;
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let base = dir.join(format!("{}-{nanos}", std::process::id()));
        let marker = Marker { denied: base.with_extension("a"), allowed: base.with_extension("b") };
        std::fs::write(&marker.denied, b"")?;
        std::fs::write(&marker.allowed, b"")?;
        Ok(marker)
    }

    /// The marker whose denied file is `path` (as recorded by a branch).
    pub fn from_denied(path: &Path) -> Marker {
        Marker { denied: path.to_path_buf(), allowed: path.with_extension("b") }
    }

    pub fn exists(&self) -> bool {
        self.denied.exists() && self.allowed.exists()
    }

    /// The Seatbelt rule that makes a process recognisable as this run's.
    pub fn rule(&self) -> String {
        format!("(deny file-read* (literal {}))\n", crate::isolate::quote(&self.denied))
    }

    pub fn remove(&self) {
        let _ = std::fs::remove_file(&self.denied);
        let _ = std::fs::remove_file(&self.allowed);
    }

    /// The processes of this run that are still alive.
    pub fn members(&self) -> Vec<libc::pid_t> {
        let (Ok(denied), Ok(allowed)) = (cpath(&self.denied), cpath(&self.allowed)) else { return Vec::new() };
        // SAFETY: getuid and getpid have no preconditions.
        let (me, uid) = unsafe { (libc::getpid(), libc::getuid()) };
        all_pids()
            .into_iter()
            .filter(|&pid| pid != me && pid > 1 && owner(pid) == Some(uid))
            .filter(|&pid| denied_to(pid, &denied) && !denied_to(pid, &allowed))
            .collect()
    }

    /// Stop every process of the run: SIGTERM, up to `grace` for them to
    /// go (cut short when `hurry` says so), then SIGKILL. A zero grace means
    /// SIGKILL at once. Returns how many there were.
    pub fn stop_all(&self, grace: Duration) -> usize {
        self.stop_all_unless(grace, &|| false)
    }

    pub fn stop_all_unless(&self, grace: Duration, hurry: &dyn Fn() -> bool) -> usize {
        let first = self.members();
        if first.is_empty() {
            return 0;
        }
        if !grace.is_zero() {
            for &pid in &first {
                // SAFETY: plain kill; the PID was a member a moment ago.
                unsafe { libc::kill(pid, libc::SIGTERM) };
            }
            let deadline = Instant::now() + grace;
            while Instant::now() < deadline && !hurry() && !self.members().is_empty() {
                reap_children();
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        // Each SIGKILL round re-lists the members, so a PID reused by some
        // other process since the last list is never hit.
        for _ in 0..50 {
            let left = self.members();
            if left.is_empty() {
                break;
            }
            for pid in left {
                // SAFETY: as above.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            reap_children();
            std::thread::sleep(Duration::from_millis(10));
        }
        reap_children();
        first.len()
    }
}

impl Marker {
    /// SIGKILL every process of the run once, without waiting or reaping
    /// (the caller may still have to collect its own child's status).
    pub fn kill_all(&self) {
        for pid in self.members() {
            // SAFETY: plain kill; the PID was a member a moment ago.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

/// Reap any of our own children that have exited, without waiting.
pub fn reap_children() {
    let mut status = 0;
    // SAFETY: waitpid with WNOHANG never blocks.
    while unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) } > 0 {}
}

fn cpath(path: &Path) -> io::Result<CString> {
    use std::os::unix::ffi::OsStrExt;
    CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))
}

fn denied_to(pid: libc::pid_t, path: &CString) -> bool {
    // SAFETY: valid C strings; sandbox_check only reads them.
    unsafe { sandbox_check(pid, c"file-read-data".as_ptr(), SANDBOX_FILTER_PATH | SANDBOX_CHECK_NO_REPORT, path.as_ptr()) == 1 }
}

/// Every PID on the system.
pub fn all_pids() -> Vec<libc::pid_t> {
    // SAFETY: a null buffer asks for the count.
    let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if n <= 0 {
        return Vec::new();
    }
    // Room for processes started in between.
    let mut pids = vec![0 as libc::pid_t; n as usize + 64];
    let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as c_int;
    // SAFETY: the buffer holds `bytes` bytes.
    let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast::<c_void>(), bytes) };
    pids.truncate(n.max(0) as usize);
    pids.retain(|&p| p > 0);
    pids
}

fn bsdinfo(pid: libc::pid_t) -> Option<libc::proc_bsdinfo> {
    // SAFETY: proc_bsdinfo is plain data; proc_pidinfo fills at most its size.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as c_int;
    let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&mut info as *mut libc::proc_bsdinfo).cast(), size) };
    (n == size).then_some(info)
}

fn owner(pid: libc::pid_t) -> Option<u32> {
    bsdinfo(pid).map(|i| i.pbi_uid)
}

/// Resident memory (physical footprint) of `pid` in bytes.
pub fn footprint(pid: libc::pid_t) -> u64 {
    // SAFETY: rusage_info_v2 is plain data; the call fills it.
    let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V2, (&mut info as *mut libc::rusage_info_v2).cast()) };
    if rc == 0 {
        info.ri_phys_footprint
    } else {
        0
    }
}

/// `foil __guard FOIL_PID MARKER`: macOS has no parent-death signal, so
/// each run gets this small watcher. It waits for foil to exit; if foil
/// left the run's marker behind (it was killed before it could clean up),
/// it stops every process of the run, as a dying PID namespace would on
/// Linux.
pub fn guard_main(args: &[std::ffi::OsString]) -> ! {
    let (Some(pid), Some(marker)) = (args.first().and_then(|a| a.to_str()).and_then(|p| p.parse::<libc::pid_t>().ok()), args.get(1)) else {
        std::process::exit(2)
    };
    let marker = Marker::from_denied(Path::new(marker));
    // SAFETY: kqueue setup on our own descriptors; kevent waits for one event.
    unsafe {
        let kq = libc::kqueue();
        let ev = libc::kevent {
            ident: pid as usize,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        if kq >= 0 && libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) == 0 {
            let mut out: libc::kevent = std::mem::zeroed();
            while libc::kevent(kq, std::ptr::null(), 0, &mut out, 1, std::ptr::null()) != 1 {}
        }
    }
    // Foil is gone (or was gone before we could watch it).
    if marker.exists() {
        marker.stop_all(Duration::ZERO);
        marker.remove();
    }
    std::process::exit(0)
}
