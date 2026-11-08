use std::ffi::OsString;
use std::io::{ErrorKind, Write};
use std::path::PathBuf;
use std::process::exit;

use foil::branch::Branch;
use foil::isolate::{self, Options};
use foil::limits::{parse_duration, parse_size, Limits};
use foil::supervisor::{exit_like, supervise_for, Termination};

/// Exit status of a run stopped by `--timeout`, as with timeout(1).
const TIMED_OUT: i32 = 124;

const USAGE: &str = "\
usage: foil run [options] [--] <command> [args...]
         -b, --branch NAME     run in branch NAME of the workspace (created if new)
         -w, --workspace DIR   the folder a new branch branches (default: .)
         --protect             make everything but the branch, /tmp, /var/tmp and
                               /dev/shm read-only for the command
         --allow-write PATH    with --protect or --strict: PATH writable (repeatable)
         --no-network          no network for the command (loopback only)
         --strict              deny by default: a private view with only system
                               folders, the workspace and allowed paths; own PID
                               and IPC namespaces; no privilege gain; clean env
         --allow-read PATH     with --strict: show PATH read-only (repeatable)
         --allow-host HOST     with --strict: allow network access to HOST through
                               foil's proxy (host, host:port, *.domain; repeatable)
         --inject-key API      with --strict: the proxy adds API's key (anthropic,
                               openai) from foil's environment; the run never sees it
         --allow-env NAME      with --strict: pass environment variable NAME
         --setenv NAME=VALUE   with --strict: set NAME inside the run
         --memory SIZE         memory limit, e.g. 2G (needs systemd)
         --cpus N              CPU limit, e.g. 1.5 (needs systemd)
         --pids N              limit on processes and threads (needs systemd)
         --timeout TIME        stop the command after TIME (90s, 10m, 2h): SIGTERM,
                               then SIGKILL after the grace period; exit 124
         --hide PATH           with --strict: the command can neither read nor write
                               PATH in the workspace (.env, keys; * within a name)
         --policy FILE         rules to follow, one option per line (default for strict
                               runs: the workspace's .foil/policy, if it has one)
         --audit FILE          record the run, and every network request allowed or
                               blocked, as JSON lines (branch runs: always, in
                               $FOIL_HOME/audit/BRANCH.jsonl)
       foil agent NAME [run options] [-- agent args]
                               run agent NAME (claude, codex) in a strict branch with
                               its profile; afterwards, review and commit or abort
       foil fanout -n N [--check CMD] [--agent NAME] [--name BASE] [--pick] [--json]
                   [run options] [--] <command or agent args...>
                               run N attempts at once, in branches BASE-1..BASE-N
                               (FOIL_ATTEMPT=i in each); CMD scores each attempt in
                               its branch (exit 0 = pass); the best is the passing
                               attempt with the fewest changes; --pick commits it;
                               --json prints the results on stdout
       foil pick NAME          commit fanout attempt NAME, discard its siblings
       foil session [--] <command> [args...]
                               run command (an agent harness, a script that starts
                               many foil runs at once) in one systemd scope foil may
                               manage, so runs inside it start without waiting for
                               systemd one at a time
       foil diff [--patch | --json] [--all] NAME
                               what branch NAME changed (A added, M modified,
                               D deleted, R replaced), or the full patch;
                               paths .gitignore ignores only with --all
       foil commit [--force] [--all] [--guarded] NAME
                               apply branch NAME to its workspace, then delete it;
                               refuses files changed there since, unless --force;
                               leaves out ignored paths (build output), unless --all
       foil abort [--force] NAME
                               delete branch NAME; the workspace is untouched
                               (--force: also after an interrupted commit)
       foil list [--json]      list branches and their workspaces
       foil audit [--json] [NAME | FILE]
                               what a run did: hosts reached and blocked, changes,
                               and what became of them; no NAME: list the logs
       foil policy init [--force] | show
                               write a .foil/policy for this workspace (hiding the
                               secret-looking files found in it), or show the rules
       foil serve [--listen ADDR] [-w DIR...] [run options]
                               serve OpenSandbox's API (default 127.0.0.1:8080):
                               each sandbox is a branch of DIR (default: .) with
                               the run options; OPEN_SANDBOX_API_KEY is the key
                               clients must send (one is made if unset)
       foil doctor             check what works on this machine
       foil setup [--print]    allow foil to use user namespaces (Ubuntu; as root)
       foil vm setup [DIR...] | foil vm [foil args] | foil vm stop | delete
                               macOS: run Linux foil in a small Linux VM (Lima),
                               sharing only DIR..., for code you don't trust at all
       foil --version";

fn main() {
    // Inside a container on Ubuntu: get foil's AppArmor profile back if the
    // runtime started us without it (re-executes once; otherwise returns).
    #[cfg(target_os = "linux")]
    foil::container::regain_apparmor_profile();
    let mut args = std::env::args_os().skip(1);
    let Some(sub) = args.next() else { usage() };
    let rest: Vec<OsString> = args.collect();
    match sub.to_str() {
        Some("run") => run(rest),
        Some("agent") => agent(rest),
        Some("fanout") => fanout(rest),
        Some("session") => {
            let command = rest.strip_prefix(&[OsString::from("--")][..]).unwrap_or(&rest);
            if command.is_empty() {
                usage();
            }
            let err = foil::limits::session(command);
            eprintln!("foil: {}: {err}", command[0].to_string_lossy());
            exit(if err.kind() == ErrorKind::NotFound { 127 } else { 126 })
        }
        Some("pick") => {
            let (_, name) = flags_and_name(rest, &[]);
            let held = Branch::open(&name).map(|b| held_back(&b, false)).unwrap_or_default();
            finish(foil::fanout::pick(&name).map(|(n, gone)| {
                println!("applied {}; discarded {}", n_changes(n), if gone.is_empty() { "nothing".to_owned() } else { gone.join(", ") });
                tell_held_back(&held);
                note_commit(&name, n, &held, false);
                for other in &gone {
                    foil::audit::note(other, "abort", serde_json::json!({"by": "foil pick", "picked": name}));
                }
            }))
        }
        // Internal: PID 1 of a strict run, and its network proxy.
        #[cfg(target_os = "linux")]
        Some("__pid1") => foil::pid1::main(rest.strip_prefix(&[OsString::from("--")][..]).unwrap_or(&rest)),
        Some("__proxy") => foil::proxy::main(&rest),
        Some("__execd") => foil::execd::main(&rest),
        #[cfg(target_os = "macos")]
        Some("__trash") => {
            if let Some(dir) = rest.first() {
                foil::branch::empty_trash(std::path::Path::new(dir));
            }
        }
        #[cfg(target_os = "macos")]
        Some("__guard") => foil::procs::guard_main(&rest),
        #[cfg(target_os = "macos")]
        Some("vm") => finish(foil::vm::main(&rest)),
        Some("serve") => serve(rest),
        Some("diff") => diff(rest),
        Some("commit") => {
            let (flags, name) = flags_and_name(rest, &["--force", "--all", "--guarded"]);
            let force = flags.contains(&"--force");
            let all = flags.contains(&"--all");
            let guarded = flags.contains(&"--guarded");
            finish(Branch::open(&name).and_then(|b| {
                let held = if guarded { Vec::new() } else { held_back(&b, all) };
                let n = b.commit(force, all, guarded)?;
                println!("applied {}", n_changes(n));
                tell_held_back(&held);
                note_commit(&name, n, &held, force);
                Ok(())
            }))
        }
        Some("abort") => {
            let (flags, name) = flags_and_name(rest, &["--force"]);
            let force = flags.contains(&"--force");
            finish(Branch::open(&name).and_then(|b| b.abort(force)).map(|()| {
                foil::audit::note(&name, "abort", serde_json::json!({"by": "foil abort"}));
            }))
        }
        Some("list") => list(rest),
        Some("audit") => audit(rest),
        Some("policy") => policy(rest),
        Some("doctor") => match foil::doctor::doctor() {
            Ok(ok) => exit(if ok { 0 } else { 1 }),
            Err(err) => finish(Err(err)),
        },
        Some("setup") => match rest.iter().map(|a| a.to_str()).collect::<Vec<_>>()[..] {
            [] => finish(foil::doctor::setup(false)),
            [Some("--print")] => finish(foil::doctor::setup(true)),
            _ => usage(),
        },
        Some("--version" | "-V" | "version") => println!("foil {}", env!("CARGO_PKG_VERSION")),
        Some("--help" | "-h" | "help") => println!("{USAGE}"),
        _ => usage(),
    }
}

/// `foil serve`: its own options, then `foil run` options for every sandbox.
fn serve(args: Vec<OsString>) {
    let mut listen = "127.0.0.1:8080".to_owned();
    let mut workspaces = Vec::new();
    let mut run_flags = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--listen") => listen = args.next().and_then(|v| v.into_string().ok()).unwrap_or_else(|| usage()),
            Some("-w" | "--workspace") => {
                let dir = PathBuf::from(args.next().unwrap_or_else(|| usage()));
                workspaces.push(dir.canonicalize().unwrap_or_else(|err| {
                    eprintln!("foil: {}: {err}", dir.display());
                    exit(2)
                }));
            }
            _ => run_flags.push(arg),
        }
    }
    if workspaces.is_empty() {
        workspaces.push(std::env::current_dir().and_then(|d| d.canonicalize()).unwrap_or_else(|err| {
            eprintln!("foil: {err}");
            exit(2)
        }));
    }
    // The run options must parse as `foil run` options, on their own.
    let mut check = run_flags.clone();
    check.push("true".into());
    let spec = parse_run(check, RunSpec::default());
    if spec.command.len() != 1 || spec.branch_name.is_some() || spec.timeout.is_some() {
        eprintln!("foil: foil serve takes --listen, -w and `foil run` options except -b and --timeout");
        exit(2);
    }
    let from_env = std::env::var("OPEN_SANDBOX_API_KEY");
    // Sandboxes inherit foil's environment unless strict: the key must not be in it.
    std::env::remove_var("OPEN_SANDBOX_API_KEY");
    let api_key = match from_env {
        Ok(key) if !key.is_empty() => key,
        _ => {
            let key = foil::execd::random_hex(16);
            eprintln!("foil serve: API key (set OPEN_SANDBOX_API_KEY to choose one): {key}");
            key
        }
    };
    let config = foil::serve::Config {
        listen,
        workspaces,
        run_flags,
        strict: spec.options.strict,
        no_network: spec.options.no_network,
        api_key,
    };
    finish(foil::serve::main(config))
}

/// A parsed `foil run` (or `foil agent`) request.
#[derive(Default)]
struct RunSpec {
    branch_name: Option<String>,
    workspace: Option<PathBuf>,
    options: Options,
    limits: Limits,
    timeout: Option<std::time::Duration>,
    allow_host: Vec<String>,
    inject: Vec<String>,
    /// `--hide` patterns, relative to the workspace.
    hide: Vec<String>,
    audit: Option<PathBuf>,
    policy: Option<PathBuf>,
    /// The policy file the run follows, once read.
    policy_used: Option<PathBuf>,
    command: Vec<OsString>,
}

fn run(args: Vec<OsString>) {
    let spec = with_policy(args, RunSpec::default());
    if spec.command.is_empty() {
        usage();
    }
    exit_like(execute(&spec))
}

/// Read `foil run` options from `args` into `spec`; the first word that is
/// not an option starts the command.
fn parse_run(args: Vec<OsString>, mut spec: RunSpec) -> RunSpec {
    let mut args = args.into_iter().peekable();
    while let Some(first) = args.peek().and_then(|a| a.to_str()).map(str::to_owned) {
        if first == "--" {
            args.next();
            break;
        }
        let (flag, inline) = match first.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag.to_owned(), Some(OsString::from(value))),
            _ => (first.clone(), None),
        };
        let (takes_value, switch) = (run_flag_takes_value(&flag), run_flag_is_switch(&flag));
        if !takes_value && !switch {
            break;
        }
        args.next();
        if switch {
            if inline.is_some() {
                usage();
            }
            match flag.as_str() {
                "--protect" => spec.options.protect = true,
                "--strict" => spec.options.strict = true,
                _ => spec.options.no_network = true,
            }
            continue;
        }
        let value = inline.or_else(|| args.next()).unwrap_or_else(|| usage());
        let text = || value.to_str().map(str::to_owned).unwrap_or_else(|| bad(&flag, &value));
        let o = &mut spec.options;
        match flag.as_str() {
            "-b" | "--branch" => spec.branch_name = Some(text()),
            "-w" | "--workspace" => spec.workspace = Some(PathBuf::from(&value)),
            "--allow-write" => o.allow_write.push(PathBuf::from(&value)),
            "--allow-read" => o.allow_read.push(PathBuf::from(&value)),
            "--listen-socket" => o.listen.push(PathBuf::from(&value)),
            "--allow-host" => spec.allow_host.push(text()),
            "--allow-env" => o.allow_env.push(text()),
            "--inject-key" => spec.inject.push(text()),
            "--hide" => {
                let pattern = text();
                if let Err(why) = foil::policy::check_hide(&pattern) {
                    eprintln!("foil: --hide {pattern}: {why}");
                    exit(2)
                }
                spec.hide.push(pattern)
            }
            "--audit" => spec.audit = Some(PathBuf::from(&value)),
            "--policy" => spec.policy = Some(PathBuf::from(&value)),
            "--setenv" => match text().split_once('=') {
                Some((k, v)) if !k.is_empty() => o.setenv.push((k.to_owned(), v.to_owned())),
                _ => bad(&flag, &value),
            },
            "--timeout" => spec.timeout = Some(parse_duration(&text()).unwrap_or_else(|| bad(&flag, &value))),
            "--memory" => spec.limits.memory = Some(parse_size(&text()).unwrap_or_else(|| bad(&flag, &value))),
            "--cpus" => {
                let cpus = text().parse::<f64>().ok().filter(|c| c.is_finite() && *c > 0.0);
                spec.limits.cpus = Some(cpus.unwrap_or_else(|| bad(&flag, &value)));
            }
            _ => spec.limits.pids = Some(text().parse().ok().filter(|n| *n > 0).unwrap_or_else(|| bad(&flag, &value))),
        }
    }
    spec.command.extend(args);
    if spec.workspace.is_some() && spec.branch_name.is_none() {
        usage();
    }
    spec
}

/// `foil run` options that take a value.
fn run_flag_takes_value(flag: &str) -> bool {
    matches!(
        flag,
        "-b" | "--branch"
            | "-w"
            | "--workspace"
            | "--allow-write"
            | "--allow-read"
            | "--listen-socket"
            | "--allow-host"
            | "--allow-env"
            | "--inject-key"
            | "--hide"
            | "--audit"
            | "--policy"
            | "--setenv"
            | "--memory"
            | "--timeout"
            | "--cpus"
            | "--pids"
    )
}

/// `foil run` options that are on/off switches.
fn run_flag_is_switch(flag: &str) -> bool {
    matches!(flag, "--protect" | "--no-network" | "--strict")
}

/// Run `spec` to the end and return how its command terminated. Problems
/// setting the run up end foil with 125 (126/127 if the command can't run).
#[cfg(target_os = "linux")]
fn execute(spec: &RunSpec) -> Termination {
    let fail = |err: std::io::Error| -> ! {
        eprintln!("foil: {err}");
        exit(125)
    };
    let mut options = spec.options.clone();
    if (!spec.allow_host.is_empty() || !spec.inject.is_empty()) && !options.strict {
        fail(std::io::Error::other("--allow-host and --inject-key need --strict"));
    }
    if options.strict {
        // The view shows each socket's folder from the host, so a socket
        // made inside is reachable from outside.
        for socket in &options.listen {
            options.allow_write.push(socket.parent().map(PathBuf::from).unwrap_or_default());
        }
    }
    if let Err(err) = foil::proxy::check_inject(&spec.inject) {
        fail(err)
    }
    if let Some(bad) = spec.allow_host.iter().find(|h| !foil::proxy::valid_allow_entry(h)) {
        fail(std::io::Error::other(format!("bad --allow-host {bad}: use host, host:port, *.domain or *")));
    }

    // With limits, this re-runs foil (with its original arguments) in a
    // limited systemd scope and does not return; inside that scope it
    // checks the limits and returns.
    let original: Vec<OsString> = std::env::args_os().skip(1).collect();
    let isolated = spec.branch_name.is_some() || options.protect || options.no_network || options.strict;
    if let Err(err) = spec.limits.enforce(&original, isolated) {
        fail(err)
    }

    let (audit, audit_path) = prepare(spec, &mut options).unwrap_or_else(|err| fail(err));
    let started = std::time::Instant::now();

    // The network proxy, outside the sandbox, for an allowlist or injected keys.
    let proxy = if options.strict && (!spec.allow_host.is_empty() || !spec.inject.is_empty()) {
        match foil::proxy::start(&spec.allow_host, &spec.inject, audit_path.as_deref()) {
            Ok(running) => {
                options.proxy_dir = running.dir.clone();
                let socket = format!("{}:{}/proxy.sock", isolate::PROXY_PORT, foil::strict::PROXY_DIR);
                options.setenv.push(("FOIL_RELAY".to_owned(), socket));
                options.setenv.extend(foil::proxy::sandbox_env(isolate::PROXY_PORT, &spec.inject, None));
                Some(running)
            }
            Err(err) => fail(err),
        }
    } else {
        None
    };
    if options.strict {
        options.setenv.push(("FOIL_STATUS_FD".to_owned(), foil::strict::STATUS_FD.to_string()));
        // For PID 1's grace period; it removes the variable before the command starts.
        if let Ok(grace) = std::env::var("FOIL_GRACE_MS") {
            options.setenv.push(("FOIL_GRACE_MS".to_owned(), grace));
        }
    }

    // Keep the branch and its lock alive until the command and everything
    // it started are gone.
    let mut held = None;
    let ready = (|| {
        let overlay = match &spec.branch_name {
            Some(name) => {
                let workspace = spec.workspace.clone().unwrap_or_else(|| PathBuf::from("."));
                let branch = Branch::open_or_create(name, &workspace)?;
                let lock = branch.lock()?;
                if let Some(dir) = foil::cgroup::box_path().filter(|_| foil::cgroup::can_box()) {
                    branch.record_box(&dir)?;
                }
                let overlay = branch.overlay()?;
                held = Some((branch, lock));
                Some(overlay)
            }
            None => None,
        };
        isolate::Plan::new(&options, overlay)
    })();
    let plan = match ready {
        Ok(plan) => plan,
        Err(err) => fail(err),
    };

    let result = supervise_for(&spec.command, plan.as_ref(), spec.timeout);
    drop(held);
    if let Some(proxy) = proxy {
        proxy.stop();
    }
    record_end(audit.as_ref(), &result, started);
    report(spec, result)
}

/// How a run ended, as foil reports it: the command's own status, or 124
/// after `--timeout`; foil exits right here if the command never started.
fn report(spec: &RunSpec, result: std::io::Result<foil::supervisor::Outcome>) -> Termination {
    match result {
        Ok(outcome) => {
            if spec.limits.memory.is_some()
                && outcome.termination == Termination::Signaled(libc::SIGKILL)
                && foil::limits::oom_killed()
            {
                eprintln!(
                    "foil: {}: killed for using more memory than --memory allows",
                    spec.command[0].to_string_lossy()
                );
            }
            if outcome.timed_out {
                eprintln!(
                    "foil: {}: stopped after --timeout {:.1} s",
                    spec.command[0].to_string_lossy(),
                    spec.timeout.unwrap_or_default().as_secs_f64()
                );
                return Termination::Exited(TIMED_OUT);
            }
            outcome.termination
        }
        Err(err) => {
            eprintln!("foil: {}: {err}", spec.command[0].to_string_lossy());
            // Same conventions as POSIX shells and `docker run`.
            exit(match err.kind() {
                ErrorKind::NotFound => 127,
                ErrorKind::PermissionDenied => 126,
                _ => 125,
            })
        }
    }
}

/// macOS: the same as above, with an APFS-clone branch and a Seatbelt
/// profile (see `src/mac/`). Limits never re-run foil here; the proxy
/// listens on a loopback port with a per-run token.
#[cfg(target_os = "macos")]
fn execute(spec: &RunSpec) -> Termination {
    let fail = |err: std::io::Error| -> ! {
        eprintln!("foil: {err}");
        exit(125)
    };
    let mut options = spec.options.clone();
    if (!spec.allow_host.is_empty() || !spec.inject.is_empty()) && !options.strict {
        fail(std::io::Error::other("--allow-host and --inject-key need --strict"));
    }
    if let Err(err) = foil::proxy::check_inject(&spec.inject) {
        fail(err)
    }
    if let Some(bad) = spec.allow_host.iter().find(|h| !foil::proxy::valid_allow_entry(h)) {
        fail(std::io::Error::other(format!("bad --allow-host {bad}: use host, host:port, *.domain or *")));
    }
    if let Err(err) = spec.limits.enforce(&[], false) {
        fail(err)
    }
    let (audit, audit_path) = prepare(spec, &mut options).unwrap_or_else(|err| fail(err));
    let started = std::time::Instant::now();
    let proxy = if options.strict && (!spec.allow_host.is_empty() || !spec.inject.is_empty()) {
        match foil::proxy::start(&spec.allow_host, &spec.inject, audit_path.as_deref()) {
            Ok(running) => {
                let port = running.port.unwrap_or_default();
                options.proxy_port = Some(port);
                options.setenv.extend(foil::proxy::sandbox_env(port, &spec.inject, running.token.as_deref()));
                Some(running)
            }
            Err(err) => fail(err),
        }
    } else {
        None
    };
    let mut held = None;
    let ready = (|| {
        let overlay = match &spec.branch_name {
            Some(name) => {
                let workspace = spec.workspace.clone().unwrap_or_else(|| PathBuf::from("."));
                let branch = Branch::open_or_create(name, &workspace)?;
                let lock = branch.lock()?;
                let overlay = branch.overlay()?;
                held = Some((branch, lock));
                Some(overlay)
            }
            None => None,
        };
        let mut plan = isolate::Plan::new(&options, overlay)?;
        if let Some(p) = &mut plan {
            p.memory = spec.limits.memory;
            if let Some((branch, _)) = &held {
                branch.record_box(&p.marker.denied)?;
            }
        }
        Ok::<_, std::io::Error>(plan)
    })();
    let plan = match ready {
        Ok(plan) => plan,
        Err(err) => fail(err),
    };
    let result = supervise_for(&spec.command, plan.as_ref(), spec.timeout);
    drop(held);
    if let Some(proxy) = proxy {
        proxy.stop();
    }
    record_end(audit.as_ref(), &result, started);
    report(spec, result)
}

/// `foil agent NAME [run options] [-- agent arguments]`.
fn agent(args: Vec<OsString>) {
    use foil::agent;
    let mut args = args.into_iter();
    let name = args.next().and_then(|n| n.into_string().ok()).unwrap_or_else(|| usage());
    let fail = |err: std::io::Error| -> ! {
        eprintln!("foil: {err}");
        exit(125)
    };
    let profile = agent::profile(&name).unwrap_or_else(|err| fail(err));
    let Some(program) = agent::find_program(profile.program) else {
        eprintln!("foil: {} not found on PATH", profile.program);
        exit(127)
    };
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| fail(std::io::Error::other("HOME is not set")));

    let mut spec = RunSpec::default();
    spec.options.strict = true;
    spec.options.allow_write = agent::state_paths(profile, &home);
    spec.options.allow_read = agent::install_dirs(&program, &home);
    spec.allow_host = profile.hosts.iter().map(|h| h.to_string()).collect();
    if foil::proxy::has_key(profile.inject) {
        spec.inject.push(profile.inject.to_owned());
    } else if let Some(var) = foil::proxy::key_env(profile.inject) {
        eprintln!(
            "foil: {var} is not set, so {name} uses its own login from its state folder, which \
             the commands it runs can read too. Set {var} and foil's proxy holds the key instead."
        );
    }
    spec.options.setenv = profile.env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    // The user's own options come next; whatever follows goes to the agent.
    let mut spec = with_policy(args.collect(), spec);
    let agent_args = std::mem::take(&mut spec.command);
    spec.command = std::iter::once(program.into_os_string()).chain(agent_args).collect();
    let branch = spec.branch_name.get_or_insert_with(|| agent::branch_name(&name)).clone();
    if spec.policy_used.is_none() && spec.hide.is_empty() {
        let here = std::env::current_dir().unwrap_or_default();
        let secrets = foil::policy::find_secrets(&here);
        if !secrets.is_empty() {
            let shown: Vec<String> = secrets.iter().take(5).map(|p| p.display().to_string()).collect();
            eprintln!(
                "foil: {name} can read files here that look secret ({}{}): run `foil policy init` to hide them",
                shown.join(", "),
                if secrets.len() > 5 { ", ..." } else { "" }
            );
        }
    }

    let termination = execute(&spec);
    summarize(&branch, termination, &spec);
    exit_like(termination)
}

/// `foil fanout -n N [--check CMD] [--agent NAME] [--name BASE] [--pick] [run options] [--] ...`.
fn fanout(args: Vec<OsString>) {
    use foil::fanout::{self, Plan};
    // All attempts start at once: give them one shared scope (see `foil
    // session`), so they don't each wait for systemd to make their own.
    #[cfg(target_os = "linux")]
    if std::env::var_os("FOIL_FANOUT_SESSION").is_none() && !foil::cgroup::can_box() {
        std::env::set_var("FOIL_FANOUT_SESSION", "1");
        let me = std::env::current_exe().map(OsString::from).unwrap_or_else(|_| "foil".into());
        let command: Vec<OsString> = [me, "fanout".into()].into_iter().chain(args.iter().cloned()).collect();
        let err = foil::limits::session(&command);
        eprintln!("foil: fanout: {err}");
        exit(125)
    }
    let mut args = args.into_iter().peekable();
    let (mut attempts, mut check, mut agent, mut base, mut pick, mut json) = (None, None, None, None, false, false);
    // Fanout's own flags may come anywhere among the `foil run` options;
    // those are set aside (with their values) for every attempt.
    let mut rest: Vec<OsString> = Vec::new();
    while let Some(flag) = args.peek().and_then(|a| a.to_str()).map(str::to_owned) {
        if flag == "--pick" || flag == "--json" {
            args.next();
            if flag == "--pick" {
                pick = true;
            } else {
                json = true;
            }
            continue;
        }
        let name = flag.split_once('=').filter(|_| flag.starts_with("--")).map_or(flag.as_str(), |(f, _)| f);
        if !matches!(flag.as_str(), "-n" | "--check" | "--agent" | "--name") {
            if !run_flag_takes_value(name) && !run_flag_is_switch(name) {
                break;
            }
            rest.extend(args.next());
            if run_flag_takes_value(name) && !flag.contains('=') {
                rest.extend(args.next());
            }
            continue;
        }
        args.next();
        let value = args.next().unwrap_or_else(|| usage());
        let text = value.to_str().map(str::to_owned).unwrap_or_else(|| bad(&flag, &value));
        match flag.as_str() {
            "-n" => attempts = Some(text.parse::<usize>().ok().filter(|n| (1..=64).contains(n)).unwrap_or_else(|| bad(&flag, &value))),
            "--check" => check = Some(text),
            "--agent" => agent = Some(foil::agent::profile(&text).map(|p| p.name.to_owned()).unwrap_or_else(|err| {
                eprintln!("foil: {err}");
                exit(2)
            })),
            _ => base = Some(text),
        }
    }
    let Some(attempts) = attempts else { usage() };

    // The rest is `foil run` options, then the command: parse them the way
    // `foil run` will, to reject mistakes now and to split the two.
    rest.extend(args);
    let spec = parse_run(rest.clone(), RunSpec::default());
    if spec.branch_name.is_some() || spec.workspace.is_some() {
        eprintln!("foil: fanout names the branches itself; leave out -b and -w");
        exit(2)
    }
    if spec.command.is_empty() && agent.is_none() {
        usage();
    }
    let mut run_options = rest[..rest.len() - spec.command.len()].to_vec();
    if run_options.last().is_some_and(|a| a == "--") {
        run_options.pop();
    }
    let base = base.unwrap_or_else(|| foil::agent::branch_name(agent.as_deref().unwrap_or("fanout")));
    let plan = Plan { base, attempts, strict: spec.options.strict || agent.is_some(), agent, check, run_options, command: spec.command };

    let started = std::time::Instant::now();
    let results = fanout::run(&plan).unwrap_or_else(|err| {
        eprintln!("foil: {err}");
        exit(125)
    });
    eprintln!("foil: {} attempts of {} in {:.1} s", plan.attempts, plan.base, started.elapsed().as_secs_f64());
    eprintln!("  {:<width$}  status   check  changes  time", "attempt", width = plan.base.len() + 3);
    let status_of = |a: &fanout::Attempt| match a.status {
        Some(TIMED_OUT) if spec.timeout.is_some() => "timeout".to_owned(),
        Some(code) => code.to_string(),
        None => "signal".to_owned(),
    };
    for a in &results {
        let check = match a.check {
            Some(true) => "pass",
            Some(false) => "fail",
            None => "-",
        };
        let status = status_of(a);
        eprintln!("  {:<width$}  {status:<7}  {check:<5}  {:<7}  {:.1} s", a.branch, a.changes, a.seconds, width = plan.base.len() + 3);
    }
    eprintln!("logs: {}", results[0].log.parent().map(|p| p.display().to_string()).unwrap_or_default());
    let best = fanout::best(&results);
    let picked = match best.filter(|_| pick) {
        Some(i) => match fanout::pick(&results[i].branch) {
            Ok((n, gone)) => {
                eprintln!("foil: picked {}: applied {}; discarded {} other attempts", results[i].branch, n_changes(n), gone.len());
                true
            }
            Err(err) => {
                eprintln!("foil: picking {}: {err}", results[i].branch);
                false
            }
        },
        None => false,
    };
    if json {
        let items: Vec<String> = results
            .iter()
            .map(|a| {
                format!(
                    "{{\"branch\":{},\"status\":{},\"timed_out\":{},\"check\":{},\"changes\":{},\"seconds\":{:.3},\"log\":{}}}",
                    json_string(&a.branch),
                    a.status.map_or("null".to_owned(), |c| c.to_string()),
                    spec.timeout.is_some() && a.status == Some(TIMED_OUT),
                    a.check.map_or("null".to_owned(), |c| c.to_string()),
                    a.changes,
                    a.seconds,
                    json_string(&a.log.to_string_lossy())
                )
            })
            .collect();
        let best_name = best.map_or("null".to_owned(), |i| json_string(&results[i].branch));
        println!("{{\"base\":{},\"best\":{best_name},\"picked\":{picked},\"attempts\":[{}]}}", json_string(&plan.base), items.join(","));
    }
    match best {
        None => {
            eprintln!("foil: no attempt passed; branches kept for review (foil diff NAME, foil abort NAME)");
            exit(1)
        }
        Some(_) if pick && !picked => exit(1),
        Some(i) if !pick => {
            let name = &results[i].branch;
            eprintln!("best: {name}\nreview: foil diff --patch {name}\nkeep it: foil pick {name} (commits it, discards the others)");
        }
        Some(_) => {}
    }
}

/// After an agent session: what the branch changed, and what to do with it.
fn summarize(name: &str, termination: Termination, spec: &RunSpec) {
    let how = match termination {
        Termination::Exited(code) => format!("exited with status {code}"),
        Termination::Signaled(sig) => format!("was stopped by signal {sig}"),
    };
    eprintln!("\nfoil: the agent {how}.");
    let log_path = spec.audit.clone().or_else(|| foil::audit::path_for(name).ok());
    if let Some(text) = log_path.as_deref().and_then(|p| std::fs::read_to_string(p).ok()) {
        let s = foil::audit::summarize(&text);
        eprint!("foil: network: {} allowed", plural(s.allowed_total(), "request"));
        if s.blocked_total() == 0 {
            eprintln!(", none blocked");
        } else {
            eprintln!(", {} blocked:", s.blocked_total());
            for (target, why, n) in s.blocked.iter().take(8) {
                eprintln!("  {target} ({why}) x{n}");
            }
        }
    }
    let Ok(branch) = Branch::open(name) else { return };
    let (changes, _) = branch.changes(false).unwrap_or_default();
    let guarded: Vec<String> = changes.iter().filter(|c| c.guarded()).map(|c| c.path.display().to_string()).collect();
    let paths: Vec<String> = changes.iter().take(200).map(|c| format!("{} {}", c.kind.letter(), c.path.display())).collect();
    foil::audit::note(name, "review", serde_json::json!({"changes": changes.len(), "paths": paths, "guarded": guarded}));
    if changes.is_empty() {
        eprintln!("foil: no changes in branch {name}; removing it.");
        let _ = branch.abort(false);
        foil::audit::note(name, "abort", serde_json::json!({"by": "foil agent", "why": "no changes"}));
        return;
    }
    eprintln!("foil: branch {name} changed {} paths in {}:", changes.len(), branch.workspace.display());
    for change in changes.iter().take(15) {
        eprintln!("  {} {}{}", change.kind.letter(), change.path.display(), guard_note(change));
    }
    if changes.len() > 15 {
        eprintln!("  ... and {} more", changes.len() - 15);
    }
    // SAFETY: isatty only inspects the descriptor.
    let interactive = unsafe { libc::isatty(0) == 1 && libc::isatty(2) == 1 };
    if !interactive {
        eprintln!("review: foil diff --patch {name}\ncommit: foil commit {name}\ndiscard: foil abort {name}");
        return;
    }
    eprint!("Apply to {}? [c]ommit / [k]eep for later / [a]bort (default: keep): ", branch.workspace.display());
    let mut answer = String::new();
    let _ = std::io::stdin().read_line(&mut answer);
    match answer.trim() {
        "c" | "commit" => match branch.commit(false, false, false) {
            Ok(n) => {
                eprintln!("foil: applied {}", n_changes(n));
                let held: Vec<PathBuf> = changes.iter().filter(|c| c.guarded()).map(|c| c.path.clone()).collect();
                tell_held_back(&held);
                note_commit(name, n, &held, false);
            }
            Err(err) => eprintln!("foil: {err}"),
        },
        "a" | "abort" => match branch.abort(false) {
            Ok(()) => {
                eprintln!("foil: branch {name} discarded");
                foil::audit::note(name, "abort", serde_json::json!({"by": "foil agent"}));
            }
            Err(err) => eprintln!("foil: {err}"),
        },
        _ => eprintln!("foil: kept branch {name}; review with `foil diff --patch {name}`"),
    }
    eprintln!("record: foil audit {name}");
}

fn diff(args: Vec<OsString>) {
    let (mut flags, name) = flags_and_name(args, &["--patch", "--json", "--all"]);
    let all = flags.contains(&"--all");
    flags.retain(|f| *f != "--all");
    if flags.len() > 1 {
        usage();
    }
    let branch = Branch::open(&name);
    let mut out = std::io::stdout().lock();
    finish(branch.and_then(|branch| {
        let (changes, hidden) = branch.changes(all)?;
        if hidden > 0 {
            eprintln!("foil: {hidden} paths ignored by .gitignore not shown (build output etc.); --all shows them");
        }
        match flags.first() {
            Some(&"--patch") => branch.write_patch(&mut out, all),
            Some(_) => {
                let items: Vec<String> = changes
                    .iter()
                    .map(|c| {
                        format!(
                            "{{\"kind\":\"{}\",\"path\":{},\"guarded\":{}}}",
                            c.kind.letter(),
                            json_string(&c.path.to_string_lossy()),
                            c.guarded()
                        )
                    })
                    .collect();
                writeln!(out, "[{}]", items.join(","))
            }
            None => {
                for change in &changes {
                    writeln!(out, "{} {}{}", change.kind.letter(), change.path.display(), guard_note(change))?;
                }
                if changes.iter().any(|c| c.guarded()) {
                    eprintln!("foil: guarded paths run on your machine outside foil (git config or hooks, a new .git, foil's policy, Claude Code or MCP settings, editor tasks); commit leaves them out unless --guarded");
                }
                Ok(())
            }
        }
    }))
}

fn list(args: Vec<OsString>) {
    let json = match args.iter().map(|a| a.to_str()).collect::<Vec<_>>()[..] {
        [] => false,
        [Some("--json")] => true,
        _ => usage(),
    };
    finish(Branch::list().map(|branches| {
        if json {
            let items: Vec<String> = branches
                .iter()
                .map(|b| {
                    format!(
                        "{{\"name\":{},\"workspace\":{}}}",
                        json_string(&b.name),
                        json_string(&b.workspace.to_string_lossy())
                    )
                })
                .collect();
            println!("[{}]", items.join(","));
        } else {
            for branch in branches {
                println!("{}\t{}", branch.name, branch.workspace.display());
            }
        }
    }))
}

/// Split `args` into allowed flags and the single branch-name argument.
fn flags_and_name(args: Vec<OsString>, allowed: &[&'static str]) -> (Vec<&'static str>, String) {
    let mut flags = Vec::new();
    let mut name = None;
    for arg in args {
        match arg.into_string() {
            Ok(arg) if arg.starts_with('-') => match allowed.iter().find(|f| **f == arg) {
                Some(flag) => flags.push(*flag),
                None => usage(),
            },
            Ok(arg) if name.is_none() => name = Some(arg),
            _ => usage(),
        }
    }
    (flags, name.unwrap_or_else(|| usage()))
}

/// "1 change", "3 changes".
/// The guarded changes (see `Change::guarded`) a commit of `branch` leaves out.
fn held_back(branch: &Branch, all: bool) -> Vec<std::path::PathBuf> {
    let changes = branch.changes(all).map(|(c, _)| c).unwrap_or_default();
    changes.into_iter().filter(|c| c.guarded()).map(|c| c.path).collect()
}

fn tell_held_back(paths: &[std::path::PathBuf]) {
    if !paths.is_empty() {
        let list: Vec<String> = paths.iter().map(|p| format!("  {}", p.display())).collect();
        eprintln!(
            "foil: left out {} that would run on your machine outside foil (git config or hooks, a new .git, \
             foil's policy, Claude Code or MCP settings, editor tasks), discarded with the branch:\n{}\n\
             to apply such changes, review them first, then commit with --guarded",
            if paths.len() == 1 { "1 path".to_owned() } else { format!("{} paths", paths.len()) },
            list.join("\n")
        );
    }
}

/// How `foil diff` marks a guarded change.
fn guard_note(change: &foil::change::Change) -> &'static str {
    if change.guarded() { "  [guarded]" } else { "" }
}

/// "1 request", "3 requests".
fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

/// Parse `foil run` options from `cli` on top of `base`, with the policy
/// they name (`--policy`), or for a strict run the workspace's own, first:
/// the command line adds to a policy, it can't take a rule away.
fn with_policy(cli: Vec<OsString>, base: RunSpec) -> RunSpec {
    let probe = parse_run(cli.clone(), RunSpec { options: base.options.clone(), ..RunSpec::default() });
    let workspace = probe.workspace.clone().unwrap_or_else(|| PathBuf::from("."));
    let workspace = workspace.canonicalize().unwrap_or(workspace);
    let path = match (&probe.policy, probe.options.strict || base.options.strict) {
        (Some(path), _) => Some(path.clone()),
        (None, true) => Some(workspace.join(foil::policy::FILE)),
        (None, false) => None,
    };
    let Some(path) = path else { return parse_run(cli, base) };
    let policy = match foil::policy::load(&path, &workspace) {
        Ok(Some(policy)) => policy,
        Ok(None) if probe.policy.is_none() => return parse_run(cli, base),
        Ok(None) => {
            eprintln!("foil: --policy {}: no such file", path.display());
            exit(2)
        }
        Err(err) => {
            eprintln!("foil: {err}");
            exit(2)
        }
    };
    // Once: not again when foil re-runs itself in a systemd scope for limits.
    if std::env::var_os("FOIL_SCOPED").is_none() {
        eprintln!("foil: following {} ({})", path.display(), plural(policy.rules, "rule"));
    }
    let mut spec = parse_run(policy.args.clone(), base);
    if !spec.command.is_empty() {
        eprintln!("foil: {}: a policy holds rules, not a command", path.display());
        exit(2)
    }
    spec.policy_used = Some(path);
    parse_run(cli, spec)
}

/// Before a run starts: find the paths `--hide` names, and open the audit
/// log (`--audit`, or the branch's own) and record how the run is set up.
fn prepare(spec: &RunSpec, options: &mut Options) -> std::io::Result<(Option<foil::audit::Log>, Option<PathBuf>)> {
    let workspace = spec.workspace.clone().unwrap_or_else(|| PathBuf::from("."));
    let workspace = workspace.canonicalize()?;
    let mut hidden = Vec::new();
    for pattern in &spec.hide {
        for rel in foil::policy::expand(&workspace, pattern) {
            if !hidden.contains(&rel) {
                hidden.push(rel);
            }
        }
    }
    options.hide = hidden.iter().map(|rel| workspace.join(rel)).collect();
    let path = match (&spec.audit, &spec.branch_name) {
        (Some(path), _) => Some(std::path::absolute(path)?),
        (None, Some(name)) => Some(foil::audit::path_for(name)?),
        (None, None) => None,
    };
    let Some(path) = path else { return Ok((None, None)) };
    let log = foil::audit::Log::open(&path).map_err(|err| std::io::Error::new(err.kind(), format!("audit log {}: {err}", path.display())))?;
    let strings = |paths: &[PathBuf]| paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>();
    log.event(
        "start",
        serde_json::json!({
            "foil": env!("CARGO_PKG_VERSION"),
            "command": spec.command.iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>(),
            "cwd": std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default(),
            "workspace": workspace.display().to_string(),
            "branch": spec.branch_name,
            "user": std::env::var("USER").unwrap_or_default(),
            "pid": std::process::id(),
            "strict": options.strict,
            "protect": options.protect,
            "no_network": options.no_network,
            "allow_host": spec.allow_host,
            "inject_key": spec.inject,
            "allow_read": strings(&options.allow_read),
            "allow_write": strings(&options.allow_write),
            "allow_env": options.allow_env,
            "setenv": options.setenv.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
            "hide": strings(&hidden),
            "policy": spec.policy_used.as_ref().map(|p| p.display().to_string()),
            "timeout_s": spec.timeout.map(|t| t.as_secs_f64()),
            "memory": spec.limits.memory,
            "cpus": spec.limits.cpus,
            "pids": spec.limits.pids,
        }),
    );
    Ok((Some(log), Some(path)))
}

/// After a run: how it ended, in its audit log.
fn record_end(log: Option<&foil::audit::Log>, result: &std::io::Result<foil::supervisor::Outcome>, started: std::time::Instant) {
    let Some(log) = log else { return };
    let seconds = (started.elapsed().as_secs_f64() * 1000.0).round() / 1000.0;
    let fields = match result {
        Ok(o) => match o.termination {
            Termination::Exited(code) => serde_json::json!({"status": code, "timed_out": o.timed_out, "seconds": seconds}),
            Termination::Signaled(sig) => serde_json::json!({"signal": sig, "timed_out": o.timed_out, "seconds": seconds}),
        },
        Err(err) => serde_json::json!({"error": err.to_string(), "seconds": seconds}),
    };
    log.event("end", fields);
}

fn note_commit(name: &str, applied: usize, held: &[PathBuf], force: bool) {
    let held: Vec<String> = held.iter().map(|p| p.display().to_string()).collect();
    foil::audit::note(name, "commit", serde_json::json!({"applied": applied, "held_back": held, "force": force}));
}

/// `foil audit [--json] [NAME | FILE]`.
fn audit(args: Vec<OsString>) {
    let mut json = false;
    let mut target = None;
    for arg in args {
        match arg.to_str() {
            Some("--json") => json = true,
            Some(a) if a.starts_with('-') => usage(),
            _ if target.is_none() => target = Some(PathBuf::from(arg)),
            _ => usage(),
        }
    }
    let Some(target) = target else {
        // List the logs, newest first.
        let dir = foil::branch::home().map(|h| h.join("audit")).unwrap_or_default();
        let mut logs: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&dir)
            .map(|d| d.flatten().filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path()))).collect())
            .unwrap_or_default();
        logs.retain(|(_, p)| p.extension().is_some_and(|e| e == "jsonl"));
        logs.sort_by_key(|l| std::cmp::Reverse(l.0));
        if logs.is_empty() {
            eprintln!("foil: no audit logs yet in {}", dir.display());
        }
        for (_, path) in logs {
            let s = foil::audit::summarize(&std::fs::read_to_string(&path).unwrap_or_default());
            let name = path.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let when = s.start.as_ref().and_then(|e| e.get("ts")).and_then(|t| t.as_str()).unwrap_or("?").to_owned();
            let outcome = s.outcomes.last().and_then(|e| e.get("event")).and_then(|e| e.as_str()).unwrap_or("open");
            println!("{name}	{when}	{} allowed	{} blocked	{outcome}", s.allowed_total(), s.blocked_total());
        }
        return;
    };
    let path = if target.is_file() {
        target
    } else {
        foil::audit::path_for(&target.to_string_lossy()).unwrap_or_else(|err| {
            eprintln!("foil: {err}");
            exit(1)
        })
    };
    let text = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        eprintln!("foil: audit log {}: {err}", path.display());
        exit(1)
    });
    let s = foil::audit::summarize(&text);
    if json {
        let allowed: Vec<_> = s.allowed.iter().map(|(t, n)| serde_json::json!({"target": t, "requests": n})).collect();
        let blocked: Vec<_> = s.blocked.iter().map(|(t, w, n)| serde_json::json!({"target": t, "reason": w, "requests": n})).collect();
        println!(
            "{}",
            serde_json::json!({"log": path.display().to_string(), "start": s.start, "end": s.end,
                               "allowed": allowed, "blocked": blocked, "outcomes": s.outcomes, "bad_lines": s.bad_lines})
        );
        return;
    }
    let get = |e: &Option<serde_json::Value>, k: &str| e.as_ref().and_then(|e| e.get(k)).cloned().unwrap_or_default();
    let list = |v: serde_json::Value| -> String {
        let items: Vec<String> = v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).unwrap_or_default();
        if items.is_empty() { "none".to_owned() } else { items.join(", ") }
    };
    println!("audit log {}", path.display());
    let mut command = list(get(&s.start, "command")).replace(", ", " ");
    if command.chars().count() > 120 {
        command = format!("{}... (--json shows it all)", command.chars().take(120).collect::<String>());
    }
    println!("  command    {command}");
    println!("  workspace  {}", get(&s.start, "workspace").as_str().unwrap_or("?"));
    println!("  started    {}", get(&s.start, "ts").as_str().unwrap_or("?"));
    match &s.end {
        Some(end) => {
            let how = match (end.get("status"), end.get("signal")) {
                (Some(c), _) => format!("exit {c}"),
                (_, Some(sig)) => format!("signal {sig}"),
                _ => format!("error: {}", end.get("error").and_then(|e| e.as_str()).unwrap_or("?")),
            };
            let timed_out = if end.get("timed_out").and_then(|t| t.as_bool()) == Some(true) { ", timed out" } else { "" };
            println!("  ended      {} ({how}{timed_out}, {} s)", end.get("ts").and_then(|t| t.as_str()).unwrap_or("?"), end.get("seconds").cloned().unwrap_or_default());
        }
        None => println!("  ended      no end recorded (still running, or foil was killed)"),
    }
    let mode = if get(&s.start, "strict").as_bool() == Some(true) { "strict" } else { "not strict" };
    let policy = get(&s.start, "policy");
    println!("  sandbox    {mode}; policy: {}", policy.as_str().unwrap_or("none"));
    println!("  hosts      {}", list(get(&s.start, "allow_host")));
    println!("  hidden     {}", list(get(&s.start, "hide")));
    println!("network: {} allowed, {} blocked", plural(s.allowed_total(), "request"), s.blocked_total());
    for (target, n) in &s.allowed {
        println!("  allowed  {target:<40} x{n}");
    }
    for (target, why, n) in &s.blocked {
        println!("  BLOCKED  {target:<40} x{n}  {why}");
    }
    for ev in &s.outcomes {
        let kind = ev.get("event").and_then(|e| e.as_str()).unwrap_or("?");
        let ts = ev.get("ts").and_then(|t| t.as_str()).unwrap_or("?");
        match kind {
            "review" => {
                println!("changes: {} (at {ts})", ev.get("changes").cloned().unwrap_or_default());
                for p in ev.get("paths").and_then(|p| p.as_array()).into_iter().flatten().take(20) {
                    println!("  {}", p.as_str().unwrap_or(""));
                }
                let guarded = list(ev.get("guarded").cloned().unwrap_or_default());
                if guarded != "none" {
                    println!("  guarded (left out of commits unless --guarded): {guarded}");
                }
            }
            "commit" => {
                let held = list(ev.get("held_back").cloned().unwrap_or_default());
                println!("committed {} at {ts}; held back: {held}", ev.get("applied").cloned().unwrap_or_default());
            }
            _ => println!("discarded at {ts} (by {})", ev.get("by").and_then(|b| b.as_str()).unwrap_or("?")),
        }
    }
    if s.bad_lines > 0 {
        println!("warning: {} lines of this log are not JSON (edited or torn)", s.bad_lines);
    }
}

/// `foil policy init [--force] | show`.
fn policy(args: Vec<OsString>) {
    let args: Vec<String> = args.into_iter().map(|a| a.into_string().unwrap_or_else(|_| usage())).collect();
    let workspace = std::env::current_dir().and_then(|d| d.canonicalize()).unwrap_or_else(|err| {
        eprintln!("foil: {err}");
        exit(1)
    });
    let path = workspace.join(foil::policy::FILE);
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["init"] | ["init", "--force"] => {
            if path.exists() && args.len() == 1 {
                eprintln!("foil: {} exists; --force replaces it", path.display());
                exit(1)
            }
            let secrets = foil::policy::find_secrets(&workspace);
            let text = foil::policy::template(&secrets);
            finish(std::fs::create_dir_all(workspace.join(".foil")).and_then(|()| std::fs::write(&path, text)));
            println!("wrote {}", path.display());
            if secrets.is_empty() {
                println!("no secret-looking files found; add `hide PATH` lines for any the agent must not read");
            } else {
                println!("hidden from strict runs ({}):", secrets.len());
                for s in &secrets {
                    println!("  {}", s.display());
                }
            }
            println!("review it, then commit it with your code: every foil agent run here follows it");
        }
        ["show"] => {
            let policy = match foil::policy::load(&path, &workspace) {
                Ok(Some(policy)) => policy,
                Ok(None) => {
                    eprintln!("foil: no {} here; `foil policy init` writes one", foil::policy::FILE);
                    exit(1)
                }
                Err(err) => {
                    eprintln!("foil: {err}");
                    exit(1)
                }
            };
            println!("{} ({}):", path.display(), plural(policy.rules, "rule"));
            let mut args = policy.args.iter().peekable();
            while let Some(flag) = args.next() {
                let flag = flag.to_string_lossy();
                let value = args.next_if(|a| !a.to_string_lossy().starts_with("--")).map(|v| v.to_string_lossy().into_owned());
                match (flag.as_ref(), value) {
                    ("--hide", Some(v)) => {
                        let found = foil::policy::expand(&workspace, &v);
                        let shown: Vec<String> = found.iter().map(|p| p.display().to_string()).collect();
                        println!("  hide {v}  -> {}", if shown.is_empty() { "nothing here now".to_owned() } else { shown.join(", ") });
                    }
                    (f, Some(v)) => println!("  {} {v}", f.trim_start_matches("--")),
                    (f, None) => println!("  {}", f.trim_start_matches("--")),
                }
            }
        }
        _ => usage(),
    }
}

fn n_changes(n: usize) -> String {
    format!("{n} change{}", if n == 1 { "" } else { "s" })
}

/// `text` as a JSON string literal.
fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn finish(result: std::io::Result<()>) {
    if let Err(err) = result {
        eprintln!("foil: {err}");
        exit(1);
    }
}

fn bad(flag: &str, value: &OsString) -> ! {
    eprintln!("foil: bad value for {flag}: {}", value.to_string_lossy());
    exit(2)
}

fn usage() -> ! {
    eprintln!("{USAGE}");
    exit(2)
}
