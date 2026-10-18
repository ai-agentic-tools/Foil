//! `foil fanout`: run N attempts of one task side by side, each in its own
//! branch of the same workspace, then score them and keep the best.
//!
//! An agent is not deterministic: the same prompt gives a different fix
//! each time. Branches are cheap, so instead of one attempt, run several,
//! check each one (e.g. with the project's tests), and commit the winner.
//! Every attempt is an ordinary `foil run` (or `foil agent`) child process
//! of this one, so it gets exactly the isolation those give.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Instant;

use crate::branch::{self, Branch};

/// What to run, as `foil fanout` was asked.
pub struct Plan {
    /// Attempts are branches `<base>-1` ... `<base>-<attempts>`.
    pub base: String,
    pub attempts: usize,
    /// `Some(name)`: each attempt is `foil agent name`; else `foil run`.
    pub agent: Option<String>,
    /// Shell command run in each attempt's branch afterwards; exit 0 = pass.
    pub check: Option<String>,
    /// `foil run` options, passed on unchanged to every attempt.
    pub run_options: Vec<OsString>,
    /// Whether those options include `--strict` (always true for agents).
    pub strict: bool,
    pub command: Vec<OsString>,
}

/// How one attempt went.
pub struct Attempt {
    pub branch: String,
    /// Exit status; `None` if a signal stopped it.
    pub status: Option<i32>,
    pub seconds: f64,
    /// `None` without `--check`, or when the attempt left no branch.
    pub check: Option<bool>,
    /// Paths the branch changes (ignored build output not counted).
    pub changes: usize,
    pub log: PathBuf,
}

/// Run every attempt at once, wait for all, then check each one.
pub fn run(plan: &Plan) -> io::Result<Vec<Attempt>> {
    let logs = branch::home()?.join("fanout").join(&plan.base);
    std::fs::create_dir_all(&logs)?;
    let foil = std::env::current_exe()?;

    let mut running = Vec::new();
    for i in 1..=plan.attempts {
        let branch = format!("{}-{i}", plan.base);
        let log = logs.join(format!("attempt-{i}.log"));
        let out = File::create(&log)?;
        let mut cmd = Command::new(&foil);
        match &plan.agent {
            Some(name) => cmd.arg("agent").arg(name),
            None => cmd.arg("run"),
        };
        cmd.args(&plan.run_options).args(["-b", &branch]);
        // A strict run starts from a clean environment, so pass it in.
        if plan.strict {
            cmd.arg("--setenv").arg(format!("FOIL_ATTEMPT={i}"));
        }
        cmd.arg("--").args(&plan.command).env("FOIL_ATTEMPT", i.to_string());
        cmd.stdin(Stdio::null()).stdout(out.try_clone()?).stderr(out);
        running.push((branch, log, Instant::now(), cmd.spawn()?));
    }

    let mut attempts = Vec::new();
    for (branch, log, started, mut child) in running {
        let status = child.wait()?;
        attempts.push(Attempt {
            branch,
            status: status.code(),
            seconds: started.elapsed().as_secs_f64(),
            check: None,
            changes: 0,
            log,
        });
    }

    if let Some(check) = &plan.check {
        // Checks run side by side too, each in its attempt's branch.
        let mut checking = Vec::new();
        for (n, a) in attempts.iter().enumerate() {
            if Branch::open(&a.branch).is_err() {
                continue; // an agent that changed nothing leaves no branch
            }
            let out = File::options().append(true).open(&a.log)?;
            let mut cmd = Command::new(&foil);
            cmd.arg("run");
            if plan.agent.is_some() {
                cmd.arg("--strict"); // agent options assume a strict run
            }
            cmd.args(&plan.run_options).args(["-b", &a.branch, "--", "sh", "-c", check]);
            cmd.stdin(Stdio::null()).stdout(out.try_clone()?).stderr(out);
            checking.push((n, cmd.spawn()?));
        }
        for (n, mut child) in checking {
            attempts[n].check = Some(child.wait()?.success());
        }
    }

    for a in &mut attempts {
        if let Ok(b) = Branch::open(&a.branch) {
            a.changes = b.changes(false)?.0.len();
        }
    }
    Ok(attempts)
}

/// The best attempt: it changed something and passed its check (or, with
/// no check, exited 0); among those, the smallest change, then the first.
pub fn best(attempts: &[Attempt]) -> Option<usize> {
    attempts
        .iter()
        .enumerate()
        .filter(|(_, a)| a.changes > 0 && a.check.unwrap_or(a.status == Some(0)))
        .min_by_key(|(i, a)| (a.changes, *i))
        .map(|(i, _)| i)
}

/// `<base>` of an attempt branch named `<base>-<number>`.
fn base_of(name: &str) -> Option<&str> {
    let (base, n) = name.rsplit_once('-')?;
    (!base.is_empty() && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())).then_some(base)
}

/// Commit attempt `name` to its workspace, then discard its sibling
/// attempts and their logs. Returns the number of changes applied and the
/// siblings discarded. If the commit fails, nothing is discarded.
pub fn pick(name: &str) -> io::Result<(usize, Vec<String>)> {
    let base = base_of(name).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("{name} is not a fanout attempt (<base>-<number>)"))
    })?;
    let chosen = Branch::open(name)?;
    let workspace = chosen.workspace.clone();
    let applied = chosen.commit(false, false)?;
    let mut discarded = Vec::new();
    for b in Branch::list()? {
        if base_of(&b.name) == Some(base) && b.workspace == workspace {
            discarded.push(b.name.clone());
            b.abort(false)?;
        }
    }
    let _ = std::fs::remove_dir_all(branch::home()?.join("fanout").join(base));
    Ok((applied, discarded))
}

#[cfg(test)]
mod tests {
    use super::{base_of, best, Attempt};

    fn attempt(status: Option<i32>, check: Option<bool>, changes: usize) -> Attempt {
        Attempt { branch: String::new(), status, seconds: 0.0, check, changes, log: Default::default() }
    }

    #[test]
    fn attempt_names() {
        assert_eq!(base_of("fix-bug-3"), Some("fix-bug"));
        assert_eq!(base_of("fix-12"), Some("fix"));
        assert_eq!(base_of("fix"), None);
        assert_eq!(base_of("fix-"), None);
        assert_eq!(base_of("-3"), None);
        assert_eq!(base_of("fix-3a"), None);
    }

    #[test]
    fn best_passes_its_check_with_the_smallest_change() {
        let a = [attempt(Some(0), Some(false), 1), attempt(Some(0), Some(true), 5), attempt(Some(1), Some(true), 2)];
        assert_eq!(best(&a), Some(2)); // the check decides, not the exit status
        let a = [attempt(Some(0), None, 3), attempt(Some(2), None, 1), attempt(Some(0), None, 3)];
        assert_eq!(best(&a), Some(0)); // no check: exit 0; ties go to the first
        let a = [attempt(Some(0), Some(true), 0), attempt(None, None, 4)];
        assert_eq!(best(&a), None); // no change, or killed: never best
    }
}
