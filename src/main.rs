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
         --allow-write PATH    with --protect: keep PATH writable too (repeatable)
         --no-network          no network for the command (loopback only)
         --memory SIZE         memory limit, e.g. 2G (needs systemd)
         --cpus N              CPU limit, e.g. 1.5 (needs systemd)
         --pids N              limit on processes and threads (needs systemd)
       foil diff [--patch | --json] NAME
                               what branch NAME changed (A added, M modified,
                               D deleted, R replaced), or the full patch
       foil commit [--force] NAME
                               apply branch NAME to its workspace, then delete it;
                               refuses files changed there since, unless --force
       foil abort NAME         delete branch NAME; the workspace is untouched
       foil list [--json]      list branches and their workspaces
       foil doctor             check what works on this machine
       foil setup [--print]    allow foil to use user namespaces (Ubuntu; as root)
       foil --version";

fn main() {
    let mut args = std::env::args_os().skip(1);
    let Some(sub) = args.next() else { usage() };
    let rest: Vec<OsString> = args.collect();
    match sub.to_str() {
        Some("run") => run(rest),
        Some("diff") => diff(rest),
        Some("commit") => {
            let (flags, name) = flags_and_name(rest, &["--force"]);
            let force = flags.contains(&"--force");
            finish(Branch::open(&name).and_then(|b| b.commit(force)).map(|n| {
                println!("applied {n} changes");
            }))
        }
        Some("abort") => {
            let (_, name) = flags_and_name(rest, &[]);
            finish(Branch::open(&name).and_then(Branch::abort))
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

fn run(args: Vec<OsString>) {
    let all = args.clone();
    let mut args = args.into_iter().peekable();
    let mut branch_name = None;
    let mut workspace = None;
    let mut options = Options::default();
    let mut limits = Limits::default();
    // Options come first; the first other word starts the command.
    while let Some(first) = args.peek().and_then(|a| a.to_str()).map(str::to_owned) {
        if first == "--" {
            args.next();
            break;
        }
        let (flag, inline) = match first.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag.to_owned(), Some(OsString::from(value))),
            _ => (first.clone(), None),
        };
        let takes_value = matches!(
            flag.as_str(),
            "-b" | "--branch" | "-w" | "--workspace" | "--allow-write" | "--memory" | "--cpus" | "--pids"
        );
        let switch = matches!(flag.as_str(), "--protect" | "--no-network");
        if !takes_value && !switch {
            break;
        }
        args.next();
        if switch {
            if inline.is_some() {
                usage();
            }
            match flag.as_str() {
                "--protect" => options.protect = true,
                _ => options.no_network = true,
            }
            continue;
        }
        let value = inline.or_else(|| args.next()).unwrap_or_else(|| usage());
        let text = || value.to_str().map(str::to_owned).unwrap_or_else(|| bad(&flag, &value));
        match flag.as_str() {
            "-b" | "--branch" => branch_name = Some(text()),
            "-w" | "--workspace" => workspace = Some(PathBuf::from(&value)),
            "--allow-write" => options.allow_write.push(PathBuf::from(&value)),
            "--memory" => limits.memory = Some(parse_size(&text()).unwrap_or_else(|| bad(&flag, &value))),
            "--cpus" => {
                let cpus = text().parse::<f64>().ok().filter(|c| c.is_finite() && *c > 0.0);
                limits.cpus = Some(cpus.unwrap_or_else(|| bad(&flag, &value)));
            }
            _ => limits.pids = Some(text().parse().ok().filter(|n| *n > 0).unwrap_or_else(|| bad(&flag, &value))),
        }
    }
    let command: Vec<OsString> = args.collect();
    if command.is_empty() || (workspace.is_some() && branch_name.is_none()) {
        usage();
    }

    // With limits, this re-runs foil in a limited systemd scope and does not
    // return; inside that scope it checks the limits and returns.
    let mut all_args = vec![OsString::from("run")];
    all_args.extend(all);
    let isolated = branch_name.is_some() || options.protect || options.no_network;
    if let Err(err) = limits.enforce(&all_args, isolated) {
        eprintln!("foil: {err}");
        exit(125);
    }

    // Keep the branch and its lock alive until the command and everything
    // it started are gone.
    let mut held = None;
    let ready = (|| {
        let overlay = match &branch_name {
            Some(name) => {
                let branch = Branch::open_or_create(name, &workspace.unwrap_or_else(|| PathBuf::from(".")))?;
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
        Err(err) => {
            eprintln!("foil: {err}");
            exit(125);
        }
    };

    match supervise(&command, plan.as_ref()) {
        Ok(outcome) => {
            drop(held);
            if limits.memory.is_some()
                && outcome.termination == Termination::Signaled(libc::SIGKILL)
                && foil::limits::oom_killed()
            {
                eprintln!("foil: {}: killed for using more memory than --memory allows", command[0].to_string_lossy());
            }
            exit_like(outcome.termination)
        }
        Err(err) => {
            eprintln!("foil: {}: {err}", command[0].to_string_lossy());
            // Same conventions as POSIX shells and `docker run`.
            exit(match err.kind() {
                ErrorKind::NotFound => 127,
                ErrorKind::PermissionDenied => 126,
                _ => 125,
            })
        }
    }
}

fn diff(args: Vec<OsString>) {
    let (flags, name) = flags_and_name(args, &["--patch", "--json"]);
    if flags.len() > 1 {
        usage();
    }
    let branch = Branch::open(&name);
    let mut out = std::io::stdout().lock();
    finish(branch.and_then(|branch| match flags.first() {
        Some(&"--patch") => branch.write_patch(&mut out),
        Some(_) => {
            let items: Vec<String> = branch
                .diff()?
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
            for change in branch.diff()? {
                writeln!(out, "{} {}", change.kind.letter(), change.path.display())?;
            }
            Ok(())
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
