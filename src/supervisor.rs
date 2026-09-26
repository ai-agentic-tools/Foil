//! Transparent process supervision for `foil run`.
//!
//! foil sits between the caller and one child command and must be invisible:
//! the child keeps foil's stdio, environment, terminal and process group, and
//! foil exits exactly the way the child did. Signals sent to foil (e.g. by
//! `docker stop` or a CI runner) are relayed to the child through a pidfd.
//! foil is a child subreaper, so when the command exits, every descendant it
//! left behind is killed and reaped before foil returns.
//!
//! Invariant: the calling process must stay single-threaded while
//! `supervise` runs. Signals are blocked only on the calling thread, and
//! orphans are reparented to (and listed under) the main thread.

use std::ffi::OsString;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::Command;

use libc::{c_int, c_long, pid_t};

/// Signals foil relays to the child. SIGCHLD is watched too, for reaping.
const FORWARDED: [c_int; 7] = [
    libc::SIGHUP,
    libc::SIGINT,
    libc::SIGQUIT,
    libc::SIGTERM,
    libc::SIGUSR1,
    libc::SIGUSR2,
    libc::SIGWINCH,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Termination {
    Exited(i32),
    Signaled(i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    pub termination: Termination,
    /// Descendants other than the command itself that foil reaped.
    pub orphans_reaped: usize,
}

/// Run `argv` as a child, relay signals to it, and wait for it and all of
/// its descendants to be gone.
pub fn supervise(argv: &[OsString]) -> io::Result<Outcome> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no command given"))?;

    // Orphaned descendants get reparented to us instead of to init.
    cvt(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1 as libc::c_ulong) })?;

    // Block the signals we handle so they queue on a signalfd instead of
    // running default actions. This must happen before the child exists.
    let mut watched = FORWARDED.to_vec();
    watched.push(libc::SIGCHLD);
    let mask = sigset(&watched);
    check(unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut()) })?;
    let sfd = cvt(unsafe { libc::signalfd(-1, &mask, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK) })?;
    // SAFETY: signalfd just returned this descriptor and nothing else owns it.
    let sfd = unsafe { OwnedFd::from_raw_fd(sfd) };

    let parent = unsafe { libc::getpid() };
    let empty = sigset(&[]);
    let mut cmd = Command::new(program);
    cmd.args(args);
    // SAFETY: the closure runs in the forked child before exec and only
    // makes async-signal-safe calls (no allocation, no locks).
    unsafe {
        cmd.pre_exec(move || {
            // The signal mask and ignored dispositions survive exec; undo ours.
            check(libc::pthread_sigmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut()))?;
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            // Dead-man's switch: if foil dies (even by SIGKILL), so does the child.
            cvt(libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong))?;
            // foil may have died between fork and prctl; then no signal will come.
            if libc::getppid() != parent {
                libc::_exit(1);
            }
            Ok(())
        });
    }
    let pid = cmd.spawn()?.id() as pid_t;

    // Drain runs even if waiting failed, so an error never leaks the child.
    let result = wait_for_exit(pid, &sfd);
    let drained = drain_descendants();
    result.map(|(termination, reaped)| Outcome {
        termination,
        orphans_reaped: reaped + drained,
    })
}

/// Relay signals to `pid` until it exits; returns its termination and the
/// number of other (orphaned) children reaped along the way.
fn wait_for_exit(pid: pid_t, sfd: &OwnedFd) -> io::Result<(Termination, usize)> {
    // The child is unreaped, so its PID cannot be recycled yet: no race here.
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as c_long, 0 as c_long) };
    if pidfd == -1 {
        let err = io::Error::last_os_error();
        return Err(io::Error::new(
            err.kind(),
            format!("pidfd_open (needs Linux >= 5.3): {err}"),
        ));
    }
    // SAFETY: pidfd_open returned a new descriptor that we now own.
    let pidfd = unsafe { OwnedFd::from_raw_fd(pidfd as c_int) };

    let mut status = None;
    let mut orphans = 0;
    loop {
        reap_exited(pid, &mut status, &mut orphans)?;
        if let Some(termination) = status {
            return Ok((termination, orphans));
        }

        let mut fds = [pollin(sfd), pollin(&pidfd)];
        if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) } == -1 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if fds[0].revents & libc::POLLIN != 0 {
            for info in read_signals(sfd)? {
                let sig = info.ssi_signo as c_int;
                // SIGCHLD only wakes us up to reap. Terminal-generated signals
                // (SI_KERNEL) already reached the child via the shared process
                // group; forwarding them would deliver Ctrl-C twice.
                if sig != libc::SIGCHLD && info.ssi_code != libc::SI_KERNEL {
                    forward(&pidfd, sig)?;
                }
            }
        }
    }
}

fn pollin(fd: &OwnedFd) -> libc::pollfd {
    libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    }
}

fn read_signals(sfd: &OwnedFd) -> io::Result<Vec<libc::signalfd_siginfo>> {
    const BATCH: usize = 16;
    let size = std::mem::size_of::<libc::signalfd_siginfo>();
    // SAFETY: signalfd_siginfo is plain integers; all-zero is a valid value.
    let mut buf: [libc::signalfd_siginfo; BATCH] = unsafe { std::mem::zeroed() };
    let n = unsafe { libc::read(sfd.as_raw_fd(), buf.as_mut_ptr().cast(), size * BATCH) };
    if n == -1 {
        let err = io::Error::last_os_error();
        return match err.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Ok(Vec::new()),
            _ => Err(err),
        };
    }
    Ok(buf[..n as usize / size].to_vec())
}

fn forward(pidfd: &OwnedFd, sig: c_int) -> io::Result<()> {
    // Signalling through the pidfd can never hit a recycled PID.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd() as c_long,
            sig as c_long,
            0 as c_long,
            0 as c_long,
        )
    };
    if rc == -1 {
        let err = io::Error::last_os_error();
        // ESRCH: the child already exited; the next reap will notice.
        if err.raw_os_error() != Some(libc::ESRCH) {
            return Err(err);
        }
    }
    Ok(())
}

/// Reap every exited child without blocking, recording the main child's status.
fn reap_exited(main: pid_t, status: &mut Option<Termination>, orphans: &mut usize) -> io::Result<()> {
    loop {
        let mut raw = 0;
        match unsafe { libc::waitpid(-1, &mut raw, libc::WNOHANG) } {
            0 => return Ok(()),
            -1 => {
                let err = io::Error::last_os_error();
                match err.raw_os_error() {
                    Some(libc::ECHILD) => return Ok(()),
                    Some(libc::EINTR) => continue,
                    _ => return Err(err),
                }
            }
            pid if pid == main => *status = Some(decode(raw)),
            _ => *orphans += 1,
        }
    }
}

fn decode(raw: c_int) -> Termination {
    if libc::WIFSIGNALED(raw) {
        Termination::Signaled(libc::WTERMSIG(raw))
    } else {
        Termination::Exited(libc::WEXITSTATUS(raw))
    }
}

/// Kill and reap every remaining descendant. As a subreaper we inherit each
/// grandchild when its parent dies, so repeat until waitpid reports ECHILD.
/// Only our own unreaped children are signalled, so no PID can be recycled.
fn drain_descendants() -> usize {
    let me = unsafe { libc::getpid() };
    let mut reaped = 0;
    loop {
        for child in children_of(me) {
            unsafe { libc::kill(child, libc::SIGKILL) };
        }
        let mut raw = 0;
        match unsafe { libc::waitpid(-1, &mut raw, 0) } {
            -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => continue,
            -1 => return reaped, // ECHILD: nothing left
            _ => reaped += 1,
        }
    }
}

/// PIDs whose parent is `parent`: fast path via the task's `children` file
/// (CONFIG_PROC_CHILDREN), falling back to scanning /proc/*/stat.
fn children_of(parent: pid_t) -> Vec<pid_t> {
    if let Ok(list) = std::fs::read_to_string(format!("/proc/{parent}/task/{parent}/children")) {
        return list.split_whitespace().filter_map(|p| p.parse().ok()).collect();
    }
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| {
            let pid: pid_t = entry.ok()?.file_name().to_str()?.parse().ok()?;
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            // Field 2 (comm) may contain spaces and ')', so parse after the last ')'.
            let after_comm = stat.get(stat.rfind(')')? + 2..)?;
            let ppid: pid_t = after_comm.split(' ').nth(1)?.parse().ok()?;
            (ppid == parent).then_some(pid)
        })
        .collect()
}

/// Exit the current process the same way the child terminated, so foil's own
/// parent (a shell, CI runner or container runtime) sees an identical status.
pub fn exit_like(termination: Termination) -> ! {
    match termination {
        Termination::Exited(code) => std::process::exit(code),
        Termination::Signaled(sig) => {
            // SAFETY: plain libc calls on a process that is about to die.
            unsafe {
                // The child may already have dumped core; foil must not add a second.
                let no_core = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
                libc::setrlimit(libc::RLIMIT_CORE, &no_core);
                libc::signal(sig, libc::SIG_DFL);
                libc::pthread_sigmask(libc::SIG_UNBLOCK, &sigset(&[sig]), std::ptr::null_mut());
                libc::raise(sig);
            }
            // Only reached if the signal's default action does not terminate.
            std::process::exit(128 + sig)
        }
    }
}

fn sigset(signals: &[c_int]) -> libc::sigset_t {
    let mut set = MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: sigemptyset initialises the whole set before it is read.
    unsafe {
        libc::sigemptyset(set.as_mut_ptr());
        for &sig in signals {
            libc::sigaddset(set.as_mut_ptr(), sig);
        }
        set.assume_init()
    }
}

/// Map the `-1 + errno` convention to `io::Result`.
fn cvt(rc: c_int) -> io::Result<c_int> {
    if rc == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

/// pthread_* functions return the error number instead of setting errno.
fn check(rc: c_int) -> io::Result<()> {
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(rc))
    }
}
