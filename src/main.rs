use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::process::exit;

use foil::branch::Branch;
use foil::supervisor::{exit_like, supervise};

const USAGE: &str = "\
usage: foil run [--branch NAME [--workspace DIR]] [--] <command> [args...]
       foil diff NAME      list what branch NAME changed (A added, M modified, D deleted, R replaced)
       foil commit NAME    apply branch NAME to its workspace, then delete the branch
       foil abort NAME     delete branch NAME; the workspace is untouched
       foil list           list branches and their workspaces";

fn main() {
    let mut args = std::env::args_os().skip(1);
    let Some(sub) = args.next() else { usage() };
    match sub.to_str() {
        Some("run") => run(args.collect()),
        Some("diff") => diff(&name_arg(args)),
        Some("commit") => finish(Branch::open(&name_arg(args)).and_then(Branch::commit).map(|n| {
            println!("applied {n} changes");
        })),
        Some("abort") => finish(Branch::open(&name_arg(args)).and_then(Branch::abort)),
        Some("list") => list(),
        _ => usage(),
    }
}

fn run(mut args: Vec<OsString>) {
    // Options come first; the first other word starts the command.
    let mut branch_name = None;
    let mut workspace = None;
    while let Some(first) = args.first().and_then(|a| a.to_str()) {
        let (flag, inline) = match first.split_once('=') {
            Some((flag, value)) => (flag, Some(value.to_owned())),
            None => (first, None),
        };
        if flag != "--branch" && flag != "--workspace" {
            break;
        }
        let flag = flag.to_owned();
        args.remove(0);
        let value = match inline {
            Some(value) => OsString::from(value),
            None if !args.is_empty() => args.remove(0),
            None => usage(),
        };
        if flag == "--branch" {
            branch_name = Some(value);
        } else {
            workspace = Some(PathBuf::from(value));
        }
    }
    if args.first().is_some_and(|a| a == "--") {
        args.remove(0);
    }
    if args.is_empty() || (workspace.is_some() && branch_name.is_none()) {
        usage();
    }

    // Keep the branch and its lock alive until the command and everything
    // it started are gone.
    let mut opened = None;
    if let Some(name) = branch_name {
        let workspace = workspace.unwrap_or_else(|| PathBuf::from("."));
        let ready = name
            .to_str()
            .ok_or_else(|| std::io::Error::new(ErrorKind::InvalidInput, "branch name must be UTF-8"))
            .and_then(|name| Branch::open_or_create(name, &workspace))
            .and_then(|branch| {
                let lock = branch.lock()?;
                let prepared = branch.prepare()?;
                Ok((branch, lock, prepared))
            });
        match ready {
            Ok(ready) => opened = Some(ready),
            Err(err) => {
                eprintln!("foil: {err}");
                exit(125);
            }
        }
    }

    match supervise(&args, opened.as_ref().map(|(_, _, prepared)| prepared)) {
        Ok(outcome) => {
            drop(opened);
            exit_like(outcome.termination)
        }
        Err(err) => {
            eprintln!("foil: {}: {err}", args[0].to_string_lossy());
            // Same conventions as POSIX shells and `docker run`.
            exit(match err.kind() {
                ErrorKind::NotFound => 127,
                ErrorKind::PermissionDenied => 126,
                _ => 125,
            })
        }
    }
}

fn diff(name: &str) {
    finish(Branch::open(name).and_then(|branch| branch.diff()).map(|changes| {
        for change in changes {
            println!("{} {}", change.kind.letter(), change.path.display());
        }
    }))
}

fn list() {
    finish(Branch::list().map(|branches| {
        for branch in branches {
            println!("{}\t{}", branch.name, branch.workspace.display());
        }
    }))
}

/// The single branch-name argument of diff, commit and abort.
fn name_arg(mut args: impl Iterator<Item = OsString>) -> String {
    match (args.next().map(OsString::into_string), args.next()) {
        (Some(Ok(name)), None) => name,
        _ => usage(),
    }
}

fn finish(result: std::io::Result<()>) {
    if let Err(err) = result {
        eprintln!("foil: {err}");
        exit(1);
    }
}

fn usage() -> ! {
    eprintln!("{USAGE}");
    exit(2)
}
