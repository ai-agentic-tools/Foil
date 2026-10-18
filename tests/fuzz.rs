//! Fuzzing on stable Rust (family E of the quality plan), black-box:
//!
//! - the network proxy gets thousands of mutated and malformed requests on
//!   its socket; it must stay alive, never contact a host off its allowlist,
//!   and never send the injected key back to the client;
//! - the command line gets random arguments; foil must never panic.
//!
//! FOIL_FUZZ_ITERS scales the number of cases (default 3000); FOIL_FUZZ_SEED
//! picks the first seed. A failure prints the input that caused it.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn iters() -> u64 {
    std::env::var("FOIL_FUZZ_ITERS").ok().and_then(|v| v.parse().ok()).unwrap_or(3000)
}

fn seed() -> u64 {
    std::env::var("FOIL_FUZZ_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(7)
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("foil-fuzz-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A loopback HTTP server that answers every request with `body` and counts them.
fn server(count: &'static AtomicUsize, body: &'static str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            count.fetch_add(1, Ordering::SeqCst);
            std::thread::spawn(move || {
                let _ = conn.set_read_timeout(Some(Duration::from_millis(500)));
                let mut reader = BufReader::new(conn.try_clone().unwrap());
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 2 {
                    line.clear();
                }
                let mut conn = conn;
                let _ = write!(conn, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            });
        }
    });
    port
}

/// Valid requests the mutations start from.
fn corpus(allowed: u16, forbidden: u16) -> Vec<Vec<u8>> {
    [
        format!("GET http://127.0.0.1:{allowed}/a HTTP/1.1\r\nHost: x\r\n\r\n"),
        format!("POST http://127.0.0.1:{allowed}/p HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello"),
        format!("POST http://127.0.0.1:{allowed}/c HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n"),
        format!("CONNECT 127.0.0.1:{allowed} HTTP/1.1\r\n\r\n"),
        format!("CONNECT 127.0.0.1:{forbidden} HTTP/1.1\r\n\r\n"),
        format!("GET http://127.0.0.1:{forbidden}/ HTTP/1.1\r\n\r\n"),
        "POST /__inject/anthropic/v1/messages HTTP/1.1\r\nContent-Length: 2\r\nx-api-key: placeholder\r\n\r\n{}".to_owned(),
        "GET /__inject/openai/v1/models HTTP/1.1\r\n\r\n".to_owned(),
        format!("POST http://127.0.0.1:{allowed}/ HTTP/1.1\r\nContent-Length: 3\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\nGET http://127.0.0.1:{forbidden}/ HTTP/1.1\r\n\r\n"),
    ]
    .into_iter()
    .map(String::into_bytes)
    .collect()
}

const TOKENS: [&[u8]; 14] = [
    b"\r\n", b"\r", b"\n", b"\0", b":", b"@", b"/", b"%2f", b"[::1]", b"99999999999999999999", b"-1",
    b"chunked", b"Content-Length: 10\r\n", b"\xff\xfe",
];

fn mutate(rng: &mut Rng, input: &[u8]) -> Vec<u8> {
    let mut out = input.to_vec();
    for _ in 0..(1 + rng.below(6)) {
        let at = rng.below(out.len() + 1);
        match rng.below(6) {
            0 if !out.is_empty() => {
                let i = rng.below(out.len());
                out[i] = rng.next() as u8;
            }
            1 => {
                let t = TOKENS[rng.below(TOKENS.len())];
                out.splice(at..at, t.iter().copied());
            }
            2 => out.truncate(at),
            3 => {
                let n = rng.below(4) * 4000;
                out.splice(at..at, std::iter::repeat_n(b'A', n));
            }
            4 if at < out.len() => {
                let end = (at + rng.below(20)).min(out.len());
                out.drain(at..end);
            }
            _ => {
                let copy = out.clone();
                out.extend_from_slice(&copy[..at.min(copy.len())]);
            }
        }
    }
    out
}

#[test]
fn the_proxy_survives_malformed_requests_and_never_leaks() {
    static ALLOWED: AtomicUsize = AtomicUsize::new(0);
    static FORBIDDEN: AtomicUsize = AtomicUsize::new(0);
    static API: AtomicUsize = AtomicUsize::new(0);
    let allowed = server(&ALLOWED, "allowed");
    let forbidden = server(&FORBIDDEN, "forbidden");
    let api = server(&API, "api");
    let dir = scratch("proxy");
    let socket = dir.join("proxy.sock");
    let mut proxy = Command::new(env!("CARGO_BIN_EXE_foil"))
        .arg("__proxy")
        .arg(&socket)
        .args(["--allow", &format!("127.0.0.1:{allowed}"), "--inject", "anthropic"])
        .env("ANTHROPIC_API_KEY", "sk-fuzz-secret")
        .env("FOIL_INJECT_ANTHROPIC_URL", format!("http://127.0.0.1:{api}"))
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..100 {
        if UnixStream::connect(&socket).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let seeds = corpus(allowed, forbidden);
    let mut rng = Rng(seed());
    for case in 0..iters() {
        let pick = rng.below(seeds.len());
        let input = mutate(&mut rng, &seeds[pick]);
        let reply = match UnixStream::connect(&socket) {
            Ok(mut conn) => {
                conn.set_read_timeout(Some(Duration::from_millis(1500))).unwrap();
                let _ = conn.write_all(&input);
                let _ = conn.shutdown(std::net::Shutdown::Write);
                let mut reply = Vec::new();
                let _ = conn.read_to_end(&mut reply);
                reply
            }
            Err(err) => panic!("case {case}: the proxy stopped accepting ({err}) after {:?}", String::from_utf8_lossy(&input)),
        };
        assert!(proxy.try_wait().unwrap().is_none(), "case {case}: the proxy died on {:?}", String::from_utf8_lossy(&input));
        let reply = String::from_utf8_lossy(&reply);
        assert!(!reply.contains("sk-fuzz-secret"), "case {case}: key sent back for {:?}", String::from_utf8_lossy(&input));
        assert!(!reply.contains("forbidden"), "case {case}: forbidden host answered {:?}", String::from_utf8_lossy(&input));
    }
    std::thread::sleep(Duration::from_millis(300));
    let _ = proxy.kill();
    let _ = proxy.wait();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(FORBIDDEN.load(Ordering::SeqCst), 0, "a host off the allowlist was contacted");
    assert!(ALLOWED.load(Ordering::SeqCst) > 0 && API.load(Ordering::SeqCst) > 0, "the fuzzing never got through: weak corpus");
}

#[test]
fn the_command_line_never_panics() {
    const WORDS: [&str; 34] = [
        "run", "agent", "fanout", "pick", "diff", "commit", "abort", "list", "doctor", "--", "-b", "-w", "-n",
        "--branch", "--strict", "--protect", "--no-network", "--memory", "--cpus", "--pids", "--allow-read",
        "--allow-write", "--allow-host", "--setenv", "--inject-key", "--check", "--patch", "--json", "--all",
        "--force", "--pick", "--name", "true", "x",
    ];
    const VALUES: [&str; 14] = [
        "", "0", "-1", "1.5", "1e309", "NaN", "64G", "999999999999", "a=b", "=", "../x", "anthropic", "\u{1F600}", "a\tb",
    ];
    let dir = scratch("cli");
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    let mut rng = Rng(seed());
    for case in 0..iters() / 3 {
        let mut args: Vec<String> = Vec::new();
        for _ in 0..rng.below(7) {
            args.push(if rng.below(3) == 0 { VALUES[rng.below(VALUES.len())] } else { WORDS[rng.below(WORDS.len())] }.to_owned());
        }
        // Never let a random command line run something real: only `true`.
        if let Some(i) = args.iter().position(|a| a == "--") {
            args.truncate(i + 1);
            args.push("true".to_owned());
        }
        let out = Command::new(env!("CARGO_BIN_EXE_foil"))
            .args(&args)
            .current_dir(dir.join("ws"))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &dir)
            .env("FOIL_HOME", dir.join("home"))
            .env("FOIL_NO_SCOPE", "1")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.code() != Some(101) && !stderr.contains("panicked"), "case {case}: foil panicked on {args:?}:\n{stderr}");
    }
    let _ = Command::new("chmod").args(["-R", "u+rwx"]).arg(&dir).status();
    let _ = std::fs::remove_dir_all(&dir);
}
