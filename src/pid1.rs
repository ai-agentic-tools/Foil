//! `foil __pid1 -- <command>`: the first process of a strict run's PID
//! namespace (see `strict.rs`).
//!
//! As PID 1 it must do what an init does: the kernel only delivers a signal
//! to PID 1 if it has a handler for it, and every orphan in the namespace
//! becomes its child. So it installs handlers, starts the command, passes
//! signals on to it, reaps every exited process, gives leftovers the grace
//! period when the command is done, and reports how the command ended on the
//! status pipe. When it exits, the kernel kills whatever is still running in
//! the namespace.
//!
//! With a network allowlist it also relays `127.0.0.1:<port>` inside the
//! run's network namespace to the proxy's socket outside (`proxy.rs`).

use std::ffi::OsString;
use std::io::{self, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::Instant;

use libc::c_int;

use crate::supervisor::{grace_from_env, Termination, FORWARDED};

extern "C" fn ignore(_: c_int) {}

pub fn main(argv: &[OsString]) -> ! {
    let Some((program, args)) = argv.split_first() else {
        eprintln!("foil __pid1: no command");
        std::process::exit(125)
    };
    let status = take_env("FOIL_STATUS_FD").and_then(|v| v.parse::<c_int>().ok());
    let relay = take_env("FOIL_RELAY");
    // foil passes its FOIL_GRACE_MS in (a strict run's environment is clean).
    let grace = grace_from_env();
    std::env::remove_var("FOIL_GRACE_MS");
    if let Some(fd) = status {
        // The command must not inherit it.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }

    // Handlers, so the kernel delivers these to PID 1 at all; then block
    // them so they queue on a signalfd instead of running the handler.
    let mut watched = FORWARDED.to_vec();
    watched.push(libc::SIGCHLD);
    let mut mask = unsafe { std::mem::zeroed::<libc::sigset_t>() };
    unsafe {
        libc::sigemptyset(&mut mask);
        for &sig in &watched {
            let mut action = std::mem::zeroed::<libc::sigaction>();
            action.sa_sigaction = ignore as extern "C" fn(c_int) as libc::sighandler_t;
            action.sa_flags = libc::SA_RESTART;
            libc::sigaction(sig, &action, std::ptr::null_mut());
            libc::sigaddset(&mut mask, sig);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut());
    }
    let sfd = unsafe { libc::signalfd(-1, &mask, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK) };
    if sfd == -1 {
        fail(status, "signalfd", io::Error::last_os_error());
    }
    // SAFETY: signalfd just returned this descriptor.
    let sfd = unsafe { OwnedFd::from_raw_fd(sfd) };

    // Threads started now inherit the blocked mask, so signals stay ours.
    if let Some(spec) = relay {
        if let Err(err) = start_relay(&spec) {
            fail(status, "network relay", err);
        }
    }

    // Descriptors foil's caller left open (a folder, a socket) would be
    // ways out of the view: the command gets stdio only. PID 1's own are
    // close-on-exec already; this catches what it inherited.
    unsafe { libc::syscall(libc::SYS_close_range, 3 as libc::c_long, u32::MAX as libc::c_long, libc::CLOSE_RANGE_CLOEXEC as libc::c_long) };
    // SAFETY: isatty only inspects the descriptors.
    let terminal = (0..3).any(|fd| unsafe { libc::isatty(fd) } == 1);

    let mut cmd = Command::new(program);
    cmd.args(args);
    unsafe {
        cmd.pre_exec(move || {
            let empty = std::mem::zeroed::<libc::sigset_t>();
            libc::pthread_sigmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
            // Without a terminal on stdio, a session of its own: the command
            // then has no controlling terminal to push keystrokes into
            // (TIOCSTI) and no way to signal foil's process group (kill(0)).
            // With one, it needs the terminal; see SECURITY.md.
            if !terminal {
                libc::setsid();
            }
            Ok(())
        });
    }
    let child = match cmd.spawn() {
        Ok(child) => child.id() as libc::pid_t,
        Err(err) => {
            eprintln!("foil: {}: {err}", program.to_string_lossy());
            let code = match err.kind() {
                io::ErrorKind::NotFound => 127,
                io::ErrorKind::PermissionDenied => 126,
                _ => 125,
            };
            finish(status, Termination::Exited(code))
        }
    };

    // Until the command exits: pass signals on, reap everything.
    let mut done = None;
    while done.is_none() {
        reap(child, &mut done);
        if done.is_some() {
            break;
        }
        for (sig, code) in wait_signals(&sfd, -1) {
            if sig != libc::SIGCHLD && code != libc::SI_KERNEL {
                // Our unreaped child: its PID can't have been reused.
                unsafe { libc::kill(child, sig) };
            }
        }
    }

    // Leftovers get SIGTERM and the grace period; whatever is still here
    // when PID 1 exits is killed by the kernel.
    if has_children() && !grace.is_zero() {
        unsafe { libc::kill(-1, libc::SIGTERM) };
        let deadline = Instant::now() + grace;
        while has_children() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let hurry = [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM];
            let ms = left.as_millis().clamp(1, 100) as c_int;
            if wait_signals(&sfd, ms).iter().any(|(s, _)| hurry.contains(s)) {
                break;
            }
            reap(child, &mut done);
        }
    }
    finish(status, done.expect("loop ends when the command is done"))
}

/// Reap every exited child without blocking; record the command's end.
fn reap(child: libc::pid_t, done: &mut Option<Termination>) {
    loop {
        let mut raw = 0;
        match unsafe { libc::waitpid(-1, &mut raw, libc::WNOHANG) } {
            0 | -1 => return,
            pid if pid == child => {
                *done = Some(if libc::WIFSIGNALED(raw) {
                    Termination::Signaled(libc::WTERMSIG(raw))
                } else {
                    Termination::Exited(libc::WEXITSTATUS(raw))
                })
            }
            _ => {}
        }
    }
}

/// Whether any process is left in the namespace (all are our children by now).
fn has_children() -> bool {
    let mut raw = 0;
    match unsafe { libc::waitpid(-1, &mut raw, libc::WNOHANG) } {
        -1 => false,
        0 => true,
        // That one had just exited (now reaped); others may remain.
        _ => has_children(),
    }
}

/// Wait up to `ms` milliseconds (-1: forever) for signals; return each one
/// with its si_code.
fn wait_signals(sfd: &OwnedFd, ms: c_int) -> Vec<(c_int, c_int)> {
    let mut fds = [libc::pollfd { fd: sfd.as_raw_fd(), events: libc::POLLIN, revents: 0 }];
    if unsafe { libc::poll(fds.as_mut_ptr(), 1, ms) } <= 0 {
        return Vec::new();
    }
    let size = std::mem::size_of::<libc::signalfd_siginfo>();
    let mut buf = [unsafe { std::mem::zeroed::<libc::signalfd_siginfo>() }; 16];
    let n = unsafe { libc::read(sfd.as_raw_fd(), buf.as_mut_ptr().cast(), size * buf.len()) };
    if n <= 0 {
        return Vec::new();
    }
    buf[..n as usize / size].iter().map(|i| (i.ssi_signo as c_int, i.ssi_code)).collect()
}

/// Report how the command ended on the status pipe, then exit.
fn finish(status: Option<c_int>, how: Termination) -> ! {
    let (report, code) = match how {
        Termination::Exited(code) => ([0u8, code as u8], code),
        Termination::Signaled(sig) => ([1u8, sig as u8], 128 + sig),
    };
    if let Some(fd) = status {
        unsafe { libc::write(fd, report.as_ptr().cast(), 2) };
    }
    std::process::exit(code)
}

fn fail(status: Option<c_int>, what: &str, err: io::Error) -> ! {
    eprintln!("foil: {what}: {err}");
    finish(status, Termination::Exited(125))
}

fn take_env(name: &str) -> Option<String> {
    let value = std::env::var(name).ok();
    std::env::remove_var(name);
    value
}

/// Relay `127.0.0.1:<port>` to the Unix socket at `<path>` (spec "port:path").
fn start_relay(spec: &str) -> io::Result<()> {
    let (port, path) = spec
        .split_once(':')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad FOIL_RELAY"))?;
    let port: u16 = port.parse().map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad relay port"))?;
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let path = path.to_owned();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let path = path.clone();
            std::thread::spawn(move || {
                let _ = relay_one(conn, &path);
            });
        }
    });
    Ok(())
}

fn relay_one(tcp: TcpStream, path: &str) -> io::Result<()> {
    let unix = UnixStream::connect(path)?;
    let (mut tcp_in, mut unix_out) = (tcp.try_clone()?, unix.try_clone()?);
    let upstream = std::thread::spawn(move || {
        let _ = io::copy(&mut tcp_in, &mut unix_out);
        let _ = unix_out.shutdown(Shutdown::Write);
    });
    let (mut unix_in, mut tcp_out) = (unix, tcp);
    let _ = io::copy(&mut unix_in, &mut tcp_out);
    let _ = tcp_out.flush();
    let _ = tcp_out.shutdown(Shutdown::Write);
    let _ = upstream.join();
    Ok(())
}
