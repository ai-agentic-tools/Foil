//! `--strict`: a deny-by-default view of the filesystem and a PID namespace.
//!
//! Instead of showing the command the whole machine (read-only or not),
//! strict mode builds a fresh root directory for it and switches into it:
//!
//! - system folders (`/usr`, `/etc`, `/lib`, ...) and the folders on `PATH`,
//!   read-only, `nosuid`, `nodev`;
//! - the workspace: the branch overlay (writable) or the current folder
//!   (read-only); and the paths given with `--allow-read` / `--allow-write`;
//! - an empty home folder, a private `/tmp`, a minimal `/dev`, a fresh `/proc`.
//!
//! Anything else (`~/.ssh`, other repositories, `/run/user` with its sockets,
//! the branch store) is not in the view, so it can't be read or written.
//! The root itself is a tmpfs: writes outside the mounted paths succeed but
//! vanish with the run, so tools that expect a writable home keep working.
//!
//! The command also gets its own PID namespace, so it can't see or signal
//! any process outside the run. The first process of a PID namespace is its
//! "init" (PID 1), which the kernel treats specially: it ignores signals it
//! has no handler for, and orphans are handed to it. So the child foil spawns
//! forks once more: the new child becomes PID 1 and runs `foil __pid1` (see
//! `pid1.rs`), which starts the command, relays signals and reaps; the
//! original child stays outside as a relay between foil and PID 1.
//!
//! Like `isolate::enter`, everything that runs in the child is raw system
//! calls on buffers built in advance: no allocation between fork and exec.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use libc::{c_int, c_long};

use crate::isolate::c_path;
use crate::supervisor::{cvt, FORWARDED};

/// Where the new root is assembled, before the switch. A tmpfs is mounted
/// over it in the run's private mount namespace; the real `/tmp` is unaffected.
const STAGING: &str = "/tmp";
/// The descriptor PID 1 reports the command's exit status on.
pub const STATUS_FD: c_int = 1023;
/// Where the network proxy's socket folder appears inside the view.
pub const PROXY_DIR: &str = "/run/foil";
/// Room for this many entries in the view; the child keeps one descriptor
/// per mounted path in a fixed-size array, since it may not allocate.
pub const MAX_ENTRIES: usize = 96;

/// Read-only system folders, shown when they exist (as symlinks if they are).
const SYSTEM: [&str; 9] = ["/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32", "/etc", "/opt"];
/// Device files the command may use. `/dev/tty` is the controlling terminal.
const DEVICES: [&str; 6] = ["/dev/null", "/dev/zero", "/dev/full", "/dev/random", "/dev/urandom", "/dev/tty"];

#[derive(Clone, Debug, PartialEq)]
enum Kind {
    /// A clone of the host's `source` mounted here.
    Bind { source: CString, read_only: bool, devices: bool, dir: bool },
    Symlink(CString),
    /// A fresh tmpfs with these mount options.
    Tmpfs(CString),
    /// An empty directory (in the root tmpfs).
    Dir,
    /// A new, private devpts instance (for programs that open terminals).
    Devpts,
}

#[derive(Clone)]
struct Entry {
    /// `STAGING` + the path inside the view.
    target: CString,
    /// Directories to create first, outermost first.
    ancestors: Vec<CString>,
    kind: Kind,
}

/// The view, ready for the child to build.
#[derive(Clone)]
pub struct View {
    entries: Vec<Entry>,
    staging: CString,
    proc_dir: CString,
    /// Where the command starts: the current folder, or `/` if it isn't in the view.
    cwd: CString,
}

/// What to put in the view besides the system folders.
pub struct Spec<'a> {
    /// The workspace path and whether it is writable (a branch) or not.
    pub workspace: Option<(&'a Path, bool)>,
    pub allow_read: &'a [PathBuf],
    pub allow_write: &'a [PathBuf],
    /// The host folder holding the network proxy's socket.
    pub proxy_dir: Option<&'a Path>,
    pub home: Option<PathBuf>,
}

impl View {
    pub fn new(spec: &Spec) -> io::Result<View> {
        // Later inserts win for the same path, so the order below is the
        // order of precedence; a BTreeMap also sorts parents before children.
        let mut map: BTreeMap<PathBuf, Kind> = BTreeMap::new();
        let ro_dir = |p: &Path| -> io::Result<Kind> {
            Ok(Kind::Bind { source: c_path(p)?, read_only: true, devices: false, dir: true })
        };
        for dir in SYSTEM.iter().map(Path::new) {
            match fs::symlink_metadata(dir) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    map.insert(dir.into(), Kind::Symlink(c_path(&fs::read_link(dir)?)?));
                }
                Ok(meta) if meta.is_dir() => {
                    map.insert(dir.into(), ro_dir(dir)?);
                }
                _ => {}
            }
        }
        // Tools on PATH outside the system folders (~/.cargo/bin, ~/.local/bin).
        let path = std::env::var_os("PATH").unwrap_or_default();
        for dir in std::env::split_paths(&path) {
            let covered = SYSTEM.iter().any(|s| dir.starts_with(s));
            if dir.is_absolute() && dir.is_dir() && !covered {
                map.insert(dir.clone(), ro_dir(&dir)?);
            }
        }
        if let Some(home) = &spec.home {
            map.insert(home.clone(), Kind::Dir);
        }
        if let Some((ws, writable)) = spec.workspace {
            map.insert(
                ws.to_path_buf(),
                Kind::Bind { source: c_path(ws)?, read_only: !writable, devices: false, dir: true },
            );
        }
        for (paths, writable, flag) in [(spec.allow_read, false, "--allow-read"), (spec.allow_write, true, "--allow-write")] {
            for path in paths {
                let real = path
                    .canonicalize()
                    .map_err(|err| io::Error::new(err.kind(), format!("{flag} {}: {err}", path.display())))?;
                let dir = real.is_dir();
                map.insert(real.clone(), Kind::Bind { source: c_path(&real)?, read_only: !writable, devices: false, dir });
            }
        }
        // Private scratch space, devices, /proc, and the proxy socket.
        let tmpfs = |opts: &str| Kind::Tmpfs(CString::new(opts).expect("no NUL"));
        map.insert("/tmp".into(), tmpfs("mode=1777"));
        map.insert("/var/tmp".into(), tmpfs("mode=1777"));
        map.insert("/dev".into(), tmpfs("mode=0755"));
        for dev in DEVICES.iter().map(Path::new).filter(|d| d.exists()) {
            map.insert(dev.into(), Kind::Bind { source: c_path(dev)?, read_only: false, devices: true, dir: false });
        }
        map.insert("/dev/pts".into(), Kind::Devpts);
        map.insert("/dev/shm".into(), tmpfs("mode=1777"));
        for (link, to) in [
            ("/dev/ptmx", "pts/ptmx"),
            ("/dev/fd", "/proc/self/fd"),
            ("/dev/stdin", "/proc/self/fd/0"),
            ("/dev/stdout", "/proc/self/fd/1"),
            ("/dev/stderr", "/proc/self/fd/2"),
        ] {
            map.insert(link.into(), Kind::Symlink(CString::new(to).expect("no NUL")));
        }
        map.insert("/proc".into(), Kind::Dir);
        map.insert("/run".into(), Kind::Dir);
        if let Some(dir) = spec.proxy_dir {
            map.insert(PROXY_DIR.into(), Kind::Bind { source: c_path(dir)?, read_only: true, devices: false, dir: true });
        }
        if map.len() > MAX_ENTRIES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("the strict view would need {} entries; at most {MAX_ENTRIES}", map.len()),
            ));
        }

        let staged = |p: &Path| -> io::Result<CString> {
            let mut bytes = STAGING.as_bytes().to_vec();
            bytes.extend_from_slice(p.as_os_str().as_bytes());
            CString::new(bytes).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL byte in a path"))
        };
        // The command starts in the current folder if the view shows it.
        let cwd = std::env::current_dir()?;
        let visible = map.iter().any(|(path, kind)| matches!(kind, Kind::Bind { .. }) && cwd.starts_with(path));
        let mut entries = Vec::new();
        for (path, kind) in map {
            let mut ancestors = Vec::new();
            for anc in path.ancestors().skip(1).filter(|a| *a != Path::new("/")) {
                ancestors.push(staged(anc)?);
            }
            ancestors.reverse();
            entries.push(Entry { target: staged(&path)?, ancestors, kind });
        }
        Ok(View {
            entries,
            staging: c_path(Path::new(STAGING))?,
            proc_dir: staged(Path::new("/proc"))?,
            cwd: if visible { c_path(&cwd)? } else { CString::new("/").expect("no NUL") },
        })
    }
}

/// Assemble the view under `STAGING`. Runs in the forked child, in its new
/// user and mount namespaces, after the branch overlay is mounted.
///
/// # Safety
/// Only raw system calls on `view`'s prebuilt buffers; call between fork and exec.
pub unsafe fn build(view: &View) -> io::Result<()> {
    // Clone every source first: mounting the staging tmpfs over /tmp would
    // otherwise hide sources that live under /tmp (a workspace, say).
    let mut clones = [-1 as c_int; MAX_ENTRIES];
    for (slot, entry) in clones.iter_mut().zip(&view.entries) {
        if let Kind::Bind { source, read_only, devices, .. } = &entry.kind {
            *slot = syscall_fd(libc::syscall(
                libc::SYS_open_tree,
                libc::AT_FDCWD as c_long,
                source.as_ptr(),
                (libc::OPEN_TREE_CLONE | libc::OPEN_TREE_CLOEXEC | libc::AT_RECURSIVE as libc::c_uint) as c_long,
            ))?;
            let mut set = libc::MOUNT_ATTR_NOSUID;
            if !devices {
                set |= libc::MOUNT_ATTR_NODEV;
            }
            if *read_only {
                set |= libc::MOUNT_ATTR_RDONLY;
            }
            let attr = libc::mount_attr { attr_set: set, attr_clr: 0, propagation: 0, userns_fd: 0 };
            syscall_fd(libc::syscall(
                libc::SYS_mount_setattr,
                *slot as c_long,
                c"".as_ptr(),
                (libc::AT_EMPTY_PATH | libc::AT_RECURSIVE) as c_long,
                &attr as *const libc::mount_attr,
                std::mem::size_of::<libc::mount_attr>(),
            ))?;
        }
    }
    cvt(libc::mount(
        c"tmpfs".as_ptr(),
        view.staging.as_ptr(),
        c"tmpfs".as_ptr(),
        libc::MS_NOSUID | libc::MS_NODEV,
        c"mode=0755".as_ptr().cast(),
    ))?;
    for (&fd, entry) in clones.iter().zip(&view.entries) {
        for dir in &entry.ancestors {
            mkdir_ok(dir)?;
        }
        let target = entry.target.as_ptr();
        match &entry.kind {
            Kind::Bind { dir, .. } => {
                if *dir {
                    mkdir_ok(&entry.target)?;
                } else {
                    // A file is mounted over a file.
                    let f = libc::open(target, libc::O_CREAT | libc::O_WRONLY | libc::O_CLOEXEC, 0o644);
                    if f != -1 {
                        libc::close(f);
                    }
                }
                syscall_fd(libc::syscall(
                    libc::SYS_move_mount,
                    fd as c_long,
                    c"".as_ptr(),
                    libc::AT_FDCWD as c_long,
                    target,
                    libc::MOVE_MOUNT_F_EMPTY_PATH as c_long,
                ))?;
                libc::close(fd);
            }
            Kind::Symlink(to) => {
                if libc::symlink(to.as_ptr(), target) == -1 && errno() != libc::EEXIST {
                    return Err(io::Error::last_os_error());
                }
            }
            Kind::Tmpfs(options) => {
                mkdir_ok(&entry.target)?;
                cvt(libc::mount(
                    c"tmpfs".as_ptr(),
                    target,
                    c"tmpfs".as_ptr(),
                    libc::MS_NOSUID | libc::MS_NODEV,
                    options.as_ptr().cast(),
                ))?;
            }
            Kind::Dir => mkdir_ok(&entry.target)?,
            Kind::Devpts => {
                mkdir_ok(&entry.target)?;
                cvt(libc::mount(
                    c"devpts".as_ptr(),
                    target,
                    c"devpts".as_ptr(),
                    libc::MS_NOSUID | libc::MS_NOEXEC,
                    c"newinstance,ptmxmode=0666,mode=0620".as_ptr().cast(),
                ))?;
            }
        }
    }
    Ok(())
}

/// Fork the PID namespace's first process. In the new process (PID 1),
/// mount its `/proc`, switch into the view, and return, so the caller goes
/// on to exec `foil __pid1`. The calling process never returns: it relays
/// signals from foil to PID 1 and exits the way PID 1 says the command did.
///
/// # Safety
/// Only raw system calls; call between fork and exec, after `build`.
pub unsafe fn fork_pid1(view: &View) -> io::Result<()> {
    // The relay waits for signals with sigwaitinfo, which only receives
    // blocked signals. Block them before the fork, so that PID 1 ending
    // early can't slip past (and so this works whether or not the caller,
    // e.g. `foil doctor`'s probe, blocked them). PID 1 unblocks its own
    // mask before exec (supervisor.rs, pid1.rs).
    let mut set = std::mem::zeroed::<libc::sigset_t>();
    libc::sigemptyset(&mut set);
    for &sig in FORWARDED.iter().chain(&[libc::SIGCHLD]) {
        libc::sigaddset(&mut set, sig);
    }
    libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    let mut pipe = [0 as c_int; 2];
    cvt(libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC))?;
    let pid = cvt(libc::fork())?;
    if pid == 0 {
        libc::close(pipe[0]);
        // dup2 clears close-on-exec, so __pid1 inherits the status pipe.
        cvt(libc::dup2(pipe[1], STATUS_FD))?;
        libc::close(pipe[1]);
        // If the relay dies (because foil did), so does everything inside.
        cvt(libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong))?;
        // A /proc showing only this namespace's processes. Only a process
        // inside the namespace can mount it, hence after the fork.
        cvt(libc::mount(
            c"proc".as_ptr(),
            view.proc_dir.as_ptr(),
            c"proc".as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
            std::ptr::null(),
        ))?;
        // Make the view the root: put it in place of /, then detach the old
        // root, which pivot_root(".", ".") leaves stacked underneath.
        cvt(libc::chdir(view.staging.as_ptr()))?;
        syscall_fd(libc::syscall(libc::SYS_pivot_root, c".".as_ptr(), c".".as_ptr()))?;
        cvt(libc::umount2(c".".as_ptr(), libc::MNT_DETACH))?;
        if libc::chdir(view.cwd.as_ptr()) == -1 {
            cvt(libc::chdir(c"/".as_ptr()))?;
        }
        return Ok(());
    }
    libc::close(pipe[1]);
    relay(pid, pipe[0], &set)
}

/// The original child, outside the PID namespace: pass foil's signals on to
/// PID 1, then exit exactly as the command did (as PID 1 reports on `status`).
unsafe fn relay(pid1: libc::pid_t, status: c_int, set: &libc::sigset_t) -> ! {
    // Keep only stdio and the status pipe. In particular this closes the
    // close-on-exec pipe Rust's spawn() waits on: this process never execs,
    // so spawn would otherwise wait for it to exit.
    if status > 3 {
        libc::syscall(libc::SYS_close_range, 3 as c_long, (status - 1) as c_long, 0 as c_long);
    }
    libc::syscall(libc::SYS_close_range, (status + 1) as c_long, u32::MAX as c_long, 0 as c_long);
    // The watched signals are blocked (see fork_pid1), so they queue for
    // sigwaitinfo instead of acting on this process.
    loop {
        let mut raw = 0;
        if libc::waitpid(pid1, &mut raw, libc::WNOHANG) == pid1 {
            mirror(raw, status);
        }
        let mut info = std::mem::zeroed::<libc::siginfo_t>();
        let sig = libc::sigwaitinfo(set, &mut info);
        if sig == -1 {
            if errno() == libc::EINTR {
                continue;
            }
            libc::_exit(125);
        }
        if sig == libc::SIGCHLD {
            continue; // checked at the top of the loop
        }
        // Terminal-generated signals (Ctrl-C) already reach the whole process group.
        if info.si_code != libc::SI_KERNEL {
            libc::kill(pid1, sig);
        }
    }
}

/// Exit the way the command did: PID 1 writes `[0, code]` or `[1, signal]`.
unsafe fn mirror(raw: c_int, status: c_int) -> ! {
    let mut report = [0u8; 2];
    let n = libc::read(status, report.as_mut_ptr().cast(), 2);
    let (signaled, value) = if n == 2 {
        (report[0] == 1, c_int::from(report[1]))
    } else if libc::WIFSIGNALED(raw) {
        (true, libc::WTERMSIG(raw))
    } else {
        (false, libc::WEXITSTATUS(raw))
    };
    if signaled {
        let no_core = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        libc::setrlimit(libc::RLIMIT_CORE, &no_core);
        libc::signal(value, libc::SIG_DFL);
        let mut set = std::mem::zeroed::<libc::sigset_t>();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, value);
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
        libc::raise(value);
        libc::_exit(128 + value);
    }
    libc::_exit(value)
}

unsafe fn mkdir_ok(path: &CString) -> io::Result<()> {
    if libc::mkdir(path.as_ptr(), 0o755) == -1 && errno() != libc::EEXIST {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn errno() -> c_int {
    io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn syscall_fd(rc: c_long) -> io::Result<c_int> {
    if rc == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc as c_int)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_puts_parents_before_children_and_hides_home_contents() {
        let home = std::env::temp_dir().join("foil-view-home");
        let spec = Spec { workspace: None, allow_read: &[], allow_write: &[], proxy_dir: None, home: Some(home.clone()) };
        let view = View::new(&spec).unwrap();
        let paths: Vec<&[u8]> = view.entries.iter().map(|e| &e.target.as_bytes()[STAGING.len()..]).collect();
        let pos = |p: &str| paths.iter().position(|q| *q == p.as_bytes()).unwrap();
        assert!(pos("/dev") < pos("/dev/null") && pos("/dev/null") < pos("/dev/pts"));
        let home_entry = view.entries.iter().find(|e| e.target.as_bytes().ends_with(home.as_os_str().as_bytes())).unwrap();
        assert_eq!(home_entry.kind, Kind::Dir, "home must be an empty directory, not the real one");
    }
}
