//! `foil serve`: OpenSandbox's sandbox API, with foil branches as sandboxes.
//!
//! OpenSandbox clients (its SDKs, CLI and MCP server) create a sandbox, then
//! talk to the execd daemon inside it. Here a sandbox is a foil branch of a
//! workspace with one long-lived foil run in it, whose command is
//! `foil __execd` (see `execd.rs`). This server answers the lifecycle API
//! (`/v1/sandboxes...`) and relays each sandbox's execd API from
//! `/v1/sandboxes/<id>/proxy/44772/...` to the daemon's Unix socket.
//!
//! What a container runtime can't offer comes from foil: a sandbox starts in
//! milliseconds from the real project, needs no image or Docker, works on a
//! laptop and a Mac, and what it changed can be reviewed (`/diff`) and
//! applied to the real project (`/commit`), refusing files changed there
//! meanwhile.
//!
//! Sandboxes outlive the server: their runs are in process groups of their
//! own and are recorded under `$FOIL_HOME/serve`, so a restarted server
//! picks them up again.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::branch::Branch;
use crate::execd::random_hex;
use crate::http::{self, Error, Request};

/// The port OpenSandbox clients use for execd, and for the egress sidecar.
pub const EXECD_PORT: u16 = 44772;
pub const EGRESS_PORT: u16 = 18080;
/// The entrypoint the SDKs send when the caller names none.
const IDLE_ENTRYPOINT: [&str; 3] = ["tail", "-f", "/dev/null"];
/// Largest lifecycle request body.
const MAX_BODY: usize = 1 << 20;
/// Most connections served at once.
const MAX_OPEN: usize = 512;
/// How long a new sandbox's daemon may take to answer.
const START_TIMEOUT: Duration = Duration::from_secs(30);

/// How `foil serve` was started.
pub struct Config {
    pub listen: String,
    /// Workspaces sandboxes may branch (canonical); the first is the default.
    pub workspaces: Vec<PathBuf>,
    /// `foil run` options every sandbox gets (`--strict`, `--memory 2G`, ...).
    pub run_flags: Vec<OsString>,
    pub strict: bool,
    pub no_network: bool,
    /// The key clients must send as `OPEN-SANDBOX-API-KEY`.
    pub api_key: String,
}

#[derive(Clone)]
struct Sandbox {
    id: String,
    branch: String,
    workspace: PathBuf,
    /// The token clients send to this sandbox's execd (`X-EXECD-ACCESS-TOKEN`).
    token: String,
    socket: PathBuf,
    /// The `foil run` holding the sandbox.
    pid: i32,
    state: &'static str,
    reason: Option<String>,
    message: Option<String>,
    transition: i64,
    created: i64,
    expires: Option<i64>,
    metadata: Map<String, Value>,
    image: Value,
    entrypoint: Vec<String>,
    /// Options for this sandbox's run on top of the server's.
    flags: Vec<String>,
    env: Vec<(String, String)>,
    strict: bool,
}

const STATES: [&str; 8] = ["Pending", "Running", "Pausing", "Paused", "Resuming", "Stopping", "Terminated", "Failed"];

fn state_name(name: &str) -> &'static str {
    STATES.iter().find(|s| s.eq_ignore_ascii_case(name)).copied().unwrap_or("Failed")
}

impl Sandbox {
    fn to_json(&self, full: bool) -> Value {
        let mut status = json!({ "state": self.state, "lastTransitionAt": http::rfc3339(self.transition) });
        if let Some(reason) = &self.reason {
            status["reason"] = json!(reason);
        }
        if let Some(message) = &self.message {
            status["message"] = json!(message);
        }
        let mut v = json!({
            "id": self.id,
            "status": status,
            "entrypoint": self.entrypoint,
            "createdAt": http::rfc3339(self.created),
            "metadata": self.metadata,
            "extensions": {
                "foil.branch": self.branch,
                "foil.workspace": self.workspace.to_string_lossy(),
                "foil.strict": self.strict.to_string(),
            },
        });
        if !self.image.is_null() {
            v["image"] = self.image.clone();
        }
        // The SDKs read a missing expiresAt as "no expiry"; null would break them.
        if let Some(expires) = self.expires {
            v["expiresAt"] = json!(http::rfc3339(expires));
        }
        if full {
            v["foil"] = json!({
                "token": self.token,
                "socket": self.socket,
                "pid": self.pid,
                "flags": self.flags,
                "env": self.env.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>(),
                "transition": self.transition,
                "created": self.created,
                "expires": self.expires,
            });
        }
        v
    }

    fn from_json(v: &Value) -> Option<Sandbox> {
        let f = v.get("foil")?;
        let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        let ext = v.get("extensions")?;
        Some(Sandbox {
            id: s(v, "id")?,
            branch: s(ext, "foil.branch")?,
            workspace: PathBuf::from(s(ext, "foil.workspace")?),
            token: s(f, "token")?,
            socket: PathBuf::from(s(f, "socket")?),
            pid: f.get("pid")?.as_i64()? as i32,
            state: state_name(v.get("status")?.get("state")?.as_str()?),
            reason: v.get("status")?.get("reason").and_then(Value::as_str).map(str::to_owned),
            message: v.get("status")?.get("message").and_then(Value::as_str).map(str::to_owned),
            transition: f.get("transition")?.as_i64()?,
            created: f.get("created")?.as_i64()?,
            expires: f.get("expires").and_then(Value::as_i64),
            metadata: v.get("metadata").and_then(Value::as_object).cloned().unwrap_or_default(),
            image: v.get("image").cloned().unwrap_or(Value::Null),
            entrypoint: v.get("entrypoint")?.as_array()?.iter().filter_map(|e| e.as_str().map(str::to_owned)).collect(),
            flags: f.get("flags")?.as_array()?.iter().filter_map(|e| e.as_str().map(str::to_owned)).collect(),
            env: f
                .get("env")?
                .as_array()?
                .iter()
                .filter_map(|p| Some((p.get(0)?.as_str()?.to_owned(), p.get(1)?.as_str()?.to_owned())))
                .collect(),
            strict: s(ext, "foil.strict").is_some_and(|v| v == "true"),
        })
    }

    fn set_state(&mut self, state: &'static str, reason: Option<&str>, message: Option<String>) {
        self.state = state;
        self.reason = reason.map(str::to_owned);
        self.message = message;
        self.transition = http::now_secs();
    }
}

struct Server {
    config: Config,
    exe: PathBuf,
    /// `$FOIL_HOME/serve`: one record and one log per sandbox.
    dir: PathBuf,
    /// A short folder for the daemons' sockets (socket paths are limited to ~100 bytes).
    sockets: PathBuf,
    sandboxes: Mutex<HashMap<String, Sandbox>>,
    /// The runs this server started; adopted ones (from before a restart) aren't here.
    children: Mutex<HashMap<String, Child>>,
}

pub fn main(config: Config) -> io::Result<()> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let dir = crate::branch::home()?.join("serve");
    fs::create_dir_all(&dir)?;
    let sockets = socket_dir()?;
    let listener = TcpListener::bind(&config.listen)
        .map_err(|err| io::Error::new(err.kind(), format!("can't listen on {}: {err}", config.listen)))?;
    let server = Arc::new(Server {
        config,
        exe,
        dir,
        sockets,
        sandboxes: Mutex::new(HashMap::new()),
        children: Mutex::new(HashMap::new()),
    });
    server.recover();
    let addr = listener.local_addr()?;
    eprintln!("foil serve: OpenSandbox API on http://{addr} for {}", server.config.workspaces.iter().map(|w| w.display().to_string()).collect::<Vec<_>>().join(", "));
    eprintln!("foil serve: clients need OPEN_SANDBOX_DOMAIN={addr} OPEN_SANDBOX_API_KEY=<the key>");
    let watcher = Arc::clone(&server);
    thread::spawn(move || watcher.watch());
    let open = Arc::new(AtomicUsize::new(0));
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        if open.load(Ordering::Relaxed) >= MAX_OPEN {
            let mut conn = conn;
            let _ = http::error(&mut conn, &Error::new(503, "BUSY", "too many open connections"));
            continue;
        }
        open.fetch_add(1, Ordering::Relaxed);
        let (server, open) = (Arc::clone(&server), Arc::clone(&open));
        thread::spawn(move || {
            let _ = conn.set_nodelay(true);
            server.handle(conn);
            open.fetch_sub(1, Ordering::Relaxed);
        });
    }
    Ok(())
}

/// `/tmp/foil-<uid>`: ours alone (mode 0700, owned by us, not a link).
fn socket_dir() -> io::Result<PathBuf> {
    // SAFETY: getuid can't fail.
    let uid = unsafe { libc::getuid() };
    let dir = PathBuf::from(format!("/tmp/foil-{uid}"));
    match fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(err),
    }
    let meta = fs::symlink_metadata(&dir)?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err(io::Error::other(format!("{} isn't a private folder of yours; remove it", dir.display())));
    }
    dir.canonicalize()
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

/// Send one HTTP request to a sandbox's daemon; its status and body.
fn ask(socket: &Path, method: &str, path: &str, body: Option<&Value>) -> io::Result<(u16, Vec<u8>)> {
    let mut conn = UnixStream::connect(socket)?;
    conn.set_read_timeout(Some(Duration::from_secs(10)))?;
    let body = body.map(Value::to_string).unwrap_or_default();
    write!(conn, "{method} {path} HTTP/1.1\r\nHost: execd\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len())?;
    let mut raw = Vec::new();
    conn.read_to_end(&mut raw)?;
    let head_end = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| io::Error::other("bad reply from execd"))?;
    let status = std::str::from_utf8(&raw[..head_end])
        .ok()
        .and_then(|h| h.split(' ').nth(1)?.parse().ok())
        .ok_or_else(|| io::Error::other("bad reply from execd"))?;
    Ok((status, raw[head_end + 4..].to_vec()))
}

fn ping(socket: &Path) -> bool {
    matches!(ask(socket, "GET", "/ping", None), Ok((200, _)))
}

/// Pause or continue every process of a sandbox: through the branch's box
/// (a cgroup freeze on Linux, the run's marker on macOS), or on Linux without
/// a box by stopping every descendant of its foil run (a subreaper, so
/// nothing it started escapes the tree).
fn freeze(sandbox: &Sandbox, frozen: bool) -> io::Result<()> {
    let boxed = Branch::open(&sandbox.branch)?.freeze(frozen);
    #[cfg(target_os = "linux")]
    if boxed.is_err() {
        let signal = if frozen { libc::SIGSTOP } else { libc::SIGCONT };
        for pid in descendants(sandbox.pid) {
            // SAFETY: signals a process the sandbox's run started.
            unsafe { libc::kill(pid, signal) };
        }
        return Ok(());
    }
    boxed
}

/// Every process below `root` in the process tree (not `root` itself).
#[cfg(target_os = "linux")]
fn descendants(root: i32) -> Vec<i32> {
    let mut parents = Vec::new();
    for entry in fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else { continue };
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else { continue };
        // The parent follows the command name, which is in parentheses and may hold anything.
        let ppid = stat.rsplit_once(')').and_then(|(_, rest)| rest.split_whitespace().nth(1)?.parse::<i32>().ok());
        if let Some(ppid) = ppid {
            parents.push((pid, ppid));
        }
    }
    let mut found = vec![root];
    let mut i = 0;
    while i < found.len() {
        let parent = found[i];
        found.extend(parents.iter().filter(|(_, pp)| *pp == parent).map(|(p, _)| *p));
        i += 1;
    }
    found.remove(0);
    found
}

/// Compare secrets in time that doesn't depend on where they differ.
fn same_secret(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn uuid() -> String {
    let h = random_hex(16);
    format!("{}-{}-4{}-{}-{}", &h[0..8], &h[8..12], &h[13..16], &h[16..20], &h[20..32])
}

impl Server {
    fn record_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    fn save(&self, sandbox: &Sandbox) {
        let path = self.record_path(&sandbox.id);
        let tmp = path.with_extension("json.tmp");
        if fs::write(&tmp, sandbox.to_json(true).to_string()).and_then(|()| fs::rename(&tmp, &path)).is_err() {
            eprintln!("foil serve: can't save the record of sandbox {}", sandbox.id);
        }
    }

    fn forget(&self, id: &str) {
        let _ = fs::remove_dir_all(self.sockets.join(id.get(..8).unwrap_or(id)));
        let _ = fs::remove_file(self.record_path(id));
        let _ = fs::remove_file(self.dir.join(format!("{id}.log")));
    }

    /// Pick up the sandboxes of an earlier server: adopt runs still going,
    /// start runs again in branches whose run is gone.
    fn recover(&self) {
        let Ok(entries) = fs::read_dir(&self.dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let Some(mut sandbox) = fs::read(&path).ok().and_then(|raw| serde_json::from_slice::<Value>(&raw).ok()).as_ref().and_then(Sandbox::from_json) else {
                continue;
            };
            if Branch::open(&sandbox.branch).is_err() {
                // Committed or aborted from the command line meanwhile.
                self.forget(&sandbox.id);
                continue;
            }
            if alive(sandbox.pid) && ping(&sandbox.socket) {
                eprintln!("foil serve: sandbox {} is still running", sandbox.id);
            } else if sandbox.state == "Running" || sandbox.state == "Paused" {
                match self.start_run(&sandbox) {
                    Ok(child) => {
                        sandbox.pid = child.id() as i32;
                        sandbox.set_state("Running", None, None);
                        self.children.lock().unwrap().insert(sandbox.id.clone(), child);
                        eprintln!("foil serve: sandbox {} started again in branch {}", sandbox.id, sandbox.branch);
                    }
                    Err(err) => sandbox.set_state("Failed", Some("RESTART_FAILED"), Some(err.to_string())),
                }
                self.save(&sandbox);
            }
            self.sandboxes.lock().unwrap().insert(sandbox.id.clone(), sandbox);
        }
    }

    /// Every second: notice runs that ended, and delete expired sandboxes.
    fn watch(&self) {
        loop {
            thread::sleep(Duration::from_secs(1));
            let now = http::now_secs();
            let mut expired = Vec::new();
            let mut ended = Vec::new();
            {
                let mut children = self.children.lock().unwrap();
                let sandboxes = self.sandboxes.lock().unwrap();
                for (id, sandbox) in sandboxes.iter() {
                    if sandbox.expires.is_some_and(|t| t <= now) && sandbox.state != "Stopping" {
                        expired.push(id.clone());
                        continue;
                    }
                    if !matches!(sandbox.state, "Running" | "Paused") {
                        continue;
                    }
                    let gone = match children.get_mut(id) {
                        Some(child) => match child.try_wait() {
                            Ok(Some(status)) => Some(status.to_string()),
                            _ => None,
                        },
                        None => (!alive(sandbox.pid)).then(|| "it is no longer running".to_owned()),
                    };
                    if let Some(how) = gone {
                        ended.push((id.clone(), how));
                    }
                }
                for (id, _) in &ended {
                    children.remove(id);
                }
            }
            for (id, how) in ended {
                let mut sandboxes = self.sandboxes.lock().unwrap();
                if let Some(sandbox) = sandboxes.get_mut(&id) {
                    if matches!(sandbox.state, "Running" | "Paused") {
                        let log = fs::read_to_string(self.dir.join(format!("{id}.log"))).unwrap_or_default();
                        let tail: Vec<&str> = log.lines().rev().take(5).collect();
                        let detail = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
                        sandbox.set_state("Failed", Some("RUN_EXITED"), Some(format!("the sandbox's foil run ended ({how}). {detail}").trim().to_owned()));
                        self.save(sandbox);
                    }
                }
            }
            for id in expired {
                if let Err(err) = self.delete(&id) {
                    eprintln!("foil serve: can't delete expired sandbox {id}: {}", err.message);
                }
            }
        }
    }

    fn handle(&self, conn: TcpStream) {
        let mut reader = BufReader::new(match conn.try_clone() {
            Ok(c) => c,
            Err(_) => return,
        });
        let mut out = conn;
        let req = match Request::read(&mut reader) {
            Ok(r) => r,
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return,
            Err(err) => {
                let _ = http::error(&mut out, &Error::bad(err.to_string()));
                return;
            }
        };
        let path = req.path.trim_end_matches('/').to_owned();
        let segments: Vec<&str> = path.split('/').skip(1).collect();
        // The execd relay: authorized by the sandbox's token or the API key.
        if let ["v1", "sandboxes", id, "proxy", port, ..] = segments.as_slice() {
            let prefix_len = format!("/v1/sandboxes/{id}/proxy/{port}").len();
            let result = self.relay(id, port, &req, prefix_len, reader, &mut out);
            if let Err(err) = result {
                let _ = http::error(&mut out, &err);
            }
            let _ = out.shutdown(Shutdown::Both);
            return;
        }
        let result = self.lifecycle(&req, &segments, &mut reader, &mut out);
        if let Err(err) = result {
            let _ = http::error(&mut out, &err);
        }
    }

    fn check_key(&self, req: &Request) -> Result<(), Error> {
        match req.header("OPEN-SANDBOX-API-KEY") {
            Some(key) if same_secret(key, &self.config.api_key) => Ok(()),
            _ => Err(Error::new(401, "UNAUTHORIZED", "missing or wrong OPEN-SANDBOX-API-KEY")),
        }
    }

    fn get(&self, id: &str) -> Result<Sandbox, Error> {
        self.sandboxes.lock().unwrap().get(id).cloned().ok_or_else(|| Error::not_found(format!("no sandbox {id}")))
    }

    fn lifecycle(&self, req: &Request, segments: &[&str], reader: &mut BufReader<TcpStream>, out: &mut TcpStream) -> Result<(), Error> {
        let segments = match segments {
            ["health"] => return http::json(out, 200, &json!({ "status": "healthy" })).map_err(Error::from),
            ["v1", rest @ ..] => rest,
            rest => rest,
        };
        self.check_key(req)?;
        let reply = |out: &mut TcpStream, status: u16, v: Value| http::json(out, status, &v).map_err(Error::from);
        match (req.method.as_str(), segments) {
            ("POST", ["sandboxes"]) => {
                let body = req.json(reader, MAX_BODY)?;
                let sandbox = self.create(&body)?;
                reply(out, 202, sandbox.to_json(false))
            }
            ("GET", ["sandboxes"]) => reply(out, 200, self.list(req)?),
            ("GET", ["sandboxes", id]) => reply(out, 200, self.get(id)?.to_json(false)),
            ("DELETE", ["sandboxes", id]) => {
                self.delete(id)?;
                http::empty(out, 204).map_err(Error::from)
            }
            ("POST", ["sandboxes", id, "pause"]) => {
                self.pause(id, true)?;
                http::empty(out, 202).map_err(Error::from)
            }
            ("POST", ["sandboxes", id, "resume"]) => {
                self.pause(id, false)?;
                http::empty(out, 202).map_err(Error::from)
            }
            ("POST", ["sandboxes", id, "renew-expiration"]) => {
                let body = req.json(reader, MAX_BODY)?;
                let text = body.get("expiresAt").and_then(Value::as_str).ok_or_else(|| Error::bad("expiresAt is required"))?;
                let when = http::parse_rfc3339(text).ok_or_else(|| Error::bad(format!("bad expiresAt {text}: use RFC 3339")))?;
                if when <= http::now_secs() {
                    return Err(Error::bad("expiresAt must be in the future"));
                }
                let mut sandboxes = self.sandboxes.lock().unwrap();
                let sandbox = sandboxes.get_mut(*id).ok_or_else(|| Error::not_found(format!("no sandbox {id}")))?;
                sandbox.expires = Some(when);
                self.save(sandbox);
                reply(out, 200, json!({ "expiresAt": http::rfc3339(when) }))
            }
            ("PATCH", ["sandboxes", id, "metadata"]) => {
                let body = req.json(reader, MAX_BODY)?;
                let patch = body.as_object().ok_or_else(|| Error::bad("the body must be a JSON object"))?;
                let mut sandboxes = self.sandboxes.lock().unwrap();
                let sandbox = sandboxes.get_mut(*id).ok_or_else(|| Error::not_found(format!("no sandbox {id}")))?;
                for (k, v) in patch {
                    if k.starts_with("opensandbox.io/") {
                        return Err(Error::new(400, "INVALID_METADATA_LABEL", format!("metadata key '{k}' is reserved")));
                    }
                    match v {
                        Value::Null => {
                            sandbox.metadata.remove(k);
                        }
                        Value::String(s) => {
                            sandbox.metadata.insert(k.clone(), json!(s));
                        }
                        other => {
                            sandbox.metadata.insert(k.clone(), json!(other.to_string()));
                        }
                    }
                }
                self.save(sandbox);
                reply(out, 200, sandbox.to_json(false))
            }
            ("GET", ["sandboxes", id, "endpoints", port]) => {
                let port: u16 = port.parse().map_err(|_| Error::bad(format!("bad port {port}")))?;
                let sandbox = self.get(id)?;
                let host = req.header("host").map_or_else(|| self.config.listen.clone(), str::to_owned);
                if port == EXECD_PORT || port == EGRESS_PORT {
                    return reply(
                        out,
                        200,
                        json!({ "endpoint": format!("{host}/v1/sandboxes/{id}/proxy/{port}"), "headers": { "X-EXECD-ACCESS-TOKEN": sandbox.token } }),
                    );
                }
                // Other ports: a sandbox with the host's network serves them directly.
                if sandbox.strict || self.config.no_network {
                    return Err(Error::new(
                        409,
                        "NO_INBOUND_NETWORK",
                        format!("sandbox {id} has no network of its own to listen on (it is strict or offline)"),
                    ));
                }
                let hostname = host.rsplit_once(':').map_or(host.as_str(), |(h, _)| h);
                reply(out, 200, json!({ "endpoint": format!("{hostname}:{port}") }))
            }
            ("GET", ["sandboxes", id, "diff"]) => {
                let sandbox = self.get(id)?;
                let branch = Branch::open(&sandbox.branch)?;
                let all = req.param("all").is_some_and(|v| v == "true" || v == "1");
                if req.param("patch").is_some_and(|v| v == "true" || v == "1") {
                    let mut patch = Vec::new();
                    branch.write_patch(&mut patch, all)?;
                    return http::send(out, 200, "text/x-diff; charset=utf-8", &patch, &[]).map_err(Error::from);
                }
                let (changes, hidden) = branch.changes(all)?;
                let changes: Vec<Value> =
                    changes.iter().map(|c| json!({ "kind": c.kind.letter().to_string(), "path": c.path.to_string_lossy() })).collect();
                reply(out, 200, json!({ "changes": changes, "ignored": hidden }))
            }
            ("POST", ["sandboxes", id, "commit"]) => {
                let body = req.json(reader, MAX_BODY)?;
                let flag = |name: &str| body.get(name).and_then(Value::as_bool).unwrap_or(false);
                let applied = self.commit(id, flag("force"), flag("all"))?;
                reply(out, 200, json!({ "applied": applied }))
            }
            ("GET", ["sandboxes", id, "logs"]) => {
                // What the sandbox's foil run printed (its own problems, not the commands' output).
                self.get(id)?;
                let tail: usize = req.param("tail").and_then(|t| t.parse().ok()).unwrap_or(100);
                let log = fs::read_to_string(self.dir.join(format!("{id}.log"))).unwrap_or_default();
                let lines: Vec<&str> = log.lines().collect();
                let text = lines[lines.len().saturating_sub(tail)..].join("\n");
                http::send(out, 200, "text/plain; charset=utf-8", text.as_bytes(), &[]).map_err(Error::from)
            }
            ("POST", ["metrics", "events"]) => {
                let _ = req.body(reader, MAX_BODY);
                http::empty(out, 204).map_err(Error::from)
            }
            _ => Err(Error::new(501, "NOT_IMPLEMENTED", format!("{} {} is not implemented by foil serve", req.method, req.path))),
        }
    }

    fn list(&self, req: &Request) -> Result<Value, Error> {
        let states: Vec<&str> = req.params("state");
        let metadata = http::parse_query(req.param("metadata").unwrap_or(""));
        let page: usize = req.param("page").map(str::parse).transpose().map_err(|_| Error::bad("bad page"))?.unwrap_or(1).max(1);
        let size: usize = req.param("pageSize").map(str::parse).transpose().map_err(|_| Error::bad("bad pageSize"))?.unwrap_or(20).clamp(1, 200);
        let sandboxes = self.sandboxes.lock().unwrap();
        let mut items: Vec<&Sandbox> = sandboxes
            .values()
            .filter(|s| states.is_empty() || states.iter().any(|st| st.eq_ignore_ascii_case(s.state)))
            .filter(|s| metadata.iter().all(|(k, v)| s.metadata.get(k).and_then(Value::as_str) == Some(v.as_str())))
            .collect();
        items.sort_by(|a, b| b.created.cmp(&a.created).then(a.id.cmp(&b.id)));
        let total = items.len();
        let pages = total.div_ceil(size);
        let shown: Vec<Value> = items.into_iter().skip((page - 1) * size).take(size).map(|s| s.to_json(false)).collect();
        Ok(json!({
            "items": shown,
            "pagination": { "page": page, "pageSize": size, "totalItems": total, "totalPages": pages, "hasNextPage": page < pages },
        }))
    }

    /// Turn an OpenSandbox egress policy into `--allow-host` options (and `--strict`).
    fn network_flags(policy: &Value) -> Result<Vec<String>, Error> {
        if policy.is_null() || policy.as_object().is_some_and(Map::is_empty) {
            return Ok(Vec::new());
        }
        let default = policy.get("defaultAction").and_then(Value::as_str).unwrap_or("deny");
        let rules = policy.get("egress").and_then(Value::as_array).cloned().unwrap_or_default();
        let mut hosts = Vec::new();
        for rule in &rules {
            let action = rule.get("action").and_then(Value::as_str).unwrap_or("allow");
            if action == "deny" {
                if default == "allow" {
                    return Err(Error::bad("foil enforces allowlists only: a deny rule needs defaultAction deny"));
                }
                continue;
            }
            let target = rule.get("target").and_then(Value::as_str).ok_or_else(|| Error::bad("foil needs a target in every allow rule"))?;
            match rule.get("ports").and_then(Value::as_array) {
                Some(ports) => {
                    for port in ports {
                        let port = port.as_u64().ok_or_else(|| Error::bad("ports must be numbers"))?;
                        hosts.push(format!("{target}:{port}"));
                    }
                }
                None => hosts.push(target.to_owned()),
            }
        }
        if default == "allow" {
            hosts = vec!["*".to_owned()];
        }
        if let Some(bad) = hosts.iter().find(|h| !crate::proxy::valid_allow_entry(h)) {
            return Err(Error::bad(format!("foil can't allow {bad}: use a host name, *.domain or an IP address (no CIDR ranges)")));
        }
        let mut flags = vec!["--strict".to_owned()];
        for host in hosts {
            flags.push("--allow-host".to_owned());
            flags.push(host);
        }
        Ok(flags)
    }

    fn create(&self, body: &Value) -> Result<Sandbox, Error> {
        if !body.is_object() {
            return Err(Error::bad("the body must be a JSON object"));
        }
        for unsupported in ["volumes", "snapshotId", "lifecycle"] {
            if body.get(unsupported).is_some_and(|v| !v.is_null() && v != &json!([]) && v != &json!({})) {
                return Err(Error::new(400, "NOT_SUPPORTED", format!("foil serve doesn't support {unsupported}")));
            }
        }
        if body.get("credentialProxy").and_then(|c| c.get("enabled")).and_then(Value::as_bool) == Some(true) {
            return Err(Error::new(400, "NOT_SUPPORTED", "foil serve doesn't support credentialProxy; start foil serve with --inject-key"));
        }
        let extensions = body.get("extensions").and_then(Value::as_object);
        let workspace = match extensions.and_then(|e| e.get("foil.workspace")).and_then(Value::as_str) {
            Some(w) => {
                let w = Path::new(w).canonicalize().map_err(|e| Error::bad(format!("foil.workspace {w}: {e}")))?;
                if !self.config.workspaces.contains(&w) {
                    return Err(Error::new(403, "WORKSPACE_NOT_SERVED", format!("{} isn't a workspace this server serves", w.display())));
                }
                w
            }
            None => self.config.workspaces[0].clone(),
        };
        let mut flags = Server::network_flags(body.get("networkPolicy").unwrap_or(&Value::Null))?;
        let strict = self.config.strict || !flags.is_empty();
        if !flags.is_empty() && self.config.strict {
            flags.remove(0);
        }
        let env: Vec<(String, String)> = match body.get("env") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Object(m)) => m
                .iter()
                .map(|(k, v)| match v {
                    Value::String(s) => Ok((k.clone(), s.clone())),
                    _ => Err(Error::bad("env values must be strings")),
                })
                .collect::<Result<_, _>>()?,
            Some(_) => return Err(Error::bad("env must be an object")),
        };
        if env.iter().any(|(k, _)| k.is_empty() || k.contains('=') || k.contains('\0')) {
            return Err(Error::bad("bad env variable name"));
        }
        let metadata = match body.get("metadata") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(m)) => m.clone(),
            Some(_) => return Err(Error::bad("metadata must be an object")),
        };
        let entrypoint: Vec<String> = match body.get("entrypoint") {
            None | Some(Value::Null) => IDLE_ENTRYPOINT.map(str::to_owned).to_vec(),
            Some(Value::Array(a)) => a.iter().map(|v| v.as_str().map(str::to_owned).ok_or_else(|| Error::bad("entrypoint items must be strings"))).collect::<Result<_, _>>()?,
            Some(_) => return Err(Error::bad("entrypoint must be an array")),
        };
        let expires = match body.get("timeout") {
            None | Some(Value::Null) => None,
            Some(t) => Some(http::now_secs() + t.as_i64().filter(|t| *t > 0).ok_or_else(|| Error::bad("timeout must be a positive number of seconds"))?),
        };

        let id = uuid();
        let branch = format!("osb-{}", &id[..8]);
        let mut sandbox = Sandbox {
            id: id.clone(),
            branch: branch.clone(),
            workspace: workspace.clone(),
            token: random_hex(16),
            // A folder per sandbox: a strict view shows the socket's folder,
            // and must not show the other sandboxes' sockets.
            socket: self.sockets.join(&id[..8]).join("execd.sock"),
            pid: 0,
            state: "Pending",
            reason: None,
            message: None,
            transition: http::now_secs(),
            created: http::now_secs(),
            expires,
            metadata,
            image: body.get("image").cloned().unwrap_or(Value::Null),
            entrypoint: entrypoint.clone(),
            flags,
            env,
            strict,
        };
        Branch::open_or_create(&branch, &workspace)?;
        let mut child = match self.start_run(&sandbox) {
            Ok(child) => child,
            Err(err) => {
                let _ = Branch::open(&branch).and_then(|b| b.abort(false));
                return Err(Error::new(500, "SANDBOX_START_FAILED", err.to_string()));
            }
        };
        sandbox.pid = child.id() as i32;
        if let Err(err) = self.wait_ready(&sandbox, &mut child) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = Branch::open(&branch).and_then(|b| b.abort(false));
            let _ = fs::remove_file(&sandbox.socket);
            self.forget(&id);
            return Err(Error::new(500, "SANDBOX_START_FAILED", err));
        }
        // A real entrypoint keeps running in the background, as a container's would.
        if !entrypoint.is_empty() && entrypoint != IDLE_ENTRYPOINT {
            if let Err(err) = ask(&sandbox.socket, "POST", "/command", Some(&json!({ "argv": entrypoint, "background": true }))) {
                eprintln!("foil serve: sandbox {id}: entrypoint didn't start: {err}");
            }
        }
        sandbox.set_state("Running", None, None);
        self.save(&sandbox);
        self.children.lock().unwrap().insert(id.clone(), child);
        self.sandboxes.lock().unwrap().insert(id, sandbox.clone());
        Ok(sandbox)
    }

    /// Start the sandbox's foil run, with execd as its command.
    fn start_run(&self, sandbox: &Sandbox) -> io::Result<Child> {
        let log = File::create(self.dir.join(format!("{}.log", sandbox.id)))?;
        let mut cmd = Command::new(&self.exe);
        cmd.arg("run").arg("-b").arg(&sandbox.branch).arg("-w").arg(&sandbox.workspace);
        cmd.args(&self.config.run_flags).args(&sandbox.flags);
        #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
        let mut env = sandbox.env.clone();
        #[cfg(target_os = "macos")]
        {
            // A macOS branch is a folder of its own; execd shows it at the workspace's path.
            let tree = Branch::open(&sandbox.branch)?.overlay()?.tree;
            env.push((crate::execd::MAP_ENV.to_owned(), format!("{}\n{}", sandbox.workspace.display(), tree.display())));
        }
        if sandbox.strict {
            cmd.arg("--allow-read").arg(&self.exe);
            for (k, v) in &env {
                cmd.arg("--setenv").arg(format!("{k}={v}"));
            }
        } else {
            cmd.envs(env);
        }
        // Never the server's own credentials, even in a sandbox that keeps foil's environment.
        cmd.env_remove("OPEN_SANDBOX_API_KEY");
        cmd.arg("--listen-socket").arg(&sandbox.socket).arg("--").arg(&self.exe).arg("__execd").arg(&sandbox.socket);
        let _ = fs::remove_file(&sandbox.socket);
        if let Some(dir) = sandbox.socket.parent() {
            match fs::DirBuilder::new().mode(0o700).create(dir) {
                Err(err) if err.kind() != io::ErrorKind::AlreadyExists => return Err(err),
                _ => {}
            }
        }
        cmd.current_dir(&sandbox.workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            // Its own process group: stopping the server leaves sandboxes running.
            .process_group(0)
            .spawn()
    }

    fn wait_ready(&self, sandbox: &Sandbox, child: &mut Child) -> Result<(), String> {
        let start = Instant::now();
        loop {
            if ping(&sandbox.socket) {
                return Ok(());
            }
            if let Ok(Some(status)) = child.try_wait() {
                let log = fs::read_to_string(self.dir.join(format!("{}.log", sandbox.id))).unwrap_or_default();
                return Err(format!("the sandbox's foil run ended ({status}): {}", log.trim()));
            }
            if start.elapsed() > START_TIMEOUT {
                return Err("the sandbox's execd didn't answer in time".to_owned());
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Stop a sandbox's run: SIGTERM, then SIGKILL after a grace period.
    fn stop_run(&self, sandbox: &Sandbox) {
        let child = self.children.lock().unwrap().remove(&sandbox.id);
        if sandbox.state == "Paused" {
            let _ = freeze(sandbox, false);
        }
        if alive(sandbox.pid) {
            // SAFETY: signals the foil run this server started (or adopted).
            unsafe { libc::kill(sandbox.pid, libc::SIGTERM) };
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut child = child;
        loop {
            let done = match child.as_mut() {
                Some(c) => !matches!(c.try_wait(), Ok(None)),
                None => !alive(sandbox.pid),
            };
            if done {
                break;
            }
            if Instant::now() > deadline {
                // SAFETY: as above.
                unsafe { libc::kill(sandbox.pid, libc::SIGKILL) };
                if let Some(c) = child.as_mut() {
                    let _ = c.wait();
                }
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let _ = fs::remove_file(&sandbox.socket);
    }

    /// Wait for the branch's lock to be free (the run's foil cleaning up).
    fn open_unlocked(name: &str) -> io::Result<Branch> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let branch = Branch::open(name)?;
            match branch.lock() {
                Ok(lock) => {
                    drop(lock);
                    return Ok(branch);
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
                Err(err) => return Err(err),
            }
        }
    }

    /// Delete a sandbox: stop its run and discard its branch.
    fn delete(&self, id: &str) -> Result<(), Error> {
        let sandbox = {
            let mut sandboxes = self.sandboxes.lock().unwrap();
            let sandbox = sandboxes.get_mut(id).ok_or_else(|| Error::not_found(format!("no sandbox {id}")))?;
            if sandbox.state == "Stopping" {
                return Err(Error::new(409, "STOPPING", format!("sandbox {id} is being deleted")));
            }
            let before = sandbox.clone();
            sandbox.set_state("Stopping", None, None);
            before
        };
        self.stop_run(&sandbox);
        match Server::open_unlocked(&sandbox.branch).and_then(|b| b.abort(false)) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                let mut sandboxes = self.sandboxes.lock().unwrap();
                if let Some(s) = sandboxes.get_mut(id) {
                    s.set_state("Failed", Some("DELETE_FAILED"), Some(err.to_string()));
                    self.save(s);
                }
                return Err(Error::new(500, "DELETE_FAILED", err.to_string()));
            }
        }
        self.sandboxes.lock().unwrap().remove(id);
        self.forget(id);
        Ok(())
    }

    fn pause(&self, id: &str, pause: bool) -> Result<(), Error> {
        let mut sandboxes = self.sandboxes.lock().unwrap();
        let sandbox = sandboxes.get_mut(id).ok_or_else(|| Error::not_found(format!("no sandbox {id}")))?;
        let (from, to) = if pause { ("Running", "Paused") } else { ("Paused", "Running") };
        if sandbox.state != from {
            return Err(Error::new(409, "INVALID_STATE", format!("sandbox {id} is {}, not {from}", sandbox.state)));
        }
        freeze(sandbox, pause)?;
        sandbox.set_state(to, None, None);
        self.save(sandbox);
        Ok(())
    }

    /// Apply a sandbox's changes to its workspace; the sandbox ends with it.
    fn commit(&self, id: &str, force: bool, all: bool) -> Result<usize, Error> {
        let sandbox = self.get(id)?;
        if !matches!(sandbox.state, "Running" | "Paused" | "Failed") {
            return Err(Error::new(409, "INVALID_STATE", format!("sandbox {id} is {}", sandbox.state)));
        }
        // Refuse before stopping anything, so a refused commit leaves the sandbox as it was.
        let conflicts = Branch::open(&sandbox.branch)?.conflicts(all)?;
        if !conflicts.is_empty() && !force {
            let paths: Vec<String> = conflicts.iter().map(|p| p.to_string_lossy().into_owned()).collect();
            return Err(Error::new(
                409,
                "CONFLICT",
                format!("changed in {} since the sandbox started: {}; commit with force to overwrite", sandbox.workspace.display(), paths.join(", ")),
            ));
        }
        {
            let mut sandboxes = self.sandboxes.lock().unwrap();
            if let Some(s) = sandboxes.get_mut(id) {
                s.set_state("Stopping", Some("COMMITTING"), None);
            }
        }
        self.stop_run(&sandbox);
        match Server::open_unlocked(&sandbox.branch).and_then(|b| b.commit(force, all)) {
            Ok(applied) => {
                self.sandboxes.lock().unwrap().remove(id);
                self.forget(id);
                Ok(applied)
            }
            Err(err) => {
                // Keep the sandbox usable: start its run again.
                let mut restarted = sandbox.clone();
                match self.start_run(&restarted) {
                    Ok(child) => {
                        restarted.pid = child.id() as i32;
                        restarted.set_state("Running", None, None);
                        self.children.lock().unwrap().insert(id.to_owned(), child);
                    }
                    Err(e) => restarted.set_state("Failed", Some("RESTART_FAILED"), Some(e.to_string())),
                }
                self.save(&restarted);
                self.sandboxes.lock().unwrap().insert(id.to_owned(), restarted);
                Err(Error::new(409, "COMMIT_FAILED", err.to_string()))
            }
        }
    }

    /// Relay a request to a sandbox's execd and its reply back, byte for byte.
    fn relay(&self, id: &str, port: &str, req: &Request, prefix_len: usize, reader: BufReader<TcpStream>, out: &mut TcpStream) -> Result<(), Error> {
        let sandbox = self.get(id)?;
        let token_ok = req.header("X-EXECD-ACCESS-TOKEN").is_some_and(|t| same_secret(t, &sandbox.token));
        if !token_ok {
            self.check_key(req)?;
        }
        if port != EXECD_PORT.to_string() {
            return Err(Error::new(501, "NOT_IMPLEMENTED", format!("foil serve relays execd (port {EXECD_PORT}) only; foil's proxy enforces egress (--allow-host)")));
        }
        match sandbox.state {
            "Running" => {}
            "Paused" => return Err(Error::new(409, "SANDBOX_PAUSED", format!("sandbox {id} is paused; resume it first"))),
            state => return Err(Error::new(409, "SANDBOX_NOT_RUNNING", format!("sandbox {id} is {state}"))),
        }
        let mut upstream = UnixStream::connect(&sandbox.socket).map_err(|e| Error::new(502, "EXECD_UNREACHABLE", format!("sandbox {id}'s execd: {e}")))?;
        // The request again, minus the prefix and the credentials: nothing
        // inside the sandbox sees a key or token.
        let target = &req.target[prefix_len.min(req.target.len())..];
        let target = if target.is_empty() || target.starts_with('?') { format!("/{target}") } else { target.to_owned() };
        let mut head = format!("{} {target} HTTP/1.1\r\n", req.method);
        for (name, value) in &req.headers {
            if ["open-sandbox-api-key", "x-execd-access-token", "connection", "proxy-connection", "keep-alive"].iter().any(|h| name.eq_ignore_ascii_case(h)) {
                continue;
            }
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("Connection: close\r\n\r\n");
        upstream.write_all(head.as_bytes())?;
        upstream.write_all(reader.buffer())?;
        let mut to_upstream = upstream.try_clone()?;
        let mut client = reader.into_inner();
        thread::spawn(move || {
            let _ = io::copy(&mut client, &mut to_upstream);
            let _ = to_upstream.shutdown(Shutdown::Write);
        });
        let _ = io::copy(&mut upstream, out);
        Ok(())
    }
}
