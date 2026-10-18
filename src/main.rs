use std::ffi::OsString;
use std::io::{ErrorKind, Write};
use std::path::PathBuf;
use std::process::exit;

use foil::branch::Branch;
use foil::isolate::{self, Options};
use foil::limits::{parse_size, Limits};
use foil::supervisor::{exit_like, supervise, Termination};

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
       foil agent NAME [run options] [-- agent args]
                               run agent NAME (claude, codex) in a strict branch with
                               its profile; afterwards, review and commit or abort
       foil fanout -n N [--check CMD] [--agent NAME] [--name BASE] [--pick]
                   [run options] [--] <command or agent args...>
                               run N attempts at once, in branches BASE-1..BASE-N
                               (FOIL_ATTEMPT=i in each); CMD scores each attempt in
                               its branch (exit 0 = pass); the best is the passing
                               attempt with the fewest changes; --pick commits it
       foil pick NAME          commit fanout attempt NAME, discard its siblings
       foil diff [--patch | --json] [--all] NAME
                               what branch NAME changed (A added, M modified,
                               D deleted, R replaced), or the full patch;
                               paths .gitignore ignores only with --all
       foil commit [--force] [--all] NAME
                               apply branch NAME to its workspace, then delete it;
                               refuses files changed there since, unless --force;
                               leaves out ignored paths (build output), unless --all
       foil abort [--force] NAME
                               delete branch NAME; the workspace is untouched
                               (--force: also after an interrupted commit)
       foil list [--json]      list branches and their workspaces
       foil doctor             check what works on this machine
       foil setup [--print]    allow foil to use user namespaces (Ubuntu; as root)
       foil --version";

fn main() {
    // Inside a container on Ubuntu: get foil's AppArmor profile back if the
    // runtime started us without it (re-executes once; otherwise returns).
    foil::container::regain_apparmor_profile();
    let mut args = std::env::args_os().skip(1);
    let Some(sub) = args.next() else { usage() };
    let rest: Vec<OsString> = args.collect();
    match sub.to_str() {
        Some("run") => run(rest),
        Some("agent") => agent(rest),
        Some("fanout") => fanout(rest),
        Some("pick") => {
            let (_, name) = flags_and_name(rest, &[]);
            finish(foil::fanout::pick(&name).map(|(n, gone)| {
                println!("applied {n} changes; discarded {}", if gone.is_empty() { "nothing".to_owned() } else { gone.join(", ") });
            }))
        }
        // Internal: PID 1 of a strict run, and its network proxy.
        Some("__pid1") => foil::pid1::main(rest.strip_prefix(&[OsString::from("--")][..]).unwrap_or(&rest)),
        Some("__proxy") => foil::proxy::main(&rest),
        Some("diff") => diff(rest),
        Some("commit") => {
            let (flags, name) = flags_and_name(rest, &["--force", "--all"]);
            let force = flags.contains(&"--force");
            let all = flags.contains(&"--all");
            finish(Branch::open(&name).and_then(|b| b.commit(force, all)).map(|n| {
                println!("applied {n} changes");
            }))
        }
        Some("abort") => {
            let (flags, name) = flags_and_name(rest, &["--force"]);
            let force = flags.contains(&"--force");
            finish(Branch::open(&name).and_then(|b| b.abort(force)))
        }
        Some("list") => list(rest),
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

/// A parsed `foil run` (or `foil agent`) request.
#[derive(Default)]
struct RunSpec {
    branch_name: Option<String>,
    workspace: Option<PathBuf>,
    options: Options,
    limits: Limits,
    allow_host: Vec<String>,
    inject: Vec<String>,
    command: Vec<OsString>,
}

fn run(args: Vec<OsString>) {
    let spec = parse_run(args, RunSpec::default());
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
            "--allow-host" => spec.allow_host.push(text()),
            "--allow-env" => o.allow_env.push(text()),
            "--inject-key" => spec.inject.push(text()),
            "--setenv" => match text().split_once('=') {
                Some((k, v)) if !k.is_empty() => o.setenv.push((k.to_owned(), v.to_owned())),
                _ => bad(&flag, &value),
            },
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
            | "--allow-host"
            | "--allow-env"
            | "--inject-key"
            | "--setenv"
            | "--memory"
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

    // With limits, this re-runs foil (with its original arguments) in a
    // limited systemd scope and does not return; inside that scope it
    // checks the limits and returns.
    let original: Vec<OsString> = std::env::args_os().skip(1).collect();
    let isolated = spec.branch_name.is_some() || options.protect || options.no_network || options.strict;
    if let Err(err) = spec.limits.enforce(&original, isolated) {
        fail(err)
    }

    // The network proxy, outside the sandbox, for an allowlist or injected keys.
    let proxy = if options.strict && (!spec.allow_host.is_empty() || !spec.inject.is_empty()) {
        match start_proxy(&spec.allow_host, &spec.inject) {
            Ok((dir, child)) => {
                options.proxy_dir = Some(dir.clone());
                let socket = format!("{}:{}/proxy.sock", isolate::PROXY_PORT, foil::strict::PROXY_DIR);
                options.setenv.push(("FOIL_RELAY".to_owned(), socket));
                options.setenv.extend(foil::proxy::sandbox_env(isolate::PROXY_PORT, &spec.inject));
                Some((dir, child))
            }
            Err(err) => fail(err),
        }
    } else {
        None
    };
    if options.strict {
        options.setenv.push(("FOIL_STATUS_FD".to_owned(), foil::strict::STATUS_FD.to_string()));
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

    let result = supervise(&spec.command, plan.as_ref());
    drop(held);
    if let Some((dir, mut child)) = proxy {
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(dir);
    }
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
    spec.options.allow_read = agent::install_dirs(&program);
    spec.allow_host = profile.hosts.iter().map(|h| h.to_string()).collect();
    if foil::proxy::has_key(profile.inject) {
        spec.inject.push(profile.inject.to_owned());
    }
    spec.options.setenv = profile.env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    // The user's own options come next; whatever follows goes to the agent.
    let mut spec = parse_run(args.collect(), spec);
    let agent_args = std::mem::take(&mut spec.command);
    spec.command = std::iter::once(program.into_os_string()).chain(agent_args).collect();
    let branch = spec.branch_name.get_or_insert_with(|| agent::branch_name(&name)).clone();

    let termination = execute(&spec);
    summarize(&branch, termination);
    exit_like(termination)
}

/// `foil fanout -n N [--check CMD] [--agent NAME] [--name BASE] [--pick] [run options] [--] ...`.
fn fanout(args: Vec<OsString>) {
    use foil::fanout::{self, Plan};
    let mut args = args.into_iter().peekable();
    let (mut attempts, mut check, mut agent, mut base, mut pick) = (None, None, None, None, false);
    // Fanout's own flags may come anywhere among the `foil run` options;
    // those are set aside (with their values) for every attempt.
    let mut rest: Vec<OsString> = Vec::new();
    while let Some(flag) = args.peek().and_then(|a| a.to_str()).map(str::to_owned) {
        if flag == "--pick" {
            args.next();
            pick = true;
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
    eprintln!("  {:<width$}  status  check  changes  time", "attempt", width = plan.base.len() + 3);
    for a in &results {
        let status = a.status.map_or("signal".to_owned(), |c| c.to_string());
        let check = match a.check {
            Some(true) => "pass",
            Some(false) => "fail",
            None => "-",
        };
        eprintln!("  {:<width$}  {status:<6}  {check:<5}  {:<7}  {:.1} s", a.branch, a.changes, a.seconds, width = plan.base.len() + 3);
    }
    eprintln!("logs: {}", results[0].log.parent().map(|p| p.display().to_string()).unwrap_or_default());
    let Some(best) = fanout::best(&results) else {
        eprintln!("foil: no attempt passed; branches kept for review (foil diff NAME, foil abort NAME)");
        exit(1)
    };
    let name = &results[best].branch;
    if !pick {
        eprintln!("best: {name}\nreview: foil diff --patch {name}\nkeep it: foil pick {name} (commits it, discards the others)");
        return;
    }
    match fanout::pick(name) {
        Ok((n, gone)) => eprintln!("foil: picked {name}: applied {n} changes; discarded {} other attempts", gone.len()),
        Err(err) => {
            eprintln!("foil: picking {name}: {err}");
            exit(1)
        }
    }
}

/// After an agent session: what the branch changed, and what to do with it.
fn summarize(name: &str, termination: Termination) {
    let how = match termination {
        Termination::Exited(code) => format!("exited with status {code}"),
        Termination::Signaled(sig) => format!("was stopped by signal {sig}"),
    };
    eprintln!("\nfoil: the agent {how}.");
    let Ok(branch) = Branch::open(name) else { return };
    let (changes, _) = branch.changes(false).unwrap_or_default();
    if changes.is_empty() {
        eprintln!("foil: no changes in branch {name}; removing it.");
        let _ = branch.abort(false);
        return;
    }
    eprintln!("foil: branch {name} changed {} paths in {}:", changes.len(), branch.workspace.display());
    for change in changes.iter().take(15) {
        eprintln!("  {} {}", change.kind.letter(), change.path.display());
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
        "c" | "commit" => match branch.commit(false, false) {
            Ok(n) => eprintln!("foil: applied {n} changes"),
            Err(err) => eprintln!("foil: {err}"),
        },
        "a" | "abort" => match branch.abort(false) {
            Ok(()) => eprintln!("foil: branch {name} discarded"),
            Err(err) => eprintln!("foil: {err}"),
        },
        _ => eprintln!("foil: kept branch {name}; review with `foil diff --patch {name}`"),
    }
}

/// Start `foil __proxy` on a socket in a fresh private folder; return the
/// folder and the process once the socket accepts connections.
fn start_proxy(allow: &[String], inject: &[String]) -> std::io::Result<(PathBuf, std::process::Child)> {
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::process::CommandExt;
    let dir = std::env::temp_dir().join(format!("foil-proxy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    let socket = dir.join("proxy.sock");
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("__proxy").arg(&socket).stdin(std::process::Stdio::null());
    for host in allow {
        cmd.args(["--allow", host]);
    }
    for name in inject {
        cmd.args(["--inject", name]);
    }
    // SAFETY: prctl is async-signal-safe. The proxy must not outlive foil.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong);
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::os::unix::net::UnixStream::connect(&socket).is_err() {
        if std::time::Instant::now() > deadline || child.try_wait()?.is_some() {
            let _ = child.kill();
            return Err(std::io::Error::other("the network proxy did not start"));
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    Ok((dir, child))
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
                            "{{\"kind\":\"{}\",\"path\":{}}}",
                            c.kind.letter(),
                            json_string(&c.path.to_string_lossy())
                        )
                    })
                    .collect();
                writeln!(out, "[{}]", items.join(","))
            }
            None => {
                for change in changes {
                    writeln!(out, "{} {}", change.kind.letter(), change.path.display())?;
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
