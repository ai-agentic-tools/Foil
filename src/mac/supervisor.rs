//! macOS: transparent process supervision for `foil run`.
//!
//! The same contract as on Linux: the command keeps foil's stdio, terminal,
//! process group and (unless strict) environment; signals sent to foil are
//! passed on; foil exits exactly as the command did; and when the command
//! ends, everything it started is stopped (SIGTERM, a grace period, then
//! SIGKILL). Linux uses pidfds, a signalfd and a cgroup. macOS has kqueue,
//! which reports both signals and a process's exit, and finds the run's
//! processes through its Seatbelt profile (`procs.rs`), which survives
//! double forks and `setsid`.

use std::ffi::OsString;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use libc::c_int;

use crate::isolate::Plan;

/// Signals foil passes on to the command.
pub(crate) const FORWARDED: [c_int; 7] =
    [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM, libc::SIGUSR1, libc::SIGUSR2, libc::SIGWINCH];

const DEFAULT_GRACE: Duration = Duration::from_millis(1000);

/// How often the memory watchdog measures, when `--memory` is set.
const WATCH_TICK: Duration = Duration::from_millis(200);

/// Set when the memory watchdog stopped a run (see `limits::oom_killed`).
pub(crate) static MEMORY_KILLED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Termination {
    Exited(i32),
    Signaled(i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    pub termination: Termination,
    /// Processes other than the command itself that foil stopped afterwards.
    pub orphans_reaped: usize,
    /// Whether the run's processes could be tracked (it had a profile).
    pub contained: bool,
    pub timed_out: bool,
}

pub fn supervise(argv: &[OsString], isolation: Option<&Plan>) -> io::Result<Outcome> {
    supervise_for(argv, isolation, None)
}

/// Run `argv` to the end, under `isolation`'s profile; at most `timeout`.
pub fn supervise_for(argv: &[OsString], isolation: Option<&Plan>, timeout: Option<Duration>) -> io::Result<Outcome> {
    let result = run(argv, isolation, timeout);
    if let Some(plan) = isolation {
        plan.cleanup();
    }
    result
}

fn run(argv: &[OsString], isolation: Option<&Plan>, timeout: Option<Duration>) -> io::Result<Outcome> {
    let (first, _) = argv.split_first().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no command given"))?;
    // sandbox-exec would report a missing command as its own failure (exit
    // 71); find it first, so foil can answer 127 or 126 like a shell.
    let path_var = isolation
        .and_then(Plan::env)
        .and_then(|env| env.iter().find(|(k, _)| k == "PATH").map(|(_, v)| v.clone()))
        .or_else(|| std::env::var_os("PATH"))
        .unwrap_or_default();
    let cwd = isolation.and_then(|p| p.cwd.clone());
    let program = resolve(first, &path_var, cwd.as_deref())?;
    let mut argv = argv.to_vec();
    argv[0] = program.into_os_string();

    let (program, args) = match isolation {
        Some(p) => p.wrap(&argv),
        None => (argv[0].clone(), argv[1..].to_vec()),
    };
    let mut cmd = Command::new(&program);
    cmd.args(&args);
    if let Some(plan) = isolation {
        if let Some(env) = plan.env() {
            cmd.env_clear().envs(env.iter().map(|(k, v)| (k, v)));
        }
        if let Some(dir) = &plan.cwd {
            cmd.current_dir(dir).env("PWD", dir);
        }
    }
    // foil ignores the signals it forwards (kqueue still reports them); the
    // command must start with the default actions, since ignored ones
    // survive exec.
    // SAFETY: signal() and sigprocmask() are async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            for sig in FORWARDED {
                libc::signal(sig, libc::SIG_DFL);
            }
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            let mut empty: libc::sigset_t = 0;
            libc::sigemptyset(&mut empty);
            libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
            Ok(())
        });
    }
    let kq = Kqueue::new()?;
    for sig in FORWARDED {
        // SAFETY: changing our own dispositions.
        unsafe { libc::signal(sig, libc::SIG_IGN) };
        kq.add(sig as usize, libc::EVFILT_SIGNAL, 0)?;
    }
    // SIGCHLD too (its default action is to do nothing): a child that exits
    // while the process watch below is being set up still wakes us.
    kq.add(libc::SIGCHLD as usize, libc::EVFILT_SIGNAL, 0)?;
    let restore = || {
        for sig in FORWARDED {
            // SAFETY: as above.
            unsafe { libc::signal(sig, libc::SIG_DFL) };
        }
    };
    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            restore();
            return Err(err);
        }
    };
    let pid = child.id() as libc::pid_t;
    // If foil is killed before it can clean up, this watcher stops the run.
    let guard = isolation.and_then(|p| {
        Command::new(std::env::current_exe().ok()?)
            .arg("__guard")
            // SAFETY: getpid has no preconditions.
            .arg(unsafe { libc::getpid() }.to_string())
            .arg(&p.marker.denied)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()
    });
    // The child may exit before kqueue watches it; waitpid below covers that.
    let _ = kq.add(pid as usize, libc::EVFILT_PROC, libc::NOTE_EXIT);

    let grace = grace_from_env();
    let memory = isolation.and_then(|p| p.memory);
    let marker = isolation.map(|p| p.marker.clone());
    // SAFETY: plain queries of our own process group and terminal.
    let foreground = unsafe { libc::isatty(0) == 1 && libc::tcgetpgrp(0) == libc::getpgrp() };

    let mut deadline = timeout.and_then(|t| Instant::now().checked_add(t));
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = try_wait(pid)? {
            break status;
        }
        let now = Instant::now();
        if let Some(at) = deadline.filter(|at| now >= *at) {
            if timed_out {
                // SAFETY: our unreaped child; its PID can't be reused yet.
                unsafe { libc::kill(pid, libc::SIGKILL) };
                deadline = None;
            } else {
                timed_out = true;
                // SAFETY: as above.
                unsafe { libc::kill(pid, libc::SIGTERM) };
                deadline = Some(at.checked_add(grace).unwrap_or(at));
            }
            continue;
        }
        if let (Some(limit), Some(m)) = (memory, &marker) {
            let used: u64 = m.members().into_iter().chain([pid]).map(crate::procs::footprint).sum();
            if used > limit {
                MEMORY_KILLED.store(true, Ordering::SeqCst);
                // SAFETY: as above.
                unsafe { libc::kill(pid, libc::SIGKILL) };
                m.kill_all();
            }
        }
        // Never wait unbounded: a last line of defence against a missed wake-up.
        let tick = if memory.is_some() { WATCH_TICK } else { Duration::from_secs(1) };
        let wait = Some(deadline.map_or(tick, |at| at.saturating_duration_since(now).min(tick)));
        for (ident, filter) in kq.wait(wait)? {
            if filter == libc::EVFILT_SIGNAL && ident != libc::SIGCHLD as usize {
                let sig = ident as c_int;
                // Keys typed at the terminal already reached the command,
                // which shares foil's foreground process group.
                let from_terminal = foreground && matches!(sig, libc::SIGINT | libc::SIGQUIT | libc::SIGWINCH);
                if !from_terminal {
                    // SAFETY: as above.
                    unsafe { libc::kill(pid, sig) };
                }
            }
        }
    };

    // Whatever the command left behind: SIGTERM, the grace period (cut
    // short if foil itself is told to stop), SIGKILL.
    let hurry = || {
        kq.wait(Some(Duration::ZERO))
            .unwrap_or_default()
            .iter()
            .any(|&(ident, filter)| filter == libc::EVFILT_SIGNAL && [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM].contains(&(ident as c_int)))
    };
    let orphans = marker.as_ref().map_or(0, |m| m.stop_all_unless(grace, &hurry));
    if let Some(mut guard) = guard {
        let _ = guard.kill();
        let _ = guard.wait();
    }
    crate::procs::reap_children();
    restore();
    Ok(Outcome { termination: decode(status), orphans_reaped: orphans, contained: marker.is_some(), timed_out })
}

/// `program` as a path to run: as given if it has a '/', else found on `path`.
fn resolve(program: &OsString, path: &OsString, cwd: Option<&Path>) -> io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let given = Path::new(program);
    let candidates: Vec<PathBuf> = if program.to_string_lossy().contains('/') {
        let full = match (given.is_relative(), cwd) {
            (true, Some(dir)) => dir.join(given),
            _ => given.to_path_buf(),
        };
        vec![full]
    } else {
        std::env::split_paths(path).map(|d| d.join(given)).collect()
    };
    let mut denied = false;
    for c in candidates {
        match std::fs::metadata(&c) {
            Ok(m) if m.is_file() && m.permissions().mode() & 0o111 != 0 => return Ok(c),
            Ok(_) => denied = true,
            Err(_) => {}
        }
    }
    let kind = if denied { io::ErrorKind::PermissionDenied } else { io::ErrorKind::NotFound };
    Err(io::Error::new(kind, if denied { "permission denied" } else { "command not found" }))
}

fn try_wait(pid: libc::pid_t) -> io::Result<Option<c_int>> {
    let mut status = 0;
    loop {
        // SAFETY: waitpid on our own child.
        match unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } {
            0 => return Ok(None),
            -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => continue,
            -1 => return Err(io::Error::last_os_error()),
            _ => return Ok(Some(status)),
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

/// FOIL_GRACE_MS as a duration; 0 means SIGKILL leftovers at once.
pub(crate) fn grace_from_env() -> Duration {
    let Some(raw) = std::env::var_os("FOIL_GRACE_MS") else { return DEFAULT_GRACE };
    match raw.to_str().and_then(|ms| ms.trim().parse().ok()) {
        Some(ms) => Duration::from_millis(ms),
        None => {
            eprintln!("foil: ignoring FOIL_GRACE_MS={raw:?}: not a whole number of milliseconds");
            DEFAULT_GRACE
        }
    }
}

/// A kqueue: one place to wait for signals and a process's exit.
struct Kqueue(c_int);

impl Kqueue {
    fn new() -> io::Result<Kqueue> {
        // SAFETY: kqueue has no preconditions.
        let fd = unsafe { libc::kqueue() };
        if fd == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: our own descriptor.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        Ok(Kqueue(fd))
    }

    fn add(&self, ident: usize, filter: i16, fflags: u32) -> io::Result<()> {
        let ev = libc::kevent { ident, filter, flags: libc::EV_ADD, fflags, data: 0, udata: std::ptr::null_mut() };
        // SAFETY: one valid change, no events out.
        if unsafe { libc::kevent(self.0, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Wait up to `timeout` (forever if `None`); returns (ident, filter) pairs.
    fn wait(&self, timeout: Option<Duration>) -> io::Result<Vec<(usize, i16)>> {
        // SAFETY: kevent is plain data.
        let mut events: [libc::kevent; 16] = unsafe { std::mem::zeroed() };
        let ts = timeout.map(|t| libc::timespec { tv_sec: t.as_secs() as libc::time_t, tv_nsec: t.subsec_nanos() as libc::c_long });
        let tsp = ts.as_ref().map_or(std::ptr::null(), |t| t as *const libc::timespec);
        // SAFETY: the buffer holds 16 events.
        let n = unsafe { libc::kevent(self.0, std::ptr::null(), 0, events.as_mut_ptr(), 16, tsp) };
        if n == -1 {
            let err = io::Error::last_os_error();
            return if err.kind() == io::ErrorKind::Interrupted { Ok(Vec::new()) } else { Err(err) };
        }
        Ok(events[..n as usize].iter().map(|e| (e.ident, e.filter)).collect())
    }
}

impl Drop for Kqueue {
    fn drop(&mut self) {
        // SAFETY: our own descriptor.
        unsafe { libc::close(self.0) };
    }
}

/// Exit the current process the same way the child terminated.
pub fn exit_like(termination: Termination) -> ! {
    match termination {
        Termination::Exited(code) => std::process::exit(code),
        Termination::Signaled(sig) => {
            // SAFETY: plain libc calls on a process that is about to die.
            unsafe {
                let no_core = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
                libc::setrlimit(libc::RLIMIT_CORE, &no_core);
                libc::signal(sig, libc::SIG_DFL);
                let mut set: libc::sigset_t = 0;
                libc::sigemptyset(&mut set);
                libc::sigaddset(&mut set, sig);
                libc::sigprocmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
                libc::raise(sig);
            }
            std::process::exit(128 + sig)
        }
    }
}
