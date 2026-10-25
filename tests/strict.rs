//! The escape suite for `--strict`: each `cannot_*` test tries something a
//! strict run must not be able to do, and checks that it failed; the other
//! tests check that strict runs still behave like ordinary runs.
//!
//! Like branches, strict runs need unprivileged user namespaces; where those
//! are blocked the tests are skipped, unless FOIL_REQUIRE_BRANCH is set,
//! which turns a skip into a failure. Folders live in Cargo's target dir.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A workspace, a fake home folder with secrets, and a folder outside both.
struct Env {
    root: PathBuf,
}

impl Env {
    fn new(test: &str) -> Env {
        // Tests run in parallel threads of one process: a counter keeps
        // every Env (each probe included) in its own folder.
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("strict-{}-{n}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["ws", "home/.ssh", "other", "branches"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("ws/a.txt"), "base\n").unwrap();
        std::fs::write(root.join("home/.ssh/id_ed25519"), "PRIVATE KEY\n").unwrap();
        std::fs::write(root.join("other/secret.txt"), "other repo secret\n").unwrap();
        Env { root: root.canonicalize().unwrap() }
    }

    /// `foil run --strict <options> -- sh -c <script>` in the workspace.
    fn cmd(&self, options: &[&str], script: &str) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_foil"));
        cmd.arg("run")
            .arg("--strict")
            .args(options)
            .args(["--", "sh", "-c", script])
            .current_dir(self.root.join("ws"))
            .env("HOME", self.root.join("home"))
            .env("FOIL_HOME", self.root.join("branches"));
        cmd
    }

    fn run(&self, options: &[&str], script: &str) -> Output {
        self.cmd(options, script).output().unwrap()
    }

    fn path(&self, rel: &str) -> String {
        self.root.join(rel).display().to_string()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = Command::new("chmod").args(["-R", "u+rwx"]).arg(&self.root).status();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Whether strict runs work here; skips (or fails, if required) otherwise.
fn available() -> bool {
    let env = Env::new("probe");
    let out = env.run(&[], "true");
    if out.status.success() {
        return true;
    }
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("cannot isolate the command") && std::env::var_os("FOIL_REQUIRE_BRANCH").is_none(),
        "strict runs unavailable: {stderr}"
    );
    eprintln!("skipped: strict runs unavailable here: {stderr}");
    false
}

fn has(program: &str) -> bool {
    Command::new("sh").args(["-c", &format!("command -v {program}")]).stdout(Stdio::null()).status().is_ok_and(|s| s.success())
}

/// A one-shot HTTP server on the host's loopback; `respond` builds the
/// reply body from the request head. Returns the port.
fn serve_once(respond: fn(&str) -> String) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut head = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                head.push_str(&line);
            }
            let len = head
                .lines()
                .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
                .unwrap_or(0);
            let mut body = vec![0u8; len];
            let _ = reader.read_exact(&mut body);
            let reply = respond(&head);
            let mut conn = conn;
            let _ = write!(conn, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len());
        }
    });
    port
}

// ---------------------------------------------------------------- escapes

#[test]
fn cannot_write_outside_the_branch() {
    if !available() {
        return;
    }
    let env = Env::new("write");
    let script = format!(
        "echo x > {other}/new.txt; echo x > {home}/.bashrc; echo x > /usr/foil-test; echo ok > a.txt; cat a.txt",
        other = env.path("other"),
        home = env.path("home")
    );
    let out = env.run(&["-b", "t"], &script);

    assert_eq!(text(&out.stdout), "ok\n", "{}", text(&out.stderr));
    assert!(!env.root.join("other/new.txt").exists(), "wrote into another folder");
    assert!(!env.root.join("home/.bashrc").exists(), "wrote into the home folder");
    assert!(!PathBuf::from("/usr/foil-test").exists());
    assert_eq!(std::fs::read_to_string(env.root.join("ws/a.txt")).unwrap(), "base\n", "wrote past the branch");
}

#[test]
fn cannot_read_secrets_or_other_folders() {
    if !available() {
        return;
    }
    let env = Env::new("read");
    let script = format!(
        "cat {home}/.ssh/id_ed25519; cat {other}/secret.txt; ls -A {home}; echo done",
        home = env.path("home"),
        other = env.path("other")
    );
    let out = env.run(&[], &script);

    let stdout = text(&out.stdout);
    assert!(!stdout.contains("PRIVATE KEY") && !stdout.contains("other repo secret"), "{stdout}");
    assert_eq!(stdout, "done\n", "the home folder is not empty: {stdout}");
}

#[test]
fn cannot_see_environment_secrets_unless_allowed() {
    if !available() {
        return;
    }
    let env = Env::new("env");
    let hidden = env.cmd(&[], "env").env("MY_SECRET_TOKEN", "tok-123").output().unwrap();
    let allowed = env.cmd(&["--allow-env", "MY_SECRET_TOKEN"], "env").env("MY_SECRET_TOKEN", "tok-123").output().unwrap();

    assert!(!text(&hidden.stdout).contains("tok-123"), "secret leaked into the environment");
    assert!(text(&allowed.stdout).contains("MY_SECRET_TOKEN=tok-123"), "--allow-env did not pass it");
}

#[test]
fn cannot_reach_the_network_or_resolve_names() {
    if !available() {
        return;
    }
    let env = Env::new("net");
    let out = env.run(&[], "tail -n +3 /proc/net/dev | cut -d: -f1 | tr -d ' '; getent hosts example.com || echo no-dns");

    assert_eq!(text(&out.stdout), "lo\nno-dns\n");
}

#[test]
fn cannot_reach_hosts_outside_the_allowlist() {
    if !available() || !has("curl") {
        return;
    }
    let env = Env::new("allow");
    let allowed = serve_once(|_| "allowed-body".to_owned());
    let other = serve_once(|_| "must-not-be-seen".to_owned());
    let script = format!(
        "curl -s --noproxy '' -x http://127.0.0.1:3128 http://127.0.0.1:{allowed}/; echo; \
         curl -s --noproxy '' -x http://127.0.0.1:3128 http://127.0.0.1:{other}/; echo; \
         curl -s --noproxy '' -p -x http://127.0.0.1:3128 http://127.0.0.1:{other}/ || echo tunnel-refused"
    );
    let out = env.run(&["--allow-host", &format!("127.0.0.1:{allowed}")], &script);

    let stdout = text(&out.stdout);
    assert!(stdout.starts_with("allowed-body\n"), "{stdout}\n{}", text(&out.stderr));
    assert!(!stdout.contains("must-not-be-seen"), "{stdout}");
    assert!(stdout.contains("tunnel-refused") || stdout.contains("blocked by foil"), "{stdout}");
    assert!(text(&out.stderr).contains("blocked network access"), "{}", text(&out.stderr));
}

#[test]
fn cannot_see_the_injected_api_key() {
    if !available() || !has("curl") {
        return;
    }
    let env = Env::new("inject");
    let api = serve_once(|head| {
        let ok = head.lines().any(|l| l.eq_ignore_ascii_case("x-api-key: sk-real-key"));
        format!("key-ok={ok}")
    });
    let script = "echo \"key=$ANTHROPIC_API_KEY\"; curl -s \"$ANTHROPIC_BASE_URL/v1/messages\" -d '{}'";
    let out = env
        .cmd(&["--inject-key", "anthropic"], script)
        .env("ANTHROPIC_API_KEY", "sk-real-key")
        .env("FOIL_INJECT_ANTHROPIC_URL", format!("http://127.0.0.1:{api}"))
        .output()
        .unwrap();

    let stdout = text(&out.stdout);
    assert!(!stdout.contains("sk-real-key"), "the key is visible inside: {stdout}");
    assert!(stdout.contains("key-ok=true"), "the API did not get the key: {stdout}\n{}", text(&out.stderr));
}

#[test]
fn cannot_see_or_signal_other_processes() {
    if !available() {
        return;
    }
    let env = Env::new("pids");
    let mut outside = Command::new("sleep").arg("30").spawn().unwrap();
    let pid = outside.id();
    let out = env.run(&[], &format!("kill -0 {pid} 2>/dev/null && echo SIGNALLED || echo unreachable; ls /proc | grep -c '^[0-9]'"));
    let _ = outside.kill();
    let _ = outside.wait();

    let stdout = text(&out.stdout);
    assert!(stdout.starts_with("unreachable\n"), "{stdout}");
    let visible: usize = stdout.lines().nth(1).unwrap().parse().unwrap();
    assert!(visible <= 5, "{visible} processes visible: the run should see only its own");
}

#[test]
fn cannot_signal_foils_process_group() {
    if !available() {
        return;
    }
    // The wrapper shell shares foil's process group; kill(0) from inside
    // would end it. Without a terminal the command has a session of its own.
    let env = Env::new("pgrp");
    let out = Command::new("sh")
        .args(["-c", "\"$0\" run --strict -- sh -c 'kill -USR1 0'; echo wrapper-alive", env!("CARGO_BIN_EXE_foil")])
        .current_dir(env.root.join("ws"))
        .env("HOME", env.root.join("home"))
        .env("FOIL_HOME", env.root.join("branches"))
        .output()
        .unwrap();
    assert!(text(&out.stdout).contains("wrapper-alive"), "{:?} {}", out.status, text(&out.stderr));
}

#[test]
fn cannot_use_descriptors_foil_inherited() {
    if !available() {
        return;
    }
    // An open folder outside the view would allow openat(fd, "..") walks.
    let env = Env::new("fds");
    let out = Command::new("sh")
        .args([
            "-c",
            "exec 7<\"$1\"; \"$0\" run --strict -- sh -c '[ -e /proc/self/fd/7 ] && echo LEAKED || echo closed'",
            env!("CARGO_BIN_EXE_foil"),
            &env.path("other"),
        ])
        .current_dir(env.root.join("ws"))
        .env("HOME", env.root.join("home"))
        .env("FOIL_HOME", env.root.join("branches"))
        .output()
        .unwrap();
    assert_eq!(text(&out.stdout), "closed\n", "{}", text(&out.stderr));
}

#[test]
fn cannot_reach_this_machine_through_a_wildcard() {
    if !available() || !has("curl") {
        return;
    }
    // `*` allows any host by name, but not one that resolves to the host's
    // own services: only a host named exactly may be internal.
    let env = Env::new("internal");
    let port = serve_once(|_| "must-not-be-seen".to_owned());
    let script = format!(
        "curl -s --noproxy '' -x http://127.0.0.1:3128 http://localhost:{port}/; echo; \
         curl -s --noproxy '' -p -x http://127.0.0.1:3128 http://localhost:{port}/ || echo tunnel-refused"
    );
    let out = env.run(&["--allow-host", &format!("*:{port}")], &script);

    let stdout = text(&out.stdout);
    assert!(!stdout.contains("must-not-be-seen"), "{stdout}");
    assert!(text(&out.stderr).contains("resolves to an address of this machine"), "{}", text(&out.stderr));
    let bad = env.run(&["--allow-host", "example.com:99999"], "true");
    assert_eq!(bad.status.code(), Some(125), "{}", text(&bad.stderr));
}

#[test]
fn cannot_gain_privileges() {
    if !available() {
        return;
    }
    let env = Env::new("privs");
    let out = env.run(&[], "grep NoNewPrivs /proc/self/status; (sudo -n true 2>/dev/null && echo ROOT) || echo no-root");

    assert_eq!(text(&out.stdout), "NoNewPrivs:\t1\nno-root\n");
}

#[test]
fn cannot_reach_local_services_or_host_tmp() {
    if !available() {
        return;
    }
    let env = Env::new("sockets");
    let marker = std::env::temp_dir().join(format!("foil-strict-marker-{}", std::process::id()));
    std::fs::write(&marker, "host tmp\n").unwrap();
    let script = format!(
        "ls /run; test -e /run/user && echo RUN-USER; test -e /var/run/docker.sock && echo DOCKER; \
         test -e {} && echo HOST-TMP; echo \"xdg=$XDG_RUNTIME_DIR ssh=$SSH_AUTH_SOCK\"",
        marker.display()
    );
    let out = env
        .cmd(&[], &script)
        // foil itself keeps XDG_RUNTIME_DIR (it may need systemd); the run must not.
        .env("SSH_AUTH_SOCK", "/tmp/ssh-agent.sock")
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&marker);

    assert_eq!(text(&out.stdout), "xdg= ssh=\n", "{}", text(&out.stderr));
}

/// A listener on the host's loopback that counts connections and answers
/// nothing useful; for checking that a forbidden host is never contacted.
fn listen_counting(count: &'static AtomicUsize) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            count.fetch_add(1, Ordering::SeqCst);
            let mut conn = conn;
            let _ = conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\nConnection: close\r\n\r\nforbidden");
        }
    });
    port
}

/// Send `raw` bytes to foil's proxy from inside a strict run; print the reply.
const RAW_TO_PROXY: &str = r#"import socket, sys
s = socket.create_connection(("127.0.0.1", 3128)); s.settimeout(5)
s.sendall(sys.argv[1].encode("latin-1").decode("unicode_escape").encode("latin-1"))
out = b""
try:
    while True:
        d = s.recv(4096)
        if not d: break
        out += d
except Exception: pass
print(out.decode("latin-1").split("\r\n")[0])"#;

#[test]
fn cannot_find_the_key_anywhere_in_proc() {
    if !available() || !has("python3") {
        return;
    }
    let env = Env::new("procscan");
    // `sk-real-ke[y]` matches the key but not this script's own command line.
    let script = "grep -l 'sk-real-ke[y]' /proc/[0-9]*/environ /proc/[0-9]*/cmdline 2>/dev/null; \
                  grep -rl 'sk-real-ke[y]' /run /tmp 2>/dev/null; echo scanned";
    let out = env.cmd(&["--inject-key", "anthropic"], script).env("ANTHROPIC_API_KEY", "sk-real-key").output().unwrap();
    assert_eq!(text(&out.stdout), "scanned\n", "the key was found here: {}", text(&out.stdout));
}

#[test]
fn cannot_reach_host_abstract_unix_sockets() {
    if !available() || !has("python3") {
        return;
    }
    let env = Env::new("abstract");
    let name = format!("foil-test-{}", std::process::id());
    let mut host = Command::new("python3")
        .args(["-c", &format!("import socket,time; s=socket.socket(socket.AF_UNIX); s.bind('\\0{name}'); s.listen(); time.sleep(20)")])
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let script = format!(
        "python3 -c \"import socket; s=socket.socket(socket.AF_UNIX); s.connect('\\0{name}'); print('CONNECTED')\" 2>/dev/null || echo refused"
    );
    let out = env.run(&[], &script);
    let _ = host.kill();
    let _ = host.wait();
    assert_eq!(text(&out.stdout), "refused\n", "{}", text(&out.stderr));
}

#[test]
fn cannot_uncover_hidden_files_with_a_nested_namespace() {
    if !available() || !has("unshare") {
        return;
    }
    let env = Env::new("nested");
    let script = format!(
        "unshare -Urm sh -c 'umount -l {home} 2>/dev/null; umount -l / 2>/dev/null; mount -t proc proc /proc 2>/dev/null; \
         cat {home}/.ssh/id_ed25519 {other}/secret.txt 2>/dev/null; ls -A {home}' 2>/dev/null; echo done",
        home = env.path("home"),
        other = env.path("other")
    );
    let out = env.run(&[], &script);
    let stdout = text(&out.stdout);
    assert!(!stdout.contains("PRIVATE KEY") && !stdout.contains("other repo secret"), "{stdout}");
}

#[test]
fn cannot_open_raw_devices_or_change_the_kernel() {
    if !available() {
        return;
    }
    let env = Env::new("devices");
    let script = "for d in /dev/mem /dev/kmem /dev/kmsg /dev/sda /dev/vda /dev/nvme0n1 /dev/port; do \
                    head -c1 $d >/dev/null 2>&1 && echo OPENED $d; done; \
                  echo 1 > /proc/sys/kernel/sysrq 2>/dev/null && echo SYSCTL; \
                  echo x > /sys/kernel/foil 2>/dev/null && echo SYSFS; \
                  mount -t tmpfs t /mnt 2>/dev/null && echo MOUNTED; echo done";
    let out = env.run(&[], script);
    assert_eq!(text(&out.stdout), "done\n", "{}", text(&out.stderr));
}

#[test]
fn cannot_write_through_a_workspace_symlink_to_outside() {
    if !available() {
        return;
    }
    let env = Env::new("symlink-out");
    std::os::unix::fs::symlink(env.root.join("home/.ssh"), env.root.join("ws/escape")).unwrap();
    let out = env.run(&["-b", "t"], "echo pwned > escape/authorized_keys 2>/dev/null; echo done");
    assert_eq!(text(&out.stdout), "done\n");
    assert!(!env.root.join("home/.ssh/authorized_keys").exists(), "wrote through the symlink");
}

#[test]
fn commit_does_not_follow_a_symlink_the_branch_replaced() {
    if !available() {
        return;
    }
    let env = Env::new("symlink-commit");
    std::os::unix::fs::symlink(env.root.join("other"), env.root.join("ws/d")).unwrap();
    let out = env.run(&["-b", "t"], "rm d && mkdir d && echo x > d/planted");
    assert!(out.status.success(), "{}", text(&out.stderr));
    let commit = Command::new(env!("CARGO_BIN_EXE_foil"))
        .args(["commit", "t"])
        .current_dir(env.root.join("ws"))
        .env("FOIL_HOME", env.root.join("branches"))
        .output()
        .unwrap();
    assert!(commit.status.success(), "{}", text(&commit.stderr));
    assert!(!env.root.join("other/planted").exists(), "commit wrote through the old symlink");
    assert!(env.root.join("ws/d").symlink_metadata().unwrap().is_dir());
}

#[test]
fn the_proxy_refuses_smuggled_and_disguised_requests() {
    if !available() || !has("python3") {
        return;
    }
    static FORBIDDEN: AtomicUsize = AtomicUsize::new(0);
    let env = Env::new("smuggle");
    let allowed = serve_once(|_| "allowed-body".to_owned());
    let other = listen_counting(&FORBIDDEN);
    let attempts = [
        // userinfo: "allowed@forbidden"
        format!("GET http://127.0.0.1:{allowed}@127.0.0.1:{other}/ HTTP/1.1\\r\\n\\r\\n"),
        // a Host header naming the forbidden host
        format!("GET http://127.0.0.1:{allowed}/ HTTP/1.1\\r\\nHost: 127.0.0.1:{other}\\r\\n\\r\\n"),
        // a second request pipelined behind an allowed one
        format!("GET http://127.0.0.1:{allowed}/ HTTP/1.1\\r\\nContent-Length: 0\\r\\n\\r\\nGET http://127.0.0.1:{other}/ HTTP/1.1\\r\\n\\r\\n"),
        // a header with a bare CR, to split the request upstream
        format!("GET http://127.0.0.1:{allowed}/ HTTP/1.1\\r\\nX: a\\rHost: 127.0.0.1:{other}\\r\\n\\r\\n"),
        // CONNECT to an allowed host but another port
        format!("CONNECT 127.0.0.1:{other} HTTP/1.1\\r\\n\\r\\n"),
        // a name that ends like an allowed domain
        "CONNECT evil.example/.allowed.test:443 HTTP/1.1\\r\\n\\r\\n".to_owned(),
        // an absurdly long header line
        format!("GET http://127.0.0.1:{allowed}/ HTTP/1.1\\r\\nX: {}\\r\\n\\r\\n", "a".repeat(100_000)),
    ];
    let mut script = String::new();
    for attempt in &attempts {
        script.push_str(&format!("python3 -c '{}' '{}'; ", RAW_TO_PROXY.replace('\'', "'\\''"), attempt));
    }
    let out = env.run(&["--allow-host", &format!("127.0.0.1:{allowed}"), "--allow-host", "*.allowed.test"], &script);
    std::thread::sleep(Duration::from_millis(200));
    // Every attempt really reached the proxy and got an answer line.
    assert_eq!(text(&out.stdout).lines().count(), attempts.len(), "{}\n{}", text(&out.stdout), text(&out.stderr));
    eprintln!("replies:\n{}", text(&out.stdout));
    assert_eq!(FORBIDDEN.load(Ordering::SeqCst), 0, "the forbidden host was contacted:\n{}\n{}", text(&out.stdout), text(&out.stderr));
    assert!(!text(&out.stdout).contains("forbidden"), "{}", text(&out.stdout));
    // Refused by the proxy's own check, not only by DNS failing afterwards.
    assert!(text(&out.stderr).contains("blocked network access to evil.example/.allowed.test"), "{}", text(&out.stderr));
}

// ---------------------------------------------------------------- still works

#[test]
fn strict_runs_keep_identity_folder_and_exit_code() {
    if !available() {
        return;
    }
    let env = Env::new("identity");
    let out = env.run(&[], "id -u; id -g; pwd; exit 7");

    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    assert_eq!(out.status.code(), Some(7));
    assert_eq!(text(&out.stdout), format!("{uid}\n{gid}\n{}\n", env.path("ws")));
}

#[test]
fn strict_runs_pass_signals_and_die_the_same_way() {
    if !available() {
        return;
    }
    use std::os::unix::process::ExitStatusExt;
    let env = Env::new("signals");
    let mut child = env.cmd(&[], "sleep 30").spawn().unwrap();
    std::thread::sleep(Duration::from_millis(500));
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let status = child.wait().unwrap();

    assert_eq!(status.signal(), Some(libc::SIGTERM));
}

#[test]
fn strict_branch_commits_like_any_branch() {
    if !available() {
        return;
    }
    let env = Env::new("commit");
    let out = env.run(&["-b", "t"], "echo changed > a.txt; echo new > b.txt");
    assert!(out.status.success(), "{}", text(&out.stderr));
    let commit = Command::new(env!("CARGO_BIN_EXE_foil"))
        .args(["commit", "t"])
        .current_dir(env.root.join("ws"))
        .env("FOIL_HOME", env.root.join("branches"))
        .output()
        .unwrap();

    assert!(commit.status.success(), "{}", text(&commit.stderr));
    assert_eq!(std::fs::read_to_string(env.root.join("ws/a.txt")).unwrap(), "changed\n");
    assert_eq!(std::fs::read_to_string(env.root.join("ws/b.txt")).unwrap(), "new\n");
}

#[test]
fn leftovers_inside_a_strict_run_end_with_it() {
    if !available() {
        return;
    }
    let env = Env::new("leftovers");
    // A daemon that detaches from the shell and would otherwise live on.
    let out = env.run(&[], "setsid sleep 299.5 >/dev/null 2>&1 < /dev/null & echo started");
    assert_eq!(text(&out.stdout), "started\n");
    std::thread::sleep(Duration::from_millis(200));
    let found = Command::new("pgrep").args(["-f", "sleep 299.5"]).output().unwrap();

    assert!(found.stdout.is_empty(), "a leftover survived the run: {}", text(&found.stdout));
}

#[test]
fn doctor_finishes_and_checks_strict_mode() {
    if !available() {
        return;
    }
    let env = Env::new("doctor");
    // Its strict probe once waited forever when signals weren't blocked.
    let out = Command::new("timeout")
        .args(["60", env!("CARGO_BIN_EXE_foil"), "doctor"])
        .env("FOIL_HOME", env.root.join("branches"))
        .output()
        .unwrap();

    assert_ne!(out.status.code(), Some(124), "foil doctor hung");
    assert!(text(&out.stdout).lines().any(|l| l.starts_with("ok  --strict")), "{}", text(&out.stdout));
}
