//! `foil __proxy`: the network allowlist for strict runs.
//!
//! A strict run has an empty network namespace: no route anywhere. Its only
//! way out is this proxy, which runs outside the sandbox and listens on a
//! Unix socket that the view shows at `/run/foil` (PID 1 relays
//! `127.0.0.1:3128` inside the run to it). Three kinds of request:
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
use std::net::{Shutdown, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
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
/// `127.0.0.1:port`, and the injected APIs through it.
pub fn sandbox_env(port: u16, inject: &[String]) -> Vec<(String, String)> {
    let proxy = format!("http://127.0.0.1:{port}");
    let mut env = Vec::new();
    for name in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy", "ALL_PROXY", "all_proxy"] {
        env.push((name.to_owned(), proxy.clone()));
    }
    for name in ["NO_PROXY", "no_proxy"] {
        env.push((name.to_owned(), "localhost,127.0.0.1,::1".to_owned()));
    }
    for p in inject.iter().filter_map(|n| provider(n)) {
        env.push((p.base_env.to_owned(), format!("{proxy}/__inject/{}{}", p.name, p.base_suffix)));
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
        let entry = entry.to_ascii_lowercase();
        let (pattern, ports) = match entry.rsplit_once(':') {
            Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => (h.to_owned(), p.parse::<u16>().ok()),
            _ => (entry.clone(), None),
        };
        let port_ok = match ports {
            Some(p) => p == port,
            None => port == 80 || port == 443,
        };
        let host_ok = pattern == "*"
            || pattern == host
            || pattern.strip_prefix("*.").is_some_and(|d| host.ends_with(&format!(".{d}")));
        port_ok && host_ok
    })
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
    tls: Arc<rustls::ClientConfig>,
}

/// `foil __proxy SOCKET [--allow HOST]... [--inject NAME]...`
pub fn main(argv: &[OsString]) -> ! {
    let mut args = argv.iter().map(|a| a.to_string_lossy().into_owned());
    let Some(socket) = args.next() else {
        eprintln!("foil __proxy: no socket");
        std::process::exit(2)
    };
    let (mut allow, mut inject) = (Vec::new(), Vec::new());
    while let Some(flag) = args.next() {
        match (flag.as_str(), args.next()) {
            ("--allow", Some(v)) => allow.push(v),
            ("--inject", Some(v)) => inject.push(v),
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
    let config = Arc::new(Config { allow, inject, tls: Arc::new(tls) });
    let listener = match UnixListener::bind(&socket) {
        Ok(l) => l,
        Err(err) => {
            eprintln!("foil: network proxy: {socket}: {err}");
            std::process::exit(1)
        }
    };
    for conn in listener.incoming().flatten() {
        let config = config.clone();
        std::thread::spawn(move || {
            let _ = handle(conn, &config);
        });
    }
    std::process::exit(0)
}

/// A parsed request head: the request line's parts and the header lines.
struct Head {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
}

/// Read one line of at most `limit` bytes (the excess is an error, not memory).
fn read_line_limited(reader: &mut BufReader<UnixStream>, out: &mut String, limit: usize) -> io::Result<usize> {
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

fn read_head(reader: &mut BufReader<UnixStream>) -> io::Result<Head> {
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

fn handle(conn: UnixStream, config: &Config) -> io::Result<()> {
    let mut reader = BufReader::new(conn.try_clone()?);
    let head = read_head(&mut reader)?;
    let mut client = conn;
    if head.method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = split_host_port(&head.target, 443);
        if !allowed(&config.allow, &host, port) {
            return deny(&mut client, &host, port);
        }
        let upstream = TcpStream::connect((host.as_str(), port))?;
        client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")?;
        // Anything the client sent after the head (rare) goes first.
        let early = reader.buffer().to_vec();
        return tunnel(client, early, upstream);
    }
    if let Some(rest) = head.target.strip_prefix("/__inject/") {
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
        let mut upstream = TcpStream::connect((host.as_str(), port))?;
        let body = read_body(&mut reader, &head.headers)?;
        upstream.write_all(&rebuild(&head, &path, authority, &[], None))?;
        upstream.write_all(&body)?;
        io::copy(&mut upstream, &mut client)?;
        return Ok(());
    }
    respond(&mut client, 400, "foil proxy: use it as an HTTP proxy")
}

/// Forward one request to `p`'s API with the key added, then stream the
/// response back. One request per connection (`Connection: close`).
fn inject(
    reader: &mut BufReader<UnixStream>,
    client: &mut UnixStream,
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
    let body = read_body(reader, &head.headers)?;
    let request = rebuild(head, &format!("{prefix}{path}"), authority, &["x-api-key", "authorization"], Some((p.header, &value)));
    let tcp = TcpStream::connect((host.as_str(), port))?;
    tcp.set_read_timeout(Some(Duration::from_secs(600)))?;
    if tls {
        let name = rustls::pki_types::ServerName::try_from(host.clone())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad upstream host"))?;
        let conn = rustls::ClientConnection::new(config.tls.clone(), name).map_err(io::Error::other)?;
        let mut stream = rustls::StreamOwned::new(conn, tcp);
        stream.write_all(&request)?;
        stream.write_all(&body)?;
        stream.flush()?;
        copy_until_close(&mut stream, client)
    } else {
        let mut stream = tcp;
        stream.write_all(&request)?;
        stream.write_all(&body)?;
        copy_until_close(&mut stream, client)
    }
}

/// Copy a response to the client as it arrives (streamed answers included).
/// A TLS peer that closes without a close_notify is treated as the end.
fn copy_until_close(from: &mut impl Read, to: &mut UnixStream) -> io::Result<()> {
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

/// The request body, by Content-Length or chunked encoding (kept as is).
fn read_body(reader: &mut BufReader<UnixStream>, headers: &[(String, String)]) -> io::Result<Vec<u8>> {
    let get = |name: &str| headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str());
    if get("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
        let mut body = Vec::new();
        loop {
            let mut size_line = String::new();
            reader.read_line(&mut size_line)?;
            body.extend_from_slice(size_line.as_bytes());
            let size = usize::from_str_radix(size_line.trim().split(';').next().unwrap_or("0"), 16)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
            let mut chunk = vec![0u8; size + 2];
            reader.read_exact(&mut chunk)?;
            body.extend_from_slice(&chunk);
            if size == 0 {
                return Ok(body);
            }
        }
    }
    let len: usize = get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    Ok(body)
}

fn tunnel(client: UnixStream, early: Vec<u8>, upstream: TcpStream) -> io::Result<()> {
    let (mut c_in, mut u_out) = (client.try_clone()?, upstream.try_clone()?);
    u_out.write_all(&early)?;
    let up = std::thread::spawn(move || {
        let _ = io::copy(&mut c_in, &mut u_out);
        let _ = u_out.shutdown(Shutdown::Write);
    });
    let (mut u_in, mut c_out) = (upstream, client);
    let _ = io::copy(&mut u_in, &mut c_out);
    let _ = c_out.shutdown(Shutdown::Write);
    let _ = up.join();
    Ok(())
}

fn split_host_port(authority: &str, default: u16) -> (String, u16) {
    // [v6]:port, host:port, or host.
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

fn deny(client: &mut UnixStream, host: &str, port: u16) -> io::Result<()> {
    eprintln!("foil: blocked network access to {host}:{port} (not in --allow-host)");
    respond(client, 403, "blocked by foil: host not in --allow-host")
}

fn respond(client: &mut UnixStream, code: u16, text: &str) -> io::Result<()> {
    let reason = match code {
        403 => "Forbidden",
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
    fn host_and_port() {
        assert_eq!(split_host_port("example.com:8443", 443), ("example.com".to_owned(), 8443));
        assert_eq!(split_host_port("example.com", 443), ("example.com".to_owned(), 443));
        assert_eq!(split_host_port("[::1]:80", 443), ("::1".to_owned(), 80));
    }
}
