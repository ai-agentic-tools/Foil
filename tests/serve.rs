//! `foil serve`: the OpenSandbox lifecycle API over foil branches, and the
//! execd relay into each sandbox. (`integrations/opensandbox/sdk_e2e.py`
//! drives the same server with the real OpenSandbox SDK.)

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};

use common::Env;
use serde_json::{json, Value};

const KEY: &str = "test-key";

struct Serve {
    child: Child,
    addr: String,
}

impl Serve {
    fn start(env: &Env, extra: &[&str]) -> Serve {
        let mut child = env
            .foil(&[&["serve", "--listen", "127.0.0.1:0"], extra].concat())
            .env("OPEN_SANDBOX_API_KEY", KEY)
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stderr = BufReader::new(child.stderr.take().unwrap());
        let mut line = String::new();
        let addr = loop {
            line.clear();
            assert!(stderr.read_line(&mut line).unwrap() > 0, "foil serve exited");
            if let Some(rest) = line.split("http://").nth(1) {
                break rest.split_whitespace().next().unwrap().to_owned();
            }
        };
        // Keep reading so the server never blocks on a full pipe.
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut stderr, &mut std::io::sink());
        });
        Serve { child, addr }
    }

    /// One request; status, headers (lowercase names) and body.
    fn call(&self, method: &str, path: &str, headers: &[(&str, &str)], body: Option<&Value>) -> (u16, Vec<(String, String)>, Vec<u8>) {
        let mut conn = TcpStream::connect(&self.addr).unwrap();
        let body = body.map(Value::to_string).unwrap_or_default();
        let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {}\r\nContent-Length: {}\r\n", self.addr, body.len());
        for (k, v) in headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str("\r\n");
        conn.write_all(req.as_bytes()).unwrap();
        conn.write_all(body.as_bytes()).unwrap();
        let mut raw = Vec::new();
        conn.read_to_end(&mut raw).unwrap();
        let split = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("a response head");
        let head = String::from_utf8_lossy(&raw[..split]).into_owned();
        let mut lines = head.lines();
        let status = lines.next().unwrap().split(' ').nth(1).unwrap().parse().unwrap();
        let headers = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_owned())).collect();
        (status, headers, raw[split + 4..].to_vec())
    }

    fn api(&self, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
        let (status, _, body) = self.call(method, &format!("/v1{path}"), &[("OPEN-SANDBOX-API-KEY", KEY)], body);
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    fn create(&self) -> (String, String, String) {
        let (status, sandbox) = self.api("POST", "/sandboxes", Some(&json!({ "image": { "uri": "foil" }, "timeout": 600 })));
        assert_eq!(status, 202, "{sandbox}");
        assert_eq!(sandbox["status"]["state"], "Running");
        let id = sandbox["id"].as_str().unwrap().to_owned();
        let (status, endpoint) = self.api("GET", &format!("/sandboxes/{id}/endpoints/44772"), None);
        assert_eq!(status, 200);
        let base = endpoint["endpoint"].as_str().unwrap();
        let path = base.split_once('/').unwrap().1;
        let token = endpoint["headers"]["X-EXECD-ACCESS-TOKEN"].as_str().unwrap().to_owned();
        (id, format!("/{path}"), token)
    }

    /// Run `command` in a sandbox through the relay; its events.
    fn run(&self, base: &str, token: &str, command: &str) -> Vec<Value> {
        let (status, headers, body) =
            self.call("POST", &format!("{base}/command"), &[("X-EXECD-ACCESS-TOKEN", token)], Some(&json!({ "command": command })));
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
        assert!(headers.iter().any(|(k, v)| k == "content-type" && v == "text/event-stream"));
        String::from_utf8(body).unwrap().split("\n\n").filter(|e| !e.trim().is_empty()).map(|e| serde_json::from_str(e).unwrap()).collect()
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn stdout(events: &[Value]) -> String {
    events.iter().filter(|e| e["type"] == "stdout").map(|e| e["text"].as_str().unwrap()).collect()
}

/// Delete every sandbox, so no run outlives the test.
fn cleanup(serve: &Serve) {
    let (_, list) = serve.api("GET", "/sandboxes", None);
    for s in list["items"].as_array().cloned().unwrap_or_default() {
        serve.api("DELETE", &format!("/sandboxes/{}", s["id"].as_str().unwrap()), None);
    }
}

#[test]
fn sandboxes_run_commands_and_commit_back() {
    let env = Env::new("serve");
    std::fs::write(env.ws().join("a.txt"), "old\n").unwrap();
    let serve = Serve::start(&env, &[]);
    let (id, base, token) = serve.create();

    let events = serve.run(&base, &token, "cat a.txt; echo new > a.txt; echo b > b.txt; exit 3");
    assert_eq!(events[0]["type"], "init");
    assert_eq!(stdout(&events), "old\n");
    let last = events.last().unwrap();
    assert_eq!(last["type"], "error");
    assert_eq!(last["error"]["exit_code"], 3);
    assert_eq!(std::fs::read_to_string(env.ws().join("a.txt")).unwrap(), "old\n", "the workspace waits for the commit");

    let (status, diff) = serve.api("GET", &format!("/sandboxes/{id}/diff"), None);
    assert_eq!(status, 200);
    assert_eq!(diff["changes"], json!([{ "kind": "M", "path": "a.txt" }, { "kind": "A", "path": "b.txt" }]));

    let (status, applied) = serve.api("POST", &format!("/sandboxes/{id}/commit"), Some(&json!({})));
    assert_eq!(status, 200, "{applied}");
    assert_eq!(applied["applied"], 2);
    assert_eq!(std::fs::read_to_string(env.ws().join("a.txt")).unwrap(), "new\n");
    assert_eq!(serve.api("GET", &format!("/sandboxes/{id}"), None).0, 404);
    cleanup(&serve);
}

#[test]
fn keys_and_tokens_are_checked_and_never_reach_the_sandbox() {
    let env = Env::new("serve-auth");
    let serve = Serve::start(&env, &[]);
    assert_eq!(serve.call("GET", "/v1/sandboxes", &[], None).0, 401);
    assert_eq!(serve.call("GET", "/v1/sandboxes", &[("OPEN-SANDBOX-API-KEY", "wrong")], None).0, 401);
    let (_, base, token) = serve.create();
    assert_eq!(serve.call("GET", &format!("{base}/ping"), &[], None).0, 401);
    assert_eq!(serve.call("GET", &format!("{base}/ping"), &[("X-EXECD-ACCESS-TOKEN", "wrong")], None).0, 401);
    assert_eq!(serve.call("GET", &format!("{base}/ping"), &[("X-EXECD-ACCESS-TOKEN", &token)], None).0, 200);
    // The API key works for the relay too (the SDK's server-proxy mode).
    assert_eq!(serve.call("GET", &format!("{base}/ping"), &[("OPEN-SANDBOX-API-KEY", KEY)], None).0, 200);
    // execd sees neither credential.
    let events = serve.run(&base, &token, "env; echo; echo \"$OPEN_SANDBOX_API_KEY\"");
    let out = stdout(&events);
    assert!(!out.contains(&token) && !out.contains(KEY), "{out}");
    cleanup(&serve);
}

#[test]
fn a_conflicting_commit_is_refused_and_the_sandbox_lives_on() {
    let env = Env::new("serve-conflict");
    std::fs::write(env.ws().join("a.txt"), "old\n").unwrap();
    let serve = Serve::start(&env, &[]);
    let (id, base, token) = serve.create();
    serve.run(&base, &token, "echo sandbox > a.txt");
    std::fs::write(env.ws().join("a.txt"), "someone else\n").unwrap();

    let (status, err) = serve.api("POST", &format!("/sandboxes/{id}/commit"), Some(&json!({})));
    assert_eq!(status, 409, "{err}");
    assert_eq!(err["code"], "CONFLICT");
    assert_eq!(std::fs::read_to_string(env.ws().join("a.txt")).unwrap(), "someone else\n");
    assert_eq!(stdout(&serve.run(&base, &token, "cat a.txt")), "sandbox\n", "still running after the refusal");

    let (status, applied) = serve.api("POST", &format!("/sandboxes/{id}/commit"), Some(&json!({ "force": true })));
    assert_eq!(status, 200, "{applied}");
    assert_eq!(std::fs::read_to_string(env.ws().join("a.txt")).unwrap(), "sandbox\n");
    cleanup(&serve);
}

#[test]
fn deleting_discards_and_expired_sandboxes_go_away() {
    let env = Env::new("serve-delete");
    let serve = Serve::start(&env, &[]);
    let (id, base, token) = serve.create();
    serve.run(&base, &token, "echo x > x.txt");
    assert_eq!(serve.api("DELETE", &format!("/sandboxes/{id}"), None).0, 204);
    assert!(!env.ws().join("x.txt").exists());
    assert_eq!(serve.api("GET", &format!("/sandboxes/{id}"), None).0, 404);

    let (status, sandbox) = serve.api("POST", "/sandboxes", Some(&json!({ "image": { "uri": "foil" }, "timeout": 1 })));
    assert_eq!(status, 202);
    let id = sandbox["id"].as_str().unwrap();
    let start = std::time::Instant::now();
    while serve.api("GET", &format!("/sandboxes/{id}"), None).0 != 404 {
        assert!(start.elapsed().as_secs() < 15, "an expired sandbox is deleted");
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let branches = String::from_utf8(env.foil(&["list"]).output().unwrap().stdout).unwrap();
    assert!(branches.trim().is_empty(), "no branch left: {branches}");
}

#[test]
fn sandboxes_survive_a_server_restart() {
    let env = Env::new("serve-restart");
    let first = Serve::start(&env, &[]);
    let (id, base, token) = first.create();
    first.run(&base, &token, "echo kept > kept.txt");
    drop(first);
    let second = Serve::start(&env, &[]);
    let (status, sandbox) = second.api("GET", &format!("/sandboxes/{id}"), None);
    assert_eq!((status, sandbox["status"]["state"].as_str()), (200, Some("Running")));
    assert_eq!(stdout(&second.run(&base, &token, "cat kept.txt")), "kept\n");
    cleanup(&second);
}

#[test]
fn strict_sandboxes_and_network_policies() {
    let env = Env::new("serve-strict");
    let serve = Serve::start(&env, &["--strict"]);
    let (id, base, token) = serve.create();
    let (_, sandbox) = serve.api("GET", &format!("/sandboxes/{id}"), None);
    assert_eq!(sandbox["extensions"]["foil.strict"], "true");
    // A strict sandbox can't see what is in the real home.
    let secret = std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(format!(".foil-serve-test-{}", std::process::id()));
    std::fs::write(&secret, "secret").unwrap();
    let events = serve.run(&base, &token, &format!("cat {} 2>/dev/null || echo hidden", secret.display()));
    let _ = std::fs::remove_file(&secret);
    assert_eq!(stdout(&events), "hidden\n");
    // Nor another sandbox's execd.
    let (other, _, _) = serve.create();
    let uid = String::from_utf8(Command::new("id").arg("-u").output().unwrap().stdout).unwrap();
    let socket = std::path::Path::new(&format!("/tmp/foil-{}", uid.trim())).canonicalize().unwrap().join(&other[..8]).join("execd.sock");
    assert!(socket.exists());
    let events = serve.run(
        &base,
        &token,
        &format!("command -v curl >/dev/null || {{ echo refused; exit; }}; curl -s --max-time 2 --unix-socket {} http://x/ping && echo reached || echo refused", socket.display()),
    );
    assert_eq!(stdout(&events), "refused\n");
    // A policy foil can't enforce is refused, not ignored.
    let (status, err) = serve.api(
        "POST",
        "/sandboxes",
        Some(&json!({ "image": { "uri": "foil" }, "networkPolicy": { "defaultAction": "deny", "egress": [{ "action": "allow", "target": "10.0.0.0/8" }] } })),
    );
    assert_eq!(status, 400, "{err}");
    cleanup(&serve);
}
