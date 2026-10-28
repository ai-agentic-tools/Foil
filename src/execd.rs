//! `foil __execd SOCKET`: OpenSandbox's execution API (execd), served from
//! inside a foil run.
//!
//! In OpenSandbox every sandbox runs a small daemon, execd, that clients use
//! to run commands and read and write files. `foil serve` starts one foil run
//! per sandbox with this as its command, so the daemon, and everything it
//! starts, lives inside the sandbox: the branch's view of the workspace, and
//! whatever `--strict` allows. It listens on a Unix socket that `foil serve`
//! connects to from outside; the access token is checked out there, never in
//! here, so nothing inside the sandbox ever holds it.
//!
//! Implemented: commands (foreground with streamed output, background with
//! status and logs, interrupt), bash sessions, files and directories, and
//! metrics. Not implemented: Jupyter code contexts, PTYs, and the isolated
//! sessions of `/v1/isolated` (a foil sandbox is already one).
//!
//! On macOS a branch is a folder of its own, so paths in the workspace are
//! translated to the branch (`FOIL_EXECD_MAP`), both ways.

use std::collections::HashMap;
use std::ffi::{CStr, OsString};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::http::{self, Error, Request};

/// Largest JSON body accepted.
const MAX_JSON: usize = 16 << 20;
/// Largest upload accepted in one request.
const MAX_UPLOAD: usize = 1 << 30;
/// Background output kept per command; older output is dropped first.
const MAX_LOG: usize = 64 << 20;
/// How often a quiet stream gets a `ping` event.
const PING: Duration = Duration::from_secs(3);
/// How long finished commands are remembered.
const RETAIN: Duration = Duration::from_secs(24 * 3600);

/// The environment variable that maps workspace paths into the branch: `FROM\nTO`.
pub const MAP_ENV: &str = "FOIL_EXECD_MAP";

pub fn main(args: &[OsString]) -> ! {
    let Some(socket) = args.first() else {
        eprintln!("usage: foil __execd SOCKET");
        std::process::exit(2);
    };
    let socket = PathBuf::from(socket);
    let _ = fs::remove_file(&socket);
    let listener = match UnixListener::bind(&socket) {
        Ok(l) => l,
        Err(err) => {
            eprintln!("foil execd: can't listen on {}: {err}", socket.display());
            std::process::exit(1);
        }
    };
    let state = Arc::new(State::new());
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let state = Arc::clone(&state);
        thread::spawn(move || {
            let _ = http::serve_one(conn, |req, reader| route(&state, req, reader));
        });
    }
    std::process::exit(0)
}

struct State {
    /// Workspace path and the branch folder that shows it (macOS).
    map: Option<(PathBuf, PathBuf)>,
    /// Where commands run when the request names no folder.
    home: PathBuf,
    shell: &'static str,
    commands: Mutex<HashMap<String, Arc<Mutex<Cmd>>>>,
    /// Bash sessions, with the process group of each one's shell.
    sessions: Mutex<HashMap<String, (i32, SharedSession)>>,
    next: AtomicU64,
}

/// A command started through `/command`.
struct Cmd {
    content: String,
    pgid: i32,
    background: bool,
    running: bool,
    exit_code: Option<i32>,
    error: Option<String>,
    started: i64,
    finished: Option<(i64, Instant)>,
    /// Background output: stdout and stderr together, as written.
    log: Vec<u8>,
    /// Bytes dropped from the front of `log` when it grew past `MAX_LOG`.
    dropped: u64,
}

impl State {
    fn new() -> State {
        let map = std::env::var(MAP_ENV).ok().and_then(|v| {
            let (from, to) = v.split_once('\n')?;
            Some((PathBuf::from(from), PathBuf::from(to)))
        });
        std::env::remove_var(MAP_ENV);
        let home = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let shell = if Path::new("/bin/bash").exists() { "/bin/bash" } else { "/bin/sh" };
        State {
            map,
            home,
            shell,
            commands: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
        }
    }

    fn new_id(&self, kind: &str) -> String {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        format!("{kind}-{}-{n}", random_hex(6))
    }

    /// The real path for a path the client sent.
    fn real(&self, path: &str) -> Result<PathBuf, Error> {
        if path.is_empty() {
            return Err(Error::bad("empty path"));
        }
        if path.contains('\0') {
            return Err(Error::bad("path contains NUL"));
        }
        let path = if let Some(rest) = path.strip_prefix("~/") {
            home_dir().join(rest)
        } else if path == "~" {
            home_dir()
        } else {
            self.home.join(path)
        };
        let path = normalize(&path);
        Ok(match &self.map {
            Some((from, to)) => match path.strip_prefix(from) {
                Ok(rest) if rest.as_os_str().is_empty() => to.clone(),
                Ok(rest) => to.join(rest),
                Err(_) => path,
            },
            None => path,
        })
    }

    /// The path to show the client for a real path.
    fn shown(&self, path: &Path) -> String {
        let path = match &self.map {
            Some((from, to)) => match path.strip_prefix(to) {
                Ok(rest) if rest.as_os_str().is_empty() => from.clone(),
                Ok(rest) => from.join(rest),
                Err(_) => path.to_owned(),
            },
            None => path.to_owned(),
        };
        path.to_string_lossy().into_owned()
    }

    fn remember(&self, id: &str, cmd: Arc<Mutex<Cmd>>) {
        let mut commands = self.commands.lock().unwrap();
        commands.retain(|_, c| c.lock().unwrap().finished.is_none_or(|(_, at)| at.elapsed() < RETAIN));
        commands.insert(id.to_owned(), cmd);
    }

    fn command(&self, id: &str) -> Result<Arc<Mutex<Cmd>>, Error> {
        self.commands.lock().unwrap().get(id).cloned().ok_or_else(|| Error::not_found(format!("no command {id}")))
    }
}

/// Lexically clean `path`: drop `.`, resolve `..` (never above `/`).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::from("/");
    for part in path.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(p) => out.push(p),
            _ => {}
        }
    }
    out
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

/// `bytes` random bytes, in hex.
pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    if File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)).is_err() {
        let t = http::now_millis() as u64 ^ u64::from(std::process::id());
        buf.iter_mut().enumerate().for_each(|(i, b)| *b = (t >> (i % 8 * 8)) as u8);
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn route(state: &State, req: &Request, reader: &mut BufReader<UnixStream>) -> Result<(), Error> {
    let out = reader.get_ref().try_clone().map_err(Error::from)?;
    let mut out = io::BufWriter::new(out);
    let path = req.path.trim_end_matches('/');
    let segments: Vec<&str> = path.split('/').skip(1).collect();
    let result = match (req.method.as_str(), segments.as_slice()) {
        ("GET", ["ping"]) => http::empty(&mut out, 200).map_err(Error::from),
        ("GET", ["ready"]) => http::json(&mut out, 200, &json!({ "ready": true })).map_err(Error::from),
        ("POST", ["command"]) => run_command(state, req.json(reader, MAX_JSON)?, &mut out),
        ("DELETE", ["command"]) => interrupt(state, req.param("id").ok_or_else(|| Error::bad("id is required"))?, &mut out),
        ("GET", ["command", "status", id]) => command_status(state, id, &mut out),
        ("GET", ["command", id, "logs"]) => command_logs(state, id, req.param("cursor"), &mut out),
        ("POST", ["session"]) => create_session(state, req.json(reader, MAX_JSON)?, &mut out),
        ("POST", ["session", id, "run"]) => run_in_session(state, id, req.json(reader, MAX_JSON)?, &mut out),
        ("DELETE", ["session", id]) => delete_session(state, id, &mut out),
        ("GET", ["files", "info"]) => files_info(state, req, &mut out),
        ("DELETE", ["files"]) => remove_files(state, req, &mut out),
        ("POST", ["files", "permissions"]) => chmod_files(state, req.json(reader, MAX_JSON)?, &mut out),
        ("POST", ["files", "mv"]) => move_files(state, req.json(reader, MAX_JSON)?, &mut out),
        ("GET", ["files", "search"]) => search_files(state, req, &mut out),
        ("POST", ["files", "replace"]) => replace_in_files(state, req, req.json(reader, MAX_JSON)?, &mut out),
        ("POST", ["files", "upload"]) => upload(state, req, reader, &mut out),
        ("GET", ["files", "download"]) => download(state, req, &mut out),
        ("GET", ["directories", "list"]) => list_directory(state, req, &mut out),
        ("POST", ["directories"]) => make_dirs(state, req.json(reader, MAX_JSON)?, &mut out),
        ("DELETE", ["directories"]) => remove_dirs(state, req, &mut out),
        ("GET", ["metrics"]) => http::json(&mut out, 200, &metrics()).map_err(Error::from),
        ("GET", ["v1", "isolated", "capabilities"]) => http::json(
            &mut out,
            200,
            &json!({
                "available": false,
                "isolator": "foil",
                "reason": "a foil sandbox is already a copy-on-write branch; use foil serve's diff and commit",
            }),
        )
        .map_err(Error::from),
        _ => Err(Error::new(501, "NOT_IMPLEMENTED", format!("{} {} is not implemented by foil", req.method, req.path))),
    };
    out.flush().map_err(Error::from)?;
    result
}

// ---- commands ----

fn string_field(body: &Value, name: &str) -> Result<Option<String>, Error> {
    match body.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(Error::bad(format!("{name} must be a string"))),
    }
}

fn int_field(body: &Value, name: &str) -> Result<Option<i64>, Error> {
    match body.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_i64().map(Some).ok_or_else(|| Error::bad(format!("{name} must be an integer"))),
    }
}

/// Commands run as the sandbox's own user; another uid/gid can't be had.
fn check_identity(body: &Value) -> Result<(), Error> {
    // SAFETY: getuid/getgid can't fail.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    if int_field(body, "uid")?.is_some_and(|u| u != i64::from(uid)) || int_field(body, "gid")?.is_some_and(|g| g != i64::from(gid)) {
        return Err(Error::bad(format!("foil runs commands as the sandbox's user (uid {uid}, gid {gid}); uid/gid can't change")));
    }
    Ok(())
}

/// Expand `$NAME`, `${NAME}` and a leading `~` in a working directory.
fn expand(text: &str, envs: &Map<String, Value>) -> Result<String, Error> {
    let lookup = |name: &str| -> Result<String, Error> {
        envs.get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| std::env::var(name).ok())
            .ok_or_else(|| Error::bad(format!("cwd uses undefined variable {name}")))
    };
    let mut out = String::new();
    let mut rest = text;
    if rest == "~" || rest.starts_with("~/") {
        out.push_str(&home_dir().to_string_lossy());
        rest = &rest[1..];
    }
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        if let Some(braced) = after.strip_prefix('{') {
            let end = braced.find('}').ok_or_else(|| Error::bad("unterminated ${ in cwd"))?;
            out.push_str(&lookup(&braced[..end])?);
            rest = &braced[end + 1..];
        } else {
            let end = after.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(after.len());
            if end == 0 {
                out.push('$');
            } else {
                out.push_str(&lookup(&after[..end])?);
            }
            rest = &after[end..];
        }
    }
    out.push_str(rest);
    Ok(out)
}

fn build_command(state: &State, body: &Value) -> Result<(Command, String), Error> {
    check_identity(body)?;
    let empty = Map::new();
    let envs = match body.get("envs") {
        None | Some(Value::Null) => &empty,
        Some(Value::Object(m)) => m,
        Some(_) => return Err(Error::bad("envs must be an object")),
    };
    let command = string_field(body, "command")?;
    let argv = match body.get("argv") {
        None | Some(Value::Null) => None,
        Some(Value::Array(a)) => Some(
            a.iter()
                .map(|v| v.as_str().map(str::to_owned).ok_or_else(|| Error::bad("argv items must be strings")))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Some(_) => return Err(Error::bad("argv must be an array")),
    };
    let (mut cmd, content) = match (command, argv) {
        (Some(c), None) if !c.is_empty() => {
            let mut cmd = Command::new(state.shell);
            if state.shell.ends_with("bash") {
                cmd.args(["--noprofile", "--norc"]);
            }
            cmd.arg("-c").arg(&c);
            (cmd, c)
        }
        (None, Some(a)) if a.first().is_some_and(|a0| !a0.is_empty()) => {
            if a.iter().any(|s| s.contains('\0')) {
                return Err(Error::bad("argv contains NUL"));
            }
            let mut cmd = Command::new(&a[0]);
            cmd.args(&a[1..]);
            (cmd, a.join(" "))
        }
        _ => return Err(Error::bad("give exactly one of command (non-empty) or argv (non-empty argv[0])")),
    };
    for (name, value) in envs {
        let value = value.as_str().ok_or_else(|| Error::bad("envs values must be strings"))?;
        cmd.env(name, value);
    }
    let cwd = match string_field(body, "cwd")? {
        Some(c) if !c.is_empty() => state.real(&expand(&c, envs)?)?,
        _ => state.home.clone(),
    };
    if !cwd.is_dir() {
        return Err(Error::bad(format!("cwd {} is not a directory", state.shown(&cwd))));
    }
    cmd.current_dir(cwd).stdin(Stdio::null()).process_group(0);
    Ok((cmd, content))
}

/// One event of the output stream.
fn event(kind: &str, fields: Value) -> Value {
    let mut ev = json!({ "type": kind, "timestamp": http::now_millis() });
    if let (Some(ev), Value::Object(fields)) = (ev.as_object_mut(), fields) {
        ev.extend(fields);
    }
    ev
}

/// Server-sent events the way execd writes them: one JSON object per event,
/// each followed by a blank line.
struct Events<'a, W: Write> {
    out: &'a mut W,
    started: bool,
}

impl<W: Write> Events<'_, W> {
    fn new(out: &mut W) -> Events<'_, W> {
        Events { out, started: false }
    }

    fn send(&mut self, ev: &Value) -> io::Result<()> {
        if !self.started {
            http::head(self.out, 200, &[("Content-Type", "text/event-stream"), ("Cache-Control", "no-cache"), ("X-Accel-Buffering", "no")])?;
            self.started = true;
        }
        self.out.write_all(ev.to_string().as_bytes())?;
        self.out.write_all(b"\n\n")?;
        self.out.flush()
    }

    fn text(&mut self, kind: &str, text: &str) -> io::Result<()> {
        // One event per line, line endings kept, as execd does.
        for line in text.split_inclusive('\n') {
            self.send(&event(kind, json!({ "text": line })))?;
        }
        Ok(())
    }

    fn ping(&mut self) -> io::Result<()> {
        self.send(&event("ping", json!({ "text": "pong" })))
    }

    /// The end of a run: `execution_complete`, or for a non-zero exit an `error`.
    fn finish(&mut self, exit_code: i32, elapsed: Duration, error: Option<String>) -> io::Result<()> {
        if exit_code == 0 && error.is_none() {
            return self.send(&event("execution_complete", json!({ "execution_time": elapsed.as_millis() as i64, "exit_code": 0 })));
        }
        let message = error.unwrap_or_else(|| format!("command exited with code {exit_code}"));
        self.send(&event(
            "error",
            json!({ "error": { "ename": "CommandExecError", "evalue": exit_code.to_string(), "traceback": [message], "exit_code": exit_code } }),
        ))
    }
}

/// Bytes to text, keeping an incomplete UTF-8 character for the next read.
#[derive(Default)]
struct Utf8Carry(Vec<u8>);

impl Utf8Carry {
    fn push(&mut self, bytes: &[u8]) -> String {
        self.0.extend_from_slice(bytes);
        let keep = match std::str::from_utf8(&self.0) {
            Ok(_) => 0,
            Err(e) if e.error_len().is_none() => self.0.len() - e.valid_up_to(),
            Err(_) => 0,
        };
        let tail = self.0.split_off(self.0.len() - keep);
        let text = String::from_utf8_lossy(&self.0).into_owned();
        self.0 = tail;
        text
    }

    fn rest(&mut self) -> String {
        let text = String::from_utf8_lossy(&self.0).into_owned();
        self.0.clear();
        text
    }
}

enum Output {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Closed,
    Exited(ExitStatus),
}

fn pump(mut from: impl Read, tx: Sender<Output>, wrap: fn(Vec<u8>) -> Output) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match from.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if tx.send(wrap(buf[..n].to_vec())).is_err() {
                    break;
                }
            }
        }
    }
    let _ = tx.send(Output::Closed);
}

fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(-1)
}

fn kill_group(pgid: i32, signal: i32) {
    if pgid > 0 {
        // SAFETY: signals the command's own process group.
        unsafe { libc::kill(-pgid, signal) };
    }
}

fn run_command(state: &State, body: Value, out: &mut impl Write) -> Result<(), Error> {
    let (mut cmd, content) = build_command(state, &body)?;
    let background = body.get("background").and_then(Value::as_bool).unwrap_or(false);
    let timeout = int_field(&body, "timeout")?.filter(|t| *t > 0).map(|t| Duration::from_millis(t as u64));
    let id = state.new_id("cmd");
    let start = Instant::now();
    let record = Arc::new(Mutex::new(Cmd {
        content,
        pgid: 0,
        background,
        running: true,
        exit_code: None,
        error: None,
        started: http::now_secs(),
        finished: None,
        log: Vec::new(),
        dropped: 0,
    }));
    if background {
        // stdout and stderr share one pipe, so the log keeps their order.
        let (read, write) = pipe()?;
        cmd.stdout(Stdio::from(write.try_clone()?)).stderr(Stdio::from(write));
        let spawned = cmd.spawn();
        drop(cmd);
        let mut events = Events::new(out);
        let mut child = match spawned {
            Ok(child) => child,
            Err(err) => {
                let mut r = record.lock().unwrap();
                (r.running, r.exit_code, r.error, r.finished) = (false, Some(255), Some(err.to_string()), Some((http::now_secs(), Instant::now())));
                drop(r);
                state.remember(&id, record);
                return Err(Error::new(500, "RUNTIME_ERROR", format!("failed to start command: {err}")));
            }
        };
        record.lock().unwrap().pgid = child.id() as i32;
        state.remember(&id, Arc::clone(&record));
        let log = Arc::clone(&record);
        thread::spawn(move || {
            let mut file = File::from(read);
            let mut buf = vec![0u8; 64 * 1024];
            while let Ok(n) = file.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let mut r = log.lock().unwrap();
                r.log.extend_from_slice(&buf[..n]);
                if r.log.len() > MAX_LOG {
                    let cut = r.log.len() - MAX_LOG;
                    r.log.drain(..cut);
                    r.dropped += cut as u64;
                }
            }
        });
        let waiter = Arc::clone(&record);
        thread::spawn(move || {
            let killer = timeout.map(|t| {
                let pgid = waiter.lock().unwrap().pgid;
                let watched = Arc::clone(&waiter);
                thread::spawn(move || {
                    thread::sleep(t);
                    if watched.lock().unwrap().running {
                        kill_group(pgid, libc::SIGKILL);
                    }
                })
            });
            let status = child.wait();
            let mut r = waiter.lock().unwrap();
            r.running = false;
            r.finished = Some((http::now_secs(), Instant::now()));
            match status {
                Ok(s) => {
                    r.exit_code = Some(exit_code(s));
                    if !s.success() {
                        r.error = Some(describe(s));
                    }
                }
                Err(err) => (r.exit_code, r.error) = (Some(1), Some(err.to_string())),
            }
            drop(killer);
        });
        events.send(&event("init", json!({ "text": id })))?;
        events.send(&event("execution_complete", json!({ "execution_time": start.elapsed().as_millis() as i64 })))?;
        return Ok(());
    }

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut events = Events::new(out);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            events.send(&event("init", json!({ "text": id })))?;
            events.send(&event(
                "error",
                json!({ "error": { "ename": "CommandExecError", "evalue": err.to_string(), "traceback": [err.to_string()] } }),
            ))?;
            return Ok(());
        }
    };
    let pgid = child.id() as i32;
    record.lock().unwrap().pgid = pgid;
    state.remember(&id, Arc::clone(&record));
    let (tx, rx) = mpsc::channel();
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    let tx1 = tx.clone();
    thread::spawn(move || pump(stdout, tx1, Output::Stdout));
    let tx2 = tx.clone();
    thread::spawn(move || pump(stderr, tx2, Output::Stderr));
    let waited = Arc::new(Mutex::new(Some(child)));
    let waiter = Arc::clone(&waited);
    thread::spawn(move || {
        // Wait without holding the lock, so interrupts can still find the PID.
        let child = waiter.lock().unwrap().take();
        if let Some(mut child) = child {
            if let Ok(status) = child.wait() {
                let _ = tx.send(Output::Exited(status));
            }
        }
    });

    let result = stream_foreground(&mut events, &id, rx, pgid, start, timeout);
    let (code, error) = match &result {
        Ok((code, error)) => (*code, error.clone()),
        Err(err) => (-1, Some(err.to_string())),
    };
    let mut r = record.lock().unwrap();
    r.running = false;
    r.exit_code = Some(code);
    r.error = error.clone();
    r.finished = Some((http::now_secs(), Instant::now()));
    drop(r);
    if result.is_err() {
        // The client went away: stop what it started.
        kill_group(pgid, libc::SIGKILL);
        return Ok(());
    }
    events.finish(code, start.elapsed(), error)?;
    Ok(())
}

/// Stream a foreground command's output until it exits; its exit code and error.
fn stream_foreground<W: Write>(
    events: &mut Events<W>,
    id: &str,
    rx: Receiver<Output>,
    pgid: i32,
    start: Instant,
    timeout: Option<Duration>,
) -> io::Result<(i32, Option<String>)> {
    events.send(&event("init", json!({ "text": id })))?;
    let (mut out, mut err) = (Utf8Carry::default(), Utf8Carry::default());
    let mut open = 2;
    let mut status = None;
    let mut timed_out = false;
    // After the command exits, children it left behind may hold its output
    // open; read what is already there, then stop.
    let mut drain_until: Option<Instant> = None;
    loop {
        let now = Instant::now();
        let mut wait = PING;
        if let Some(deadline) = timeout.map(|t| start + t).filter(|_| !timed_out) {
            wait = wait.min(deadline.saturating_duration_since(now));
        }
        if let Some(until) = drain_until {
            wait = wait.min(until.saturating_duration_since(now));
        }
        match rx.recv_timeout(wait) {
            Ok(Output::Stdout(bytes)) => events.text("stdout", &out.push(&bytes))?,
            Ok(Output::Stderr(bytes)) => events.text("stderr", &err.push(&bytes))?,
            Ok(Output::Closed) => open -= 1,
            Ok(Output::Exited(s)) => {
                status = Some(s);
                drain_until = Some(Instant::now() + Duration::from_millis(200));
            }
            Err(RecvTimeoutError::Timeout) => {
                if drain_until.is_some_and(|u| Instant::now() >= u) {
                    break;
                }
                if timeout.is_some_and(|t| start.elapsed() >= t) && !timed_out {
                    timed_out = true;
                    kill_group(pgid, libc::SIGKILL);
                } else {
                    events.ping()?;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if open == 0 && status.is_some() {
            break;
        }
    }
    events.text("stdout", &out.rest())?;
    events.text("stderr", &err.rest())?;
    let code = status.map_or(-1, exit_code);
    if timed_out {
        return Ok((code, Some(format!("timed out after {} ms", timeout.unwrap_or_default().as_millis()))));
    }
    Ok((code, status.filter(|s| !s.success()).map(describe)))
}

fn describe(status: ExitStatus) -> String {
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exit status {code}"),
        (None, Some(sig)) => format!("signal: {}", signal_name(sig)),
        _ => status.to_string(),
    }
}

fn signal_name(sig: i32) -> String {
    // SAFETY: strsignal returns a static or thread-local string, or NULL.
    let name = unsafe { libc::strsignal(sig) };
    if name.is_null() {
        return sig.to_string();
    }
    // SAFETY: non-NULL strsignal result is a NUL-terminated string.
    unsafe { CStr::from_ptr(name) }.to_string_lossy().to_lowercase()
}

fn interrupt(state: &State, id: &str, out: &mut impl Write) -> Result<(), Error> {
    let cmd = state.command(id)?;
    let r = cmd.lock().unwrap();
    if r.running {
        kill_group(r.pgid, libc::SIGKILL);
    }
    drop(r);
    http::empty(out, 200).map_err(Error::from)
}

fn command_status(state: &State, id: &str, out: &mut impl Write) -> Result<(), Error> {
    let cmd = state.command(id)?;
    let r = cmd.lock().unwrap();
    let mut status = json!({
        "id": id,
        "content": r.content,
        "running": r.running,
        "exit_code": r.exit_code,
        "started_at": http::rfc3339(r.started),
        "finished_at": r.finished.map(|(t, _)| http::rfc3339(t)),
    });
    if let Some(error) = &r.error {
        status["error"] = json!(error);
    }
    drop(r);
    http::json(out, 200, &status).map_err(Error::from)
}

fn command_logs(state: &State, id: &str, cursor: Option<&str>, out: &mut impl Write) -> Result<(), Error> {
    let cmd = state.command(id)?;
    let cursor = match cursor {
        Some(c) => c.parse::<u64>().map_err(|_| Error::bad("cursor must be a non-negative integer"))?,
        None => 0,
    };
    let r = cmd.lock().unwrap();
    if !r.background {
        return Err(Error::bad(format!("{id} ran in the foreground; its output was streamed")));
    }
    let end = r.dropped + r.log.len() as u64;
    let from = cursor.clamp(r.dropped, end);
    let body = r.log[(from - r.dropped) as usize..].to_vec();
    drop(r);
    let end = end.to_string();
    http::send(out, 200, "text/plain; charset=utf-8", &body, &[("EXECD-COMMANDS-TAIL-CURSOR", &end)]).map_err(Error::from)
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: pipe fills two fds on success.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both fds are new and owned by nobody else.
    let (read, write) = unsafe { (std::os::fd::FromRawFd::from_raw_fd(fds[0]), std::os::fd::FromRawFd::from_raw_fd(fds[1])) };
    for fd in fds {
        // SAFETY: setting close-on-exec on our own fd.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    Ok((read, write))
}

// ---- bash sessions ----

type SharedSession = Arc<Mutex<Session>>;

struct Session {
    child: Child,
    stdin: ChildStdin,
    output: Receiver<Output>,
}

impl Drop for Session {
    fn drop(&mut self) {
        kill_group(self.child.id() as i32, libc::SIGKILL);
        let _ = self.child.wait();
    }
}

fn create_session(state: &State, body: Value, out: &mut impl Write) -> Result<(), Error> {
    let cwd = match string_field(&body, "cwd")? {
        Some(c) if !c.is_empty() => state.real(&c)?,
        _ => state.home.clone(),
    };
    if !cwd.is_dir() {
        return Err(Error::bad(format!("cwd {} is not a directory", state.shown(&cwd))));
    }
    let (read, write) = pipe()?;
    let mut cmd = Command::new(state.shell);
    if state.shell.ends_with("bash") {
        cmd.args(["--noprofile", "--norc"]);
    }
    let mut child = cmd
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::from(write.try_clone()?))
        .stderr(Stdio::from(write))
        .process_group(0)
        .spawn()?;
    drop(cmd);
    let stdin = child.stdin.take().expect("piped");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || pump(File::from(read), tx, Output::Stdout));
    let id = state.new_id("session");
    let pgid = child.id() as i32;
    state.sessions.lock().unwrap().insert(id.clone(), (pgid, Arc::new(Mutex::new(Session { child, stdin, output: rx }))));
    http::json(out, 200, &json!({ "session_id": id })).map_err(Error::from)
}

/// Quote `text` for the shell.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

fn run_in_session(state: &State, id: &str, body: Value, out: &mut impl Write) -> Result<(), Error> {
    let session = state.sessions.lock().unwrap().get(id).map(|(_, s)| Arc::clone(s)).ok_or_else(|| Error::not_found(format!("no session {id}")))?;
    let command = string_field(&body, "command")?.ok_or_else(|| Error::bad("command is required"))?;
    let timeout = int_field(&body, "timeout")?.filter(|t| *t > 0).map(|t| Duration::from_millis(t as u64));
    let cwd = match string_field(&body, "cwd")? {
        Some(c) if !c.is_empty() => Some(state.real(&c)?),
        _ => None,
    };
    let Ok(mut s) = session.try_lock() else {
        return Err(Error::new(409, "SESSION_BUSY", format!("session {id} is running another command")));
    };
    let marker = format!("__foil_done_{}__", random_hex(8));
    // The command's own stdin is /dev/null, so it can't eat what follows.
    let mut script = String::new();
    if let Some(cwd) = &cwd {
        script.push_str(&format!("__foil_pwd=$PWD; cd -- {} && ", shell_quote(&cwd.to_string_lossy())));
    }
    script.push_str(&format!("{{ {command}\n}} </dev/null; __foil_status=$?; "));
    if cwd.is_some() {
        script.push_str("cd -- \"$__foil_pwd\"; ");
    }
    script.push_str(&format!("printf '%s%d\\n' {marker} \"$__foil_status\"\n"));
    let start = Instant::now();
    let mut events = Events::new(out);
    if s.stdin.write_all(script.as_bytes()).and_then(|()| s.stdin.flush()).is_err() {
        drop(s);
        state.sessions.lock().unwrap().remove(id);
        return Err(Error::new(410, "SESSION_ENDED", format!("session {id} has ended")));
    }
    events.send(&event("init", json!({ "text": id })))?;
    let mut text = Utf8Carry::default();
    let mut pending = String::new();
    let mut result: Option<i32> = None;
    let mut error = None;
    while result.is_none() {
        let wait = timeout.map_or(PING, |t| PING.min(t.saturating_sub(start.elapsed())));
        match s.output.recv_timeout(wait) {
            Ok(Output::Stdout(bytes)) => {
                pending.push_str(&text.push(&bytes));
                if let Some(at) = pending.find(&marker) {
                    let after = &pending[at + marker.len()..];
                    if let Some(end) = after.find('\n') {
                        result = Some(after[..end].trim().parse().unwrap_or(-1));
                        let before = pending[..at].to_owned();
                        events.text("stdout", &before)?;
                        pending.clear();
                    }
                    continue;
                }
                // Hold back what could be the start of the marker.
                let keep = (0..marker.len().min(pending.len()))
                    .rev()
                    .find(|&n| n > 0 && pending.is_char_boundary(pending.len() - n) && marker.starts_with(&pending[pending.len() - n..]))
                    .unwrap_or(0);
                let emit = pending.len() - keep;
                let ready: String = pending.drain(..emit).collect();
                events.text("stdout", &ready)?;
            }
            Ok(_) | Err(RecvTimeoutError::Disconnected) => {
                // The shell itself exited (`exit`, or killed).
                events.text("stdout", &pending)?;
                let status = s.child.try_wait().ok().flatten().map_or(-1, exit_code);
                result = Some(if status == 0 { 0 } else { status });
                drop(s);
                state.sessions.lock().unwrap().remove(id);
                events.finish(result.unwrap_or(-1), start.elapsed(), None)?;
                return Ok(());
            }
            Err(RecvTimeoutError::Timeout) => {
                if timeout.is_some_and(|t| start.elapsed() >= t) {
                    // There is no telling where the shell is; end the session.
                    events.text("stdout", &pending)?;
                    error = Some(format!("timed out after {} ms; the session was closed", timeout.unwrap_or_default().as_millis()));
                    result = Some(-1);
                    drop(s);
                    state.sessions.lock().unwrap().remove(id);
                    break;
                }
                events.ping()?;
            }
        }
    }
    events.finish(result.unwrap_or(-1), start.elapsed(), error)?;
    Ok(())
}

fn delete_session(state: &State, id: &str, out: &mut impl Write) -> Result<(), Error> {
    let (pgid, session) = state.sessions.lock().unwrap().remove(id).ok_or_else(|| Error::not_found(format!("no session {id}")))?;
    // A run in progress holds the session; killing its shell ends the run,
    // and the session's last holder reaps the shell.
    kill_group(pgid, libc::SIGKILL);
    drop(session);
    http::empty(out, 200).map_err(Error::from)
}

// ---- files ----

fn file_type(meta: &fs::Metadata) -> &'static str {
    let t = meta.file_type();
    if t.is_symlink() {
        "symlink"
    } else if t.is_dir() {
        "directory"
    } else if t.is_file() {
        "file"
    } else {
        "other"
    }
}

fn user_name(uid: u32) -> String {
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result = std::ptr::null_mut();
    // SAFETY: getpwuid_r with our own buffers.
    let rc = unsafe { libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result) };
    if rc != 0 || result.is_null() {
        return uid.to_string();
    }
    // SAFETY: pw_name points into `buf`, NUL-terminated.
    unsafe { CStr::from_ptr(pwd.pw_name) }.to_string_lossy().into_owned()
}

fn group_name(gid: u32) -> String {
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut grp: libc::group = unsafe { std::mem::zeroed() };
    let mut result = std::ptr::null_mut();
    // SAFETY: getgrgid_r with our own buffers.
    let rc = unsafe { libc::getgrgid_r(gid, &mut grp, buf.as_mut_ptr(), buf.len(), &mut result) };
    if rc != 0 || result.is_null() {
        return gid.to_string();
    }
    // SAFETY: gr_name points into `buf`, NUL-terminated.
    unsafe { CStr::from_ptr(grp.gr_name) }.to_string_lossy().into_owned()
}

/// A permission mode written in octal digits, read as a decimal number (`755`).
fn mode_digits(mode: u32) -> u64 {
    format!("{:o}", mode & 0o7777).parse().unwrap_or(0)
}

fn mode_from_digits(digits: i64) -> Result<u32, Error> {
    u32::from_str_radix(&digits.to_string(), 8).ok().filter(|m| *m <= 0o7777).ok_or_else(|| Error::bad(format!("bad mode {digits}: use octal digits like 644")))
}

fn created(meta: &fs::Metadata) -> i64 {
    meta.created().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(meta.ctime(), |d| d.as_secs() as i64)
}

fn file_info(state: &State, path: &Path, meta: &fs::Metadata) -> Value {
    json!({
        "path": state.shown(path),
        "type": file_type(meta),
        "size": meta.len(),
        "modified_at": http::rfc3339(meta.mtime()),
        "created_at": http::rfc3339(created(meta)),
        "owner": user_name(meta.uid()),
        "group": group_name(meta.gid()),
        "mode": mode_digits(meta.mode()),
    })
}

fn files_info(state: &State, req: &Request, out: &mut impl Write) -> Result<(), Error> {
    let paths = req.params("path");
    if paths.is_empty() {
        return Err(Error::bad("path is required"));
    }
    let mut infos = Map::new();
    for p in paths {
        let real = state.real(p)?;
        let meta = fs::symlink_metadata(&real).map_err(|e| Error::not_found(format!("file not found: {p}: {e}")))?;
        infos.insert(p.to_owned(), file_info(state, &real, &meta));
    }
    http::json(out, 200, &Value::Object(infos)).map_err(Error::from)
}

fn remove_files(state: &State, req: &Request, out: &mut impl Write) -> Result<(), Error> {
    for p in req.params("path") {
        let real = state.real(p)?;
        match fs::symlink_metadata(&real) {
            Ok(meta) if meta.is_dir() => return Err(Error::bad(format!("{p} is a directory; delete it with DELETE /directories"))),
            Ok(_) => fs::remove_file(&real)?,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
    }
    http::empty(out, 200).map_err(Error::from)
}

/// Apply a `Permission` object (`mode`, and `owner`/`group` by name) to `path`.
fn apply_permission(path: &Path, perm: &Value) -> Result<(), Error> {
    if let Some(mode) = int_field(perm, "mode")?.filter(|m| *m != 0) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode_from_digits(mode)?))?;
    }
    let owner = string_field(perm, "owner")?.filter(|s| !s.is_empty());
    let group = string_field(perm, "group")?.filter(|s| !s.is_empty());
    if owner.is_some() || group.is_some() {
        let meta = fs::symlink_metadata(path)?;
        let uid = owner.as_deref().map(lookup_user).transpose()?.unwrap_or(meta.uid());
        let gid = group.as_deref().map(lookup_group).transpose()?.unwrap_or(meta.gid());
        if (uid, gid) != (meta.uid(), meta.gid()) {
            std::os::unix::fs::lchown(path, Some(uid), Some(gid))?;
        }
    }
    Ok(())
}

fn lookup_user(name: &str) -> Result<u32, Error> {
    if let Ok(uid) = name.parse() {
        return Ok(uid);
    }
    let cname = std::ffi::CString::new(name).map_err(|_| Error::bad("bad user name"))?;
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result = std::ptr::null_mut();
    // SAFETY: getpwnam_r with our own buffers.
    let rc = unsafe { libc::getpwnam_r(cname.as_ptr(), &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result) };
    if rc != 0 || result.is_null() {
        return Err(Error::bad(format!("no user {name}")));
    }
    Ok(pwd.pw_uid)
}

fn lookup_group(name: &str) -> Result<u32, Error> {
    if let Ok(gid) = name.parse() {
        return Ok(gid);
    }
    let cname = std::ffi::CString::new(name).map_err(|_| Error::bad("bad group name"))?;
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut grp: libc::group = unsafe { std::mem::zeroed() };
    let mut result = std::ptr::null_mut();
    // SAFETY: getgrnam_r with our own buffers.
    let rc = unsafe { libc::getgrnam_r(cname.as_ptr(), &mut grp, buf.as_mut_ptr(), buf.len(), &mut result) };
    if rc != 0 || result.is_null() {
        return Err(Error::bad(format!("no group {name}")));
    }
    Ok(grp.gr_gid)
}

fn object(body: &Value) -> Result<&Map<String, Value>, Error> {
    body.as_object().ok_or_else(|| Error::bad("the body must be a JSON object"))
}

fn chmod_files(state: &State, body: Value, out: &mut impl Write) -> Result<(), Error> {
    for (p, perm) in object(&body)? {
        apply_permission(&state.real(p)?, perm)?;
    }
    http::empty(out, 200).map_err(Error::from)
}

fn move_files(state: &State, body: Value, out: &mut impl Write) -> Result<(), Error> {
    let items = body.as_array().ok_or_else(|| Error::bad("the body must be a JSON array"))?;
    for item in items {
        let src = string_field(item, "src")?.ok_or_else(|| Error::bad("src is required"))?;
        let dest = string_field(item, "dest")?.ok_or_else(|| Error::bad("dest is required"))?;
        let (src_real, dest_real) = (state.real(&src)?, state.real(&dest)?);
        fs::symlink_metadata(&src_real).map_err(|_| Error::not_found(format!("file not found: {src}")))?;
        if !dest_real.parent().is_some_and(Path::is_dir) {
            return Err(Error::not_found(format!("the folder of {dest} doesn't exist")));
        }
        fs::rename(&src_real, &dest_real)?;
    }
    http::empty(out, 200).map_err(Error::from)
}

/// Glob match on a name: `*` (and `**`) any run, `?` one character, `[...]` a class.
fn glob(pattern: &[char], name: &[char]) -> bool {
    match pattern.first() {
        None => name.is_empty(),
        Some('*') => {
            let rest = &pattern[pattern.iter().take_while(|c| **c == '*').count()..];
            (0..=name.len()).any(|i| glob(rest, &name[i..]))
        }
        Some('?') => !name.is_empty() && glob(&pattern[1..], &name[1..]),
        Some('[') => {
            let Some(end) = pattern.iter().skip(1).position(|c| *c == ']').map(|p| p + 1) else {
                return name.first() == Some(&'[') && glob(&pattern[1..], &name[1..]);
            };
            let Some(c) = name.first() else { return false };
            let mut class = &pattern[1..end];
            let negate = matches!(class.first(), Some('!' | '^'));
            if negate {
                class = &class[1..];
            }
            let mut hit = false;
            let mut i = 0;
            while i < class.len() {
                if i + 2 < class.len() && class[i + 1] == '-' {
                    hit |= class[i] <= *c && *c <= class[i + 2];
                    i += 3;
                } else {
                    hit |= class[i] == *c;
                    i += 1;
                }
            }
            hit != negate && glob(&pattern[end + 1..], &name[1..])
        }
        Some('\\') if pattern.len() > 1 => name.first() == Some(&pattern[1]) && glob(&pattern[2..], &name[1..]),
        Some(p) => name.first() == Some(p) && glob(&pattern[1..], &name[1..]),
    }
}

fn search_files(state: &State, req: &Request, out: &mut impl Write) -> Result<(), Error> {
    let root = req.param("path").ok_or_else(|| Error::bad("path is required"))?;
    let pattern: Vec<char> = req.param("pattern").filter(|p| !p.is_empty()).unwrap_or("**").chars().collect();
    let real = state.real(root)?;
    fs::metadata(&real).map_err(|_| Error::not_found(format!("not found: {root}")))?;
    let mut found = Vec::new();
    let mut stack = vec![real];
    while let Some(dir) = stack.pop() {
        let Ok(meta) = fs::symlink_metadata(&dir) else { continue };
        if !meta.is_dir() {
            if meta.is_file() || meta.file_type().is_symlink() {
                let name: Vec<char> = dir.file_name().map(|n| n.to_string_lossy().chars().collect()).unwrap_or_default();
                if glob(&pattern, &name) {
                    found.push(file_info(state, &dir, &meta));
                }
            }
            continue;
        }
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        let mut children: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        children.sort();
        stack.extend(children.into_iter().rev());
    }
    http::json(out, 200, &Value::Array(found)).map_err(Error::from)
}

fn replace_in_files(state: &State, req: &Request, body: Value, out: &mut impl Write) -> Result<(), Error> {
    let mut counts = Map::new();
    for (p, item) in object(&body)? {
        let old = string_field(item, "old")?.filter(|s| !s.is_empty()).ok_or_else(|| Error::bad("old must be a non-empty string"))?;
        let new = string_field(item, "new")?.ok_or_else(|| Error::bad("new is required"))?;
        let real = state.real(p)?;
        let text = fs::read_to_string(&real)?;
        let count = text.matches(&old).count();
        if count > 0 {
            let perms = fs::metadata(&real)?.permissions();
            fs::write(&real, text.replace(&old, &new))?;
            fs::set_permissions(&real, perms)?;
        }
        counts.insert(p.clone(), json!({ "replacedCount": count }));
    }
    if req.param("verbose").is_some_and(|v| v == "true" || v == "1") {
        return http::json(out, 200, &Value::Object(counts)).map_err(Error::from);
    }
    http::empty(out, 200).map_err(Error::from)
}

/// The parts of a `multipart/form-data` body: (headers, content).
fn multipart<'a>(body: &'a [u8], boundary: &str) -> Result<Vec<(String, &'a [u8])>, Error> {
    let delimiter = format!("--{boundary}");
    let d = delimiter.as_bytes();
    let find = |from: usize| body[from..].windows(d.len()).position(|w| w == d).map(|p| p + from);
    let mut parts = Vec::new();
    let mut at = find(0).ok_or_else(|| Error::bad("multipart body without its boundary"))?;
    loop {
        let start = at + d.len();
        if body[start..].starts_with(b"--") {
            break;
        }
        let head_start = start + if body[start..].starts_with(b"\r\n") { 2 } else { 0 };
        let head_end = body[head_start..].windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| Error::bad("malformed multipart part"))? + head_start;
        let next = find(head_end + 4).ok_or_else(|| Error::bad("unterminated multipart body"))?;
        let mut content_end = next;
        if body[..content_end].ends_with(b"\r\n") {
            content_end -= 2;
        }
        let head = String::from_utf8_lossy(&body[head_start..head_end]).into_owned();
        parts.push((head, &body[head_end + 4..content_end]));
        at = next;
    }
    Ok(parts)
}

fn part_name(head: &str) -> Option<String> {
    let disposition = head.lines().find(|l| l.to_ascii_lowercase().starts_with("content-disposition"))?;
    let at = disposition.find("name=\"")? + 6;
    let end = disposition[at..].find('"')? + at;
    Some(disposition[at..end].to_owned())
}

fn upload(state: &State, req: &Request, reader: &mut BufReader<UnixStream>, out: &mut impl Write) -> Result<(), Error> {
    let content_type = req.header("content-type").unwrap_or("");
    let boundary = content_type
        .split(';')
        .find_map(|p| p.trim().strip_prefix("boundary="))
        .map(|b| b.trim_matches('"').to_owned())
        .ok_or_else(|| Error::bad("expected multipart/form-data with a boundary"))?;
    let body = req.body(reader, MAX_UPLOAD)?;
    let mut metadata: Option<Value> = None;
    for (head, content) in multipart(&body, &boundary)? {
        match part_name(&head).as_deref() {
            Some("metadata") => metadata = Some(serde_json::from_slice(content).map_err(|e| Error::bad(format!("bad metadata: {e}")))?),
            Some("file") => {
                let meta = metadata.take().ok_or_else(|| Error::bad("each file part must follow its metadata part"))?;
                let path = string_field(&meta, "path")?.ok_or_else(|| Error::bad("metadata.path is required"))?;
                let real = state.real(&path)?;
                if let Some(parent) = real.parent() {
                    fs::create_dir_all(parent)?;
                }
                write_atomically(&real, content)?;
                apply_permission(&real, &meta)?;
            }
            _ => {}
        }
    }
    http::empty(out, 200).map_err(Error::from)
}

/// Replace `path` with `content` all at once, so readers never see half a file.
fn write_atomically(path: &Path, content: &[u8]) -> io::Result<()> {
    let tmp = path.with_file_name(format!(".{}.foil-upload-{}", path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default(), random_hex(4)));
    let result = (|| {
        let mut file = File::create(&tmp)?;
        file.write_all(content)?;
        if let Ok(meta) = fs::metadata(path) {
            file.set_permissions(meta.permissions())?;
        }
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn download(state: &State, req: &Request, out: &mut impl Write) -> Result<(), Error> {
    let p = req.param("path").ok_or_else(|| Error::bad("path is required"))?;
    let real = state.real(p)?;
    let meta = fs::metadata(&real).map_err(|_| Error::not_found(format!("file not found: {p}")))?;
    if meta.is_dir() {
        return Err(Error::bad(format!("{p} is a directory")));
    }
    let offset = req.param("offset").map(str::parse::<usize>).transpose().map_err(|_| Error::bad("offset must be a positive integer"))?;
    let limit = req.param("limit").map(str::parse::<usize>).transpose().map_err(|_| Error::bad("limit must be a positive integer"))?;
    if offset.is_some() || limit.is_some() {
        if req.header("range").is_some() {
            return Err(Error::bad("offset/limit and Range can't be combined"));
        }
        let skip = offset.unwrap_or(1).max(1) - 1;
        let take = limit.unwrap_or(usize::MAX);
        let mut text = String::new();
        for line in BufReader::new(File::open(&real)?).split(b'\n').skip(skip).take(take) {
            text.push_str(&String::from_utf8_lossy(&line?));
            text.push('\n');
        }
        return http::send(out, 200, "text/plain; charset=utf-8", text.as_bytes(), &[]).map_err(Error::from);
    }
    let size = meta.len();
    let name = real.file_name().map(|n| n.to_string_lossy().replace('"', "")).unwrap_or_default();
    let disposition = format!("attachment; filename=\"{name}\"");
    let mut file = File::open(&real)?;
    let (status, start, len) = match req.header("range") {
        Some(range) => match parse_range(range, size) {
            Some((s, e)) => (206, s, e - s + 1),
            None => {
                return http::send(
                    out,
                    416,
                    "application/json",
                    json!({ "code": "INVALID_RANGE", "message": format!("range {range} is outside the file ({size} bytes)") }).to_string().as_bytes(),
                    &[("Content-Range", &format!("bytes */{size}"))],
                )
                .map_err(Error::from)
            }
        },
        None => (200, 0, size),
    };
    let length = len.to_string();
    let content_range = format!("bytes {start}-{}/{size}", start + len.saturating_sub(1));
    let mut headers = vec![("Content-Type", "application/octet-stream"), ("Content-Length", length.as_str()), ("Content-Disposition", disposition.as_str()), ("Accept-Ranges", "bytes")];
    if status == 206 {
        headers.push(("Content-Range", content_range.as_str()));
    }
    http::head(out, status, &headers)?;
    file.seek(SeekFrom::Start(start))?;
    io::copy(&mut file.take(len), out)?;
    Ok(())
}

/// `bytes=a-b`, `bytes=a-` or `bytes=-n` as an inclusive range within `size`.
fn parse_range(header: &str, size: u64) -> Option<(u64, u64)> {
    let spec = header.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (start, end) = match (a.trim(), b.trim()) {
        ("", n) => {
            let n: u64 = n.parse().ok()?;
            if n == 0 {
                return None;
            }
            (size.saturating_sub(n), size.checked_sub(1)?)
        }
        (a, "") => (a.parse().ok()?, size.checked_sub(1)?),
        (a, b) => (a.parse().ok()?, b.parse::<u64>().ok()?.min(size.checked_sub(1)?)),
    };
    (start <= end && start < size).then_some((start, end))
}

fn list_directory(state: &State, req: &Request, out: &mut impl Write) -> Result<(), Error> {
    let p = req.param("path").ok_or_else(|| Error::bad("path is required"))?;
    let depth = req.param("depth").map(str::parse::<usize>).transpose().map_err(|_| Error::bad("depth must be a non-negative integer"))?.unwrap_or(1);
    let real = state.real(p)?;
    let meta = fs::symlink_metadata(&real).map_err(|_| Error::not_found(format!("not found: {p}")))?;
    if meta.file_type().is_symlink() {
        return Err(Error::bad(format!("{p} is a symbolic link; list the folder it points to")));
    }
    if !meta.is_dir() {
        return Err(Error::bad(format!("{p} is not a directory")));
    }
    let mut entries = Vec::new();
    list_into(state, &real, depth, &mut entries)?;
    http::json(out, 200, &Value::Array(entries)).map_err(Error::from)
}

fn list_into(state: &State, dir: &Path, depth: usize, out: &mut Vec<Value>) -> io::Result<()> {
    if depth == 0 {
        return Ok(());
    }
    let mut children: Vec<_> = fs::read_dir(dir)?.flatten().collect();
    children.sort_by_key(|e| e.file_name());
    for child in children {
        let path = child.path();
        let Ok(meta) = fs::symlink_metadata(&path) else { continue };
        out.push(file_info(state, &path, &meta));
        if meta.is_dir() {
            let _ = list_into(state, &path, depth - 1, out);
        }
    }
    Ok(())
}

fn make_dirs(state: &State, body: Value, out: &mut impl Write) -> Result<(), Error> {
    for (p, perm) in object(&body)? {
        let real = state.real(p)?;
        let existed = real.is_dir();
        fs::create_dir_all(&real)?;
        if !existed {
            apply_permission(&real, perm)?;
        }
    }
    http::empty(out, 200).map_err(Error::from)
}

fn remove_dirs(state: &State, req: &Request, out: &mut impl Write) -> Result<(), Error> {
    for p in req.params("path") {
        let real = state.real(p)?;
        if real == Path::new("/") {
            return Err(Error::bad("refusing to delete /"));
        }
        match fs::remove_dir_all(&real) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
    }
    http::empty(out, 200).map_err(Error::from)
}

// ---- metrics ----

fn metrics() -> Value {
    let cpus = thread::available_parallelism().map_or(1, |n| n.get()) as f64;
    let mut load = [0f64; 3];
    // SAFETY: getloadavg fills up to 3 doubles.
    let got = unsafe { libc::getloadavg(load.as_mut_ptr(), 3) };
    let cpu_used = if got > 0 { (load[0] / cpus * 100.0).min(100.0) } else { 0.0 };
    let (total, used) = memory_mib();
    json!({
        "cpu_count": cpus,
        "cpu_used_pct": cpu_used,
        "mem_total_mib": total,
        "mem_used_mib": used,
        "timestamp": http::now_millis(),
    })
}

#[cfg(target_os = "linux")]
fn memory_mib() -> (f64, f64) {
    let text = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let field = |name: &str| -> f64 {
        text.lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_whitespace().nth(1)?.parse::<f64>().ok())
            .unwrap_or(0.0)
            / 1024.0
    };
    let total = field("MemTotal:");
    (total, total - field("MemAvailable:"))
}

#[cfg(target_os = "macos")]
fn memory_mib() -> (f64, f64) {
    let mut total: u64 = 0;
    let mut size = std::mem::size_of::<u64>();
    // SAFETY: hw.memsize is a u64.
    unsafe { libc::sysctlbyname(c"hw.memsize".as_ptr(), (&mut total as *mut u64).cast(), &mut size, std::ptr::null_mut(), 0) };
    let total = total as f64 / 1_048_576.0;
    // Used memory: everything but free and speculative pages.
    let out = Command::new("/usr/bin/vm_stat").output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    let page = out.split("page size of ").nth(1).and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok()).unwrap_or(16384.0);
    let pages = |name: &str| -> f64 {
        out.lines().find(|l| l.starts_with(name)).and_then(|l| l.rsplit(':').next()?.trim().trim_end_matches('.').parse::<f64>().ok()).unwrap_or(0.0)
    };
    let free = (pages("Pages free") + pages("Pages speculative")) * page / 1_048_576.0;
    (total, (total - free).max(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        let g = |p: &str, n: &str| glob(&p.chars().collect::<Vec<_>>(), &n.chars().collect::<Vec<_>>());
        assert!(g("**", "anything.txt"));
        assert!(g("*.txt", "a.txt"));
        assert!(!g("*.txt", "a.txt.bak"));
        assert!(g("a?c", "abc"));
        assert!(g("[a-c]x", "bx"));
        assert!(!g("[!a-c]x", "bx"));
        assert!(g("\\*", "*"));
    }

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-9", 100), Some((0, 9)));
        assert_eq!(parse_range("bytes=90-", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=-10", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=50-500", 100), Some((50, 99)));
        assert_eq!(parse_range("bytes=100-", 100), None);
        assert_eq!(parse_range("bytes=0-1,4-5", 100), None);
        assert_eq!(parse_range("bytes=0-0", 0), None);
    }

    #[test]
    fn multipart_parts() {
        let body = b"--XX\r\nContent-Disposition: form-data; name=\"metadata\"\r\n\r\n{\"path\":\"/a\"}\r\n--XX\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a\"\r\nContent-Type: application/octet-stream\r\n\r\nhi\r\nthere\r\n--XX--\r\n";
        let parts = multipart(body, "XX").unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(part_name(&parts[0].0).as_deref(), Some("metadata"));
        assert_eq!(parts[0].1, b"{\"path\":\"/a\"}");
        assert_eq!(part_name(&parts[1].0).as_deref(), Some("file"));
        assert_eq!(parts[1].1, b"hi\r\nthere");
    }

    #[test]
    fn utf8_split_across_reads() {
        let mut carry = Utf8Carry::default();
        let bytes = "héllo".as_bytes();
        assert_eq!(carry.push(&bytes[..2]), "h");
        assert_eq!(carry.push(&bytes[2..]), "éllo");
        assert_eq!(carry.rest(), "");
    }

    #[test]
    fn modes_are_octal_digits() {
        assert_eq!(mode_digits(0o100755), 755);
        assert_eq!(mode_from_digits(644).unwrap(), 0o644);
        assert!(mode_from_digits(999).is_err());
    }

    #[test]
    fn working_directories_expand() {
        let mut envs = Map::new();
        envs.insert("X".into(), json!("/tmp"));
        assert_eq!(expand("$X/a/${X}", &envs).unwrap(), "/tmp/a//tmp");
        assert!(expand("$FOIL_SURELY_UNSET_VAR", &envs).is_err());
        assert_eq!(expand("a$", &envs).unwrap(), "a$");
    }

    #[test]
    fn paths_are_mapped_both_ways() {
        let mut state = State::new();
        state.home = PathBuf::from("/w");
        state.map = Some((PathBuf::from("/w"), PathBuf::from("/b/tree")));
        assert_eq!(state.real("x/../y").unwrap(), PathBuf::from("/b/tree/y"));
        assert_eq!(state.real("/w").unwrap(), PathBuf::from("/b/tree"));
        assert_eq!(state.real("/etc/hosts").unwrap(), PathBuf::from("/etc/hosts"));
        assert_eq!(state.shown(Path::new("/b/tree/y")), "/w/y");
        assert_eq!(state.shown(Path::new("/b/tree")), "/w");
    }
}
