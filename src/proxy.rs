//! `foil __proxy`: the network allowlist for strict runs.
//!
//! A strict run has no network of its own. Its only way out is this proxy,
//! which runs outside the sandbox. On Linux it listens on a Unix socket that
//! the view shows at `/run/foil` (PID 1 relays `127.0.0.1:3128` inside the
//! run's empty network namespace to it). On macOS the run shares the host's
//! loopback, so the proxy listens on a random loopback port and every
//! request must carry a per-run token (as proxy credentials, or in the
//! injection path): other local programs can't use it. Three kinds of request:
//!
//! - `CONNECT host:port` (HTTPS through a proxy): tunnelled if the host is
//!   allowed, refused with 403 otherwise.
//! - `GET http://host/...` (plain HTTP through a proxy): the same check.
//! - `/__inject/<provider>/...`: forwarded to that provider's API over TLS,
//!   with the API key added on the way out. The key lives only in this
//!   process; the sandbox has a placeholder, so nothing inside it, not even
//!   a prompt-injected agent, can read the key and send it elsewhere.
//!
//! DNS is resolved here, outside, so the sandbox can't leak data through
//! DNS queries either. Blocked attempts are reported on stderr.

use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::net::TcpListener;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// An API whose key the proxy adds to requests.
struct Provider {
    name: &'static str,
    /// The real endpoint; `FOIL_INJECT_<NAME>_URL` overrides it (for a
    /// company gateway, or a test server).
    url: &'static str,
    /// The environment variable holding the key, in foil's environment.
    key_env: &'static str,
    /// The header to set, and how to format the key into it.
    header: &'static str,
    bearer: bool,
    /// Variables set inside the sandbox so the agent uses the proxy.
    base_env: &'static str,
    base_suffix: &'static str,
}

const PROVIDERS: [Provider; 2] = [
    Provider {
        name: "anthropic",
        url: "https://api.anthropic.com",
        key_env: "ANTHROPIC_API_KEY",
        header: "x-api-key",
        bearer: false,
        base_env: "ANTHROPIC_BASE_URL",
        base_suffix: "",
    },
    Provider {
        name: "openai",
        url: "https://api.openai.com",
        key_env: "OPENAI_API_KEY",
        header: "authorization",
        bearer: true,
        base_env: "OPENAI_BASE_URL",
        base_suffix: "/v1",
    },
];

/// The value the sandbox sees instead of a real key.
pub const PLACEHOLDER: &str = "foil-injects-the-real-key";

fn provider(name: &str) -> Option<&'static Provider> {
    PROVIDERS.iter().find(|p| p.name == name)
}

/// The environment variable that holds API `name`'s key.
pub fn key_env(name: &str) -> Option<&'static str> {
    provider(name).map(|p| p.key_env)
}

/// Whether foil's environment holds the key for API `name`.
pub fn has_key(name: &str) -> bool {
    provider(name).is_some_and(|p| std::env::var_os(p.key_env).is_some())
}

/// Check `--inject-key` names and that their keys are present, before a run.
pub fn check_inject(names: &[String]) -> io::Result<()> {
    for name in names {
        let p = provider(name).ok_or_else(|| {
            let known: Vec<&str> = PROVIDERS.iter().map(|p| p.name).collect();
            io::Error::new(io::ErrorKind::InvalidInput, format!("unknown --inject-key {name}; known: {}", known.join(", ")))
        })?;
        if std::env::var_os(p.key_env).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("--inject-key {name} needs {} in foil's environment", p.key_env),
            ));
        }
    }
    Ok(())
}

/// The environment a strict run gets so its HTTP clients use the proxy at
/// `127.0.0.1:port`, and the injected APIs through it. With a `token`, it
/// rides along as proxy credentials and in the injection path.
pub fn sandbox_env(port: u16, inject: &[String], token: Option<&str>) -> Vec<(String, String)> {
    let base = format!("http://127.0.0.1:{port}");
    let proxy = match token {
        Some(t) => format!("http://foil:{t}@127.0.0.1:{port}"),
        None => base.clone(),
    };
    let inject_root = match token {
        Some(t) => format!("{base}/__inject/{t}"),
        None => format!("{base}/__inject"),
    };
    let mut env = Vec::new();
    for name in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy", "ALL_PROXY", "all_proxy"] {
        env.push((name.to_owned(), proxy.clone()));
    }
    for name in ["NO_PROXY", "no_proxy"] {
        env.push((name.to_owned(), "localhost,127.0.0.1,::1".to_owned()));
    }
    for p in inject.iter().filter_map(|n| provider(n)) {
        env.push((p.base_env.to_owned(), format!("{inject_root}/{}{}", p.name, p.base_suffix)));
        env.push((p.key_env.to_owned(), PLACEHOLDER.to_owned()));
    }
    env
}

/// Does the allowlist let `host:port` through? Entries are `host`,
/// `host:port`, `*.domain` (any subdomain) or `*` (anything); without a
/// port only 80 and 443 are allowed.
pub fn allowed(allow: &[String], host: &str, port: u16) -> bool {
    // Only real host names and IP addresses: odd characters ('@', '/', '%',
    // spaces) could make a name match a pattern and then mean another host.
    if !valid_host(host) {
        return false;
    }
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    allow.iter().any(|entry| {
        // Port 0: none given.
        let (pattern, entry_port) = split_host_port(&entry.to_ascii_lowercase(), 0);
        let pattern = pattern.trim_end_matches('.');
        let port_ok = match entry_port {
            0 => port == 80 || port == 443,
            p => p == port,
        };
        let host_ok = pattern == "*"
            || pattern == host
            || pattern.strip_prefix("*.").is_some_and(|d| host.ends_with(&format!(".{d}")));
        port_ok && host_ok
    })
}

/// Whether `entry` is a usable `--allow-host` value: `*`, `*.domain`, a
/// host name or IP address, each optionally with `:port` (IPv6 as `[addr]:port`).
pub fn valid_allow_entry(entry: &str) -> bool {
    let (pattern, port) = split_host_port(entry, 0);
    let has_port = entry.parse::<IpAddr>().is_err() && entry.rsplit_once(':').is_some_and(|(h, _)| !h.contains(':') || h.ends_with(']'));
    if has_port && port == 0 {
        return false; // "host:99999", "host:", "host:http"
    }
    pattern == "*" || pattern.strip_prefix("*.").map_or_else(|| valid_host(&pattern), valid_host)
}

/// Whether `host` is named in the allowlist as it is: not only through
/// `*` or `*.domain`.
fn named_exactly(allow: &[String], host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    allow.iter().any(|entry| {
        let (pattern, _) = split_host_port(&entry.to_ascii_lowercase(), 0);
        pattern.trim_end_matches('.') == host
    })
}

/// Addresses of this machine or its local networks: loopback, private,
/// link-local (cloud metadata at 169.254.169.254), carrier-grade NAT,
/// unspecified, and the IPv6 equivalents.
fn is_internal(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || a == 0
                || (a == 100 && (64..128).contains(&b))
        }
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_internal(IpAddr::V4(v4)),
            None => {
                let first = v6.segments()[0];
                v6.is_loopback() || v6.is_unspecified() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
            }
        },
    }
}

/// Connect to an allowed `host:port`. The proxy runs on the host's network,
/// so a name that resolves to an internal address (by DNS rebinding, or a
/// disguise like `127.1` or `2130706433`) would reach this machine's own
/// services: that is only allowed when the allowlist names the host as it
/// is. The check and the connection use the same resolved address.
fn connect_allowed(allow: &[String], host: &str, port: u16) -> io::Result<TcpStream> {
    let addrs: Vec<SocketAddr> = (host, port).to_socket_addrs()?.collect();
    let exact = named_exactly(allow, host);
    let usable: Vec<SocketAddr> = addrs.iter().copied().filter(|a| exact || !is_internal(a.ip())).collect();
    if usable.is_empty() && !addrs.is_empty() {
        eprintln!("foil: blocked network access to {host}:{port} (resolves to an address of this machine or its network)");
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "internal address"));
    }
    TcpStream::connect(&usable[..])
}

/// A DNS name (letters, digits, '-' and '.', 1-253 bytes, no empty label)
/// or an IPv4 or IPv6 address.
pub fn valid_host(host: &str) -> bool {
    if host.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    let name = host.strip_suffix('.').unwrap_or(host);
    !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

struct Config {
    allow: Vec<String>,
    inject: Vec<String>,
    /// macOS: what every request must carry (see the module comment).
    token: Option<String>,
    tls: Arc<rustls::ClientConfig>,
}

/// A client connection: a Unix socket (Linux) or loopback TCP (macOS).
trait Conn: Read + Write + Send + Sized + 'static {
    fn try_clone_conn(&self) -> io::Result<Self>;
    fn set_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;
    fn shutdown_write(&self);
}

impl Conn for UnixStream {
    fn try_clone_conn(&self) -> io::Result<Self> {
        self.try_clone()
    }
    fn set_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.set_read_timeout(timeout)
    }
    fn shutdown_write(&self) {
        let _ = self.shutdown(Shutdown::Write);
    }
}

impl Conn for TcpStream {
    fn try_clone_conn(&self) -> io::Result<Self> {
        self.try_clone()
    }
    fn set_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.set_read_timeout(timeout)
    }
    fn shutdown_write(&self) {
        let _ = self.shutdown(Shutdown::Write);
    }
}

/// `foil __proxy SOCKET|tcp [--allow HOST]... [--inject NAME]... [--token T] [--ready-fd N]`
pub fn main(argv: &[OsString]) -> ! {
    let mut args = argv.iter().map(|a| a.to_string_lossy().into_owned());
    let Some(socket) = args.next() else {
        eprintln!("foil __proxy: no socket");
        std::process::exit(2)
    };
    let (mut allow, mut inject, mut ready, mut token) = (Vec::new(), Vec::new(), None, None);
    while let Some(flag) = args.next() {
        match (flag.as_str(), args.next()) {
            ("--allow", Some(v)) => allow.push(v),
            ("--inject", Some(v)) => inject.push(v),
            ("--token", Some(v)) => token = Some(v),
            ("--ready-fd", Some(v)) if v.parse::<i32>().is_ok() => ready = v.parse::<i32>().ok(),
            _ => {
                eprintln!("foil __proxy: bad arguments");
                std::process::exit(2)
            }
        }
    }
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    let config = Arc::new(Config { allow, inject, token, tls: Arc::new(tls) });
    exit_with_parent();
    // Tell foil where we listen ("ready" or the TCP port), once we do.
    let report = |what: String| {
        if let Some(fd) = ready {
            // SAFETY: foil passed this descriptor for exactly this; we own it now.
            let mut pipe = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fd) };
            let _ = writeln!(pipe, "{what}");
        }
    };
    if socket == "tcp" {
        let listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(l) => l,
            Err(err) => {
                eprintln!("foil: network proxy: {err}");
                std::process::exit(1)
            }
        };
        report(listener.local_addr().map(|a| a.port().to_string()).unwrap_or_default());
        serve(listener.incoming(), &config)
    }
    let listener = match UnixListener::bind(&socket) {
        Ok(l) => l,
        Err(err) => {
            eprintln!("foil: network proxy: {socket}: {err}");
            std::process::exit(1)
        }
    };
    report("ready".to_owned());
    serve(listener.incoming(), &config)
}

/// The proxy must not outlive the foil that started it. On Linux the
/// parent-death signal sees to that; macOS has none, so a thread watches
/// for the parent to go (the proxy is then adopted by launchd).
fn exit_with_parent() {
    // SAFETY: getppid has no preconditions.
    let parent = unsafe { libc::getppid() };
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(250));
        // SAFETY: as above.
        if unsafe { libc::getppid() } != parent {
            std::process::exit(0);
        }
    });
}

/// Accept connections until the listener fails for good.
fn serve<C: Conn>(incoming: impl Iterator<Item = io::Result<C>>, config: &Arc<Config>) -> ! {
    // Connections being served; past the cap new ones are closed at once,
    // so a flood from the sandbox can't exhaust the proxy's threads or files.
    static OPEN: AtomicUsize = AtomicUsize::new(0);
    const MAX_OPEN: usize = 256;
    for conn in incoming {
        let conn = match conn {
            Ok(conn) => conn,
            // Out of descriptors (or similar): back off instead of spinning.
            Err(_) => {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
        };
        if OPEN.fetch_add(1, Ordering::SeqCst) >= MAX_OPEN {
            OPEN.fetch_sub(1, Ordering::SeqCst);
            continue;
        }
        let config = config.clone();
        std::thread::spawn(move || {
            let _ = handle(conn, &config);
            OPEN.fetch_sub(1, Ordering::SeqCst);
        });
    }
    std::process::exit(0)
}

/// A running proxy, as `start` left it.
pub struct Running {
    pub child: Child,
    /// Linux: the private folder holding the socket.
    pub dir: Option<PathBuf>,
    /// macOS: the loopback port and the token requests must carry.
    pub port: Option<u16>,
    pub token: Option<String>,
}

impl Running {
    /// Stop the proxy, if it still runs, and remove its folder.
    pub fn stop(mut self) {
        // The proxy is foil's child, so the supervisor's cleanup may have
        // stopped and reaped it already: then its PID may belong to someone
        // else by now, and must not be signalled. Only if it still runs is
        // the PID still its own.
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        if let Some(dir) = self.dir.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// The descriptor on which `foil __proxy --ready-fd` reports that it listens.
pub const READY_FD: i32 = 3;

/// Start `foil __proxy`: on Linux on a socket in a fresh private folder, on
/// macOS on a loopback port with a fresh token. Returns once it accepts
/// connections.
pub fn start(allow: &[String], inject: &[String]) -> io::Result<Running> {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("__proxy").stdin(Stdio::null());
    let (dir, token) = if cfg!(target_os = "linux") {
        use std::os::unix::fs::DirBuilderExt;
        sweep_proxy_dirs();
        let dir = std::env::temp_dir().join(format!("foil-proxy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
        cmd.arg(dir.join("proxy.sock"));
        (Some(dir), None)
    } else {
        let token = random_token()?;
        cmd.arg("tcp").args(["--token", &token]);
        (None, Some(token))
    };
    for host in allow {
        cmd.args(["--allow", host]);
    }
    for name in inject {
        cmd.args(["--inject", name]);
    }
    // The proxy writes one line to this pipe once it listens (or exits,
    // which closes it): waiting on it costs no polling delay.
    let (read_end, write_end) = cloexec_pipe()?;
    let write_fd = std::os::fd::AsRawFd::as_raw_fd(&write_end);
    cmd.arg("--ready-fd").arg(READY_FD.to_string());
    // SAFETY: prctl and dup2 are async-signal-safe.
    unsafe {
        cmd.pre_exec(move || {
            // Linux: the proxy must not outlive foil, even a SIGKILLed one.
            #[cfg(target_os = "linux")]
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong);
            // dup2 clears close-on-exec on the copy.
            if libc::dup2(write_fd, READY_FD) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    drop(write_end);
    let mut fds = [libc::pollfd { fd: std::os::fd::AsRawFd::as_raw_fd(&read_end), events: libc::POLLIN, revents: 0 }];
    // SAFETY: one valid pollfd.
    let woke = unsafe { libc::poll(fds.as_mut_ptr(), 1, 5000) } == 1;
    let mut line = String::new();
    let _ = woke && BufReader::new(&read_end).read_line(&mut line).is_ok();
    let line = line.trim();
    let port = line.parse::<u16>().ok();
    if line.is_empty() || (token.is_some() && port.is_none()) {
        let _ = child.kill();
        let _ = child.wait();
        if let Some(dir) = &dir {
            let _ = std::fs::remove_dir_all(dir);
        }
        return Err(io::Error::other("the network proxy did not start"));
    }
    Ok(Running { child, dir, port, token })
}

/// A pipe whose two ends close on exec.
fn cloexec_pipe() -> io::Result<(std::fs::File, std::os::fd::OwnedFd)> {
    use std::os::fd::FromRawFd;
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: pipe fills in two new descriptors.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    for fd in fds {
        // SAFETY: descriptors we just made.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    // SAFETY: pipe just returned these and nothing else owns them.
    Ok(unsafe { (std::fs::File::from_raw_fd(fds[0]), std::os::fd::OwnedFd::from_raw_fd(fds[1])) })
}

/// 32 random hex digits from the system's random source.
fn random_token() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Remove the proxy folders of foils that are gone (killed before they
/// could clean up): ours only, and only if no process has that PID.
fn sweep_proxy_dirs() {
    use std::os::unix::fs::MetadataExt;
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else { return };
    // SAFETY: getuid has no preconditions.
    let me = unsafe { libc::getuid() };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|n| n.strip_prefix("foil-proxy-")) else { continue };
        let alive = pid.parse::<u32>().is_ok_and(|p| Path::new(&format!("/proc/{p}")).exists());
        if !alive && entry.metadata().is_ok_and(|m| m.uid() == me && m.is_dir()) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Standard base64 (with padding), for Basic proxy credentials.
fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ABC[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A parsed request head: the request line's parts and the header lines.
struct Head {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
}

/// Read one line of at most `limit` bytes (the excess is an error, not memory).
fn read_line_limited<C: Conn>(reader: &mut BufReader<C>, out: &mut String, limit: usize) -> io::Result<usize> {
    let mut bytes = Vec::new();
    let n = reader.by_ref().take(limit as u64 + 1).read_until(b'\n', &mut bytes)?;
    if n > limit {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "request head too large"));
    }
    // Control characters other than the line end could split a header in two upstream.
    let line = String::from_utf8(bytes).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "request head is not text"))?;
    if line.trim_end_matches(['\r', '\n']).bytes().any(|b| b < 0x20 && b != b'\t' || b == 0x7f) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "control character in request head"));
    }
    out.push_str(&line);
    Ok(n)
}

fn read_head<C: Conn>(reader: &mut BufReader<C>) -> io::Result<Head> {
    let mut line = String::new();
    read_line_limited(reader, &mut line, 16 * 1024)?;
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad request line"));
    };
    let (method, target) = (method.to_owned(), target.to_owned());
    let mut headers = Vec::new();
    let mut total = line.len();
    loop {
        let mut h = String::new();
        if read_line_limited(reader, &mut h, 16 * 1024)? == 0 {
            break;
        }
        total += h.len();
        if total > 64 * 1024 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "request head too large"));
        }
        let h = h.trim_end_matches(['\r', '\n']);
        if h.is_empty() {
            break;
        }
        if let Some((name, value)) = h.split_once(':') {
            headers.push((name.trim().to_owned(), value.trim().to_owned()));
        }
    }
    Ok(Head { method, target, headers })
}

fn handle<C: Conn>(conn: C, config: &Config) -> io::Result<()> {
    // A client that opens a connection and sends nothing doesn't hold a thread forever.
    conn.set_timeout(Some(Duration::from_secs(120)))?;
    let mut reader = BufReader::new(conn.try_clone_conn()?);
    let head = read_head(&mut reader)?;
    let mut client = conn;
    if let Some(token) = &config.token {
        // The injection path carries the token; proxy requests carry it as credentials.
        let inject_ok = head.target.strip_prefix("/__inject/").and_then(|r| r.split_once('/')).is_some_and(|(t, _)| t == token);
        let expected = format!("Basic {}", base64(format!("foil:{token}").as_bytes()));
        let creds_ok = head.headers.iter().any(|(n, v)| n.eq_ignore_ascii_case("proxy-authorization") && *v == expected);
        if !inject_ok && !creds_ok {
            return respond(&mut client, 407, "foil proxy: missing or wrong token");
        }
    }
    if head.method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = split_host_port(&head.target, 443);
        if !allowed(&config.allow, &host, port) {
            return deny(&mut client, &host, port);
        }
        let upstream = match connect_allowed(&config.allow, &host, port) {
            Ok(upstream) => upstream,
            Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
                return respond(&mut client, 403, "blocked by foil: internal address");
            }
            Err(err) => return respond(&mut client, 502, &format!("foil proxy: {host}:{port}: {err}")),
        };
        client.set_timeout(None)?;
        client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")?;
        // Anything the client sent after the head (rare) goes first.
        let early = reader.buffer().to_vec();
        return tunnel(client, early, upstream);
    }
    if let Some(mut rest) = head.target.strip_prefix("/__inject/") {
        if let Some(token) = &config.token {
            rest = rest.strip_prefix(token.as_str()).and_then(|r| r.strip_prefix('/')).unwrap_or(rest);
        }
        let (name, path) = rest.split_once('/').map_or((rest, "/".to_owned()), |(n, p)| (n, format!("/{p}")));
        return match provider(name).filter(|p| config.inject.iter().any(|n| n == p.name)) {
            Some(p) => inject(&mut reader, &mut client, &head, p, &path, config),
            None => respond(&mut client, 404, "no such injected API"),
        };
    }
    if let Some(rest) = head.target.strip_prefix("http://") {
        let (authority, path) = rest.split_once('/').map_or((rest, "/".to_owned()), |(a, p)| (a, format!("/{p}")));
        let (host, port) = split_host_port(authority, 80);
        if !allowed(&config.allow, &host, port) {
            return deny(&mut client, &host, port);
        }
        let mut upstream = match connect_allowed(&config.allow, &host, port) {
            Ok(upstream) => upstream,
            Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
                return respond(&mut client, 403, "blocked by foil: internal address");
            }
            Err(err) => return respond(&mut client, 502, &format!("foil proxy: {host}:{port}: {err}")),
        };
        upstream.write_all(&rebuild(&head, &path, authority, &[], None))?;
        copy_body(&mut reader, &head.headers, &mut upstream)?;
        client.set_timeout(None)?;
        io::copy(&mut upstream, &mut client)?;
        return Ok(());
    }
    respond(&mut client, 400, "foil proxy: use it as an HTTP proxy")
}

/// Forward one request to `p`'s API with the key added, then stream the
/// response back. One request per connection (`Connection: close`).
fn inject<C: Conn>(
    reader: &mut BufReader<C>,
    client: &mut C,
    head: &Head,
    p: &Provider,
    path: &str,
    config: &Config,
) -> io::Result<()> {
    let key = std::env::var(p.key_env).unwrap_or_default();
    let base = std::env::var(format!("FOIL_INJECT_{}_URL", p.name.to_ascii_uppercase())).unwrap_or(p.url.to_owned());
    let (tls, rest) = match (base.strip_prefix("https://"), base.strip_prefix("http://")) {
        (Some(r), _) => (true, r),
        (None, Some(r)) => (false, r),
        _ => return respond(client, 502, "bad upstream URL"),
    };
    let (authority, prefix) = rest.split_once('/').map_or((rest, String::new()), |(a, p)| (a, format!("/{p}")));
    let (host, port) = split_host_port(authority, if tls { 443 } else { 80 });
    let value = if p.bearer { format!("Bearer {key}") } else { key };
    let request = rebuild(head, &format!("{prefix}{path}"), authority, &["x-api-key", "authorization"], Some((p.header, &value)));
    let tcp = TcpStream::connect((host.as_str(), port))?;
    tcp.set_read_timeout(Some(Duration::from_secs(600)))?;
    if tls {
        let name = rustls::pki_types::ServerName::try_from(host.clone())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad upstream host"))?;
        let conn = rustls::ClientConnection::new(config.tls.clone(), name).map_err(io::Error::other)?;
        let mut stream = rustls::StreamOwned::new(conn, tcp);
        stream.write_all(&request)?;
        copy_body(reader, &head.headers, &mut stream)?;
        stream.flush()?;
        client.set_timeout(None)?;
        copy_until_close(&mut stream, client)
    } else {
        let mut stream = tcp;
        stream.write_all(&request)?;
        copy_body(reader, &head.headers, &mut stream)?;
        client.set_timeout(None)?;
        copy_until_close(&mut stream, client)
    }
}

/// Copy a response to the client as it arrives (streamed answers included).
/// A TLS peer that closes without a close_notify is treated as the end.
fn copy_until_close(from: &mut impl Read, to: &mut impl Write) -> io::Result<()> {
    let mut buf = [0u8; 16 * 1024];
    loop {
        match from.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => to.write_all(&buf[..n])?,
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
}

/// The request head again, for `path` on `authority`, without `strip`
/// headers or hop-by-hop ones, plus `add`, and `Connection: close`.
fn rebuild(head: &Head, path: &str, authority: &str, strip: &[&str], add: Option<(&str, &str)>) -> Vec<u8> {
    let mut out = format!("{} {} HTTP/1.1\r\nHost: {authority}\r\n", head.method, path);
    for (name, value) in &head.headers {
        let lower = name.to_ascii_lowercase();
        let hop = ["host", "connection", "proxy-connection", "proxy-authorization", "keep-alive"].contains(&lower.as_str());
        if !hop && !strip.contains(&lower.as_str()) {
            out.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    if let Some((name, value)) = add {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str("Connection: close\r\n\r\n");
    out.into_bytes()
}

/// Copy the request body, by Content-Length or chunked encoding (kept as
/// is), from the client to `to` as it arrives: a body is never held in
/// memory, so a huge one costs time, not the proxy.
fn copy_body<C: Conn>(reader: &mut BufReader<C>, headers: &[(String, String)], to: &mut impl Write) -> io::Result<()> {
    let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_owned());
    let get = |name: &str| headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str());
    let copy_exact = |reader: &mut BufReader<C>, to: &mut dyn Write, len: u64| -> io::Result<()> {
        if io::copy(&mut reader.by_ref().take(len), to)? != len {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "request body ended early"));
        }
        Ok(())
    };
    if get("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
        loop {
            let mut size_line = String::new();
            if read_line_limited(reader, &mut size_line, 1024)? == 0 {
                return Err(bad("request body ended early"));
            }
            to.write_all(size_line.as_bytes())?;
            let size = u64::from_str_radix(size_line.trim().split(';').next().unwrap_or("").trim(), 16)
                .map_err(|_| bad("bad chunk size"))?;
            if size == 0 {
                // Trailer lines, up to the empty line that ends the body.
                loop {
                    let mut line = String::new();
                    if read_line_limited(reader, &mut line, 16 * 1024)? == 0 {
                        return Err(bad("request body ended early"));
                    }
                    to.write_all(line.as_bytes())?;
                    if line.trim_end_matches(['\r', '\n']).is_empty() {
                        return Ok(());
                    }
                }
            }
            // The chunk and the line end after it.
            copy_exact(reader, to, size.checked_add(2).ok_or_else(|| bad("bad chunk size"))?)?;
        }
    }
    let len = match get("content-length") {
        Some(v) => v.trim().parse::<u64>().map_err(|_| bad("bad Content-Length"))?,
        None => 0,
    };
    copy_exact(reader, to, len)
}

fn tunnel<C: Conn>(client: C, early: Vec<u8>, upstream: TcpStream) -> io::Result<()> {
    let (mut c_in, mut u_out) = (client.try_clone_conn()?, upstream.try_clone()?);
    u_out.write_all(&early)?;
    let up = std::thread::spawn(move || {
        let _ = io::copy(&mut c_in, &mut u_out);
        let _ = u_out.shutdown(Shutdown::Write);
    });
    let (mut u_in, mut c_out) = (upstream, client);
    let _ = io::copy(&mut u_in, &mut c_out);
    c_out.shutdown_write();
    let _ = up.join();
    Ok(())
}

fn split_host_port(authority: &str, default: u16) -> (String, u16) {
    // [v6]:port, host:port, or host (a bare IPv6 address included).
    if authority.parse::<IpAddr>().is_ok() {
        return (authority.to_owned(), default);
    }
    if let Some(rest) = authority.strip_prefix('[') {
        if let Some((h, p)) = rest.split_once(']') {
            return (h.to_owned(), p.strip_prefix(':').and_then(|p| p.parse().ok()).unwrap_or(default));
        }
    }
    match authority.rsplit_once(':') {
        Some((h, p)) if p.parse::<u16>().is_ok() => (h.to_owned(), p.parse().unwrap_or(default)),
        _ => (authority.to_owned(), default),
    }
}

fn deny(client: &mut impl Write, host: &str, port: u16) -> io::Result<()> {
    eprintln!("foil: blocked network access to {host}:{port} (not in --allow-host)");
    respond(client, 403, "blocked by foil: host not in --allow-host")
}

fn respond(client: &mut impl Write, code: u16, text: &str) -> io::Result<()> {
    let reason = match code {
        403 => "Forbidden",
        407 => "Proxy Authentication Required",
        404 => "Not Found",
        502 => "Bad Gateway",
        _ => "Bad Request",
    };
    write!(client, "HTTP/1.1 {code} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_matching() {
        let allow: Vec<String> = ["api.example.com", "*.npmjs.org", "127.0.0.1:8080"].map(String::from).to_vec();
        assert!(allowed(&allow, "api.example.com", 443));
        assert!(allowed(&allow, "API.Example.com.", 80));
        assert!(!allowed(&allow, "api.example.com", 22));
        assert!(allowed(&allow, "registry.npmjs.org", 443));
        assert!(!allowed(&allow, "npmjs.org.evil.com", 443));
        assert!(!allowed(&allow, "evilnpmjs.org", 443));
        assert!(allowed(&allow, "127.0.0.1", 8080));
        assert!(!allowed(&allow, "127.0.0.1", 443));
        assert!(!allowed(&[], "anything", 443));
        assert!(allowed(&["*".to_owned()], "anything", 443));
        assert!(allowed(&["[::1]:8080".to_owned()], "::1", 8080));
        assert!(allowed(&["::1".to_owned()], "::1", 443));
        assert!(allowed(&["example.com.".to_owned()], "example.com", 443));
    }

    #[test]
    fn allow_entries_are_checked() {
        for ok in ["example.com", "example.com:8443", "*.example.com", "*", "10.0.0.1:80", "::1", "[::1]:80"] {
            assert!(valid_allow_entry(ok), "{ok}");
        }
        for bad in ["example.com:99999", "example.com:", "example.com:http", "ex ample.com", "*.", "http://example.com", ""] {
            assert!(!valid_allow_entry(bad), "{bad}");
        }
    }

    #[test]
    fn odd_host_names_never_match() {
        let allow: Vec<String> = ["*.allowed.test", "*"].map(String::from).to_vec();
        for host in [
            "evil.com/.allowed.test",
            "evil.com@x.allowed.test",
            "evil.com%2f.allowed.test",
            "evil.com .allowed.test",
            "evil.com\0.allowed.test",
            "x..allowed.test",
            "-x.allowed.test",
            "",
            ".",
        ] {
            assert!(!allowed(&allow, host, 443), "{host:?} matched");
        }
        assert!(valid_host("a-b.allowed.test") && valid_host("10.0.0.1") && valid_host("::1") && valid_host("x.test."));
        assert!(!valid_host(&"a".repeat(64)) && !valid_host(&format!("{}.test", "a.".repeat(130))));
    }

    #[test]
    fn internal_addresses() {
        for ip in ["127.0.0.1", "10.1.2.3", "172.16.0.1", "192.168.1.1", "169.254.169.254", "100.64.0.1", "0.0.0.0", "::1", "fd00::1", "fe80::1", "::ffff:127.0.0.1"] {
            assert!(is_internal(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "160.79.104.10", "2606:4700::1111", "100.128.0.1"] {
            assert!(!is_internal(ip.parse().unwrap()), "{ip}");
        }
        let allow: Vec<String> = ["127.0.0.1:8080", "*.example.com", "[::1]:80"].map(String::from).to_vec();
        assert!(named_exactly(&allow, "127.0.0.1") && named_exactly(&allow, "::1"));
        assert!(!named_exactly(&allow, "x.example.com") && !named_exactly(&["*".to_owned()], "127.0.0.1"));
    }

    #[test]
    fn bodies_are_copied_exactly() {
        let body = |headers: &[(&str, &str)], input: &[u8]| -> io::Result<Vec<u8>> {
            let (mut a, b) = UnixStream::pair()?;
            a.write_all(input)?;
            drop(a);
            let headers: Vec<(String, String)> = headers.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect();
            let mut out = Vec::new();
            copy_body(&mut BufReader::new(b), &headers, &mut out)?;
            Ok(out)
        };
        assert_eq!(body(&[("Content-Length", "5")], b"helloEXTRA").unwrap(), b"hello");
        assert!(body(&[("Content-Length", "50")], b"short").is_err());
        assert!(body(&[("Content-Length", "-1")], b"").is_err());
        let chunked = b"5\r\nhello\r\n0\r\nX-Trailer: 1\r\n\r\nNEXT";
        assert_eq!(body(&[("Transfer-Encoding", "chunked")], chunked).unwrap(), &chunked[..chunked.len() - 4]);
        assert!(body(&[("Transfer-Encoding", "chunked")], b"ffffffffffffffff\r\n").is_err());
        assert!(body(&[("Transfer-Encoding", "chunked")], &[b'1'; 5000]).is_err());
    }

    #[test]
    fn base64_encodes() {
        assert_eq!(base64(b"foil:abc"), "Zm9pbDphYmM=");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"abc"), "YWJj");
        assert_eq!(base64(b""), "");
    }

    #[test]
    fn host_and_port() {
        assert_eq!(split_host_port("example.com:8443", 443), ("example.com".to_owned(), 8443));
        assert_eq!(split_host_port("example.com", 443), ("example.com".to_owned(), 443));
        assert_eq!(split_host_port("[::1]:80", 443), ("::1".to_owned(), 80));
    }
}
