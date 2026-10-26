//! Random kills (family G of the quality plan, bounded): ordinary commands
//! that start a few background children, with foil itself SIGKILLed at a
//! random moment. Afterwards no process the run started may survive, and
//! nothing foil made for the run (its network proxy, the proxy's folder)
//! may be left behind.
//!
//! Strict runs end everything through their PID namespace even when foil is
//! killed, so they run wherever strict mode works. Plain branch runs rely on
//! the cgroup box and run only in box mode (FOIL_REQUIRE_CGROUP).
//! FOIL_KILL_ROUNDS sets the rounds per mode (default 20).

mod common;

use std::time::{Duration, Instant};

use common::{branches_available, Env};

/// Processes whose command line contains `token`.
fn survivors(token: &str) -> Vec<String> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else { continue };
        let cmdline = String::from_utf8_lossy(&cmdline).replace('\0', " ");
        if cmdline.contains(token) {
            found.push(format!("{name}: {cmdline}"));
        }
    }
    found
}

/// Wait until no process carries `token`, up to `limit`; the survivors if any.
fn settle(token: &str, limit: Duration) -> Vec<String> {
    let start = Instant::now();
    loop {
        let left = survivors(token);
        if left.is_empty() || start.elapsed() > limit {
            return left;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn rounds() -> u64 {
    std::env::var("FOIL_KILL_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(20)
}

#[cfg(target_os = "linux")]
fn proxy_dirs() -> usize {
    std::fs::read_dir(std::env::temp_dir())
        .map(|d| d.flatten().filter(|e| e.file_name().to_string_lossy().starts_with("foil-proxy-")).count())
        .unwrap_or(0)
}

#[cfg(target_os = "linux")]
/// One round: start `foil run <mode...>` with a few background sleepers,
/// kill foil after `delay_ms`, and require that everything goes away.
fn round(env: &Env, mode: &[&str], token: &str, delay_ms: u64) -> Result<(), String> {
    // `: token` marks each shell so it can be found; all of it is harmless.
    let script = format!("sh -c 'sleep 30; : {token}' & sh -c 'sleep 30; : {token}' & sleep 30; : {token}");
    let mut args = vec!["run"];
    args.extend_from_slice(mode);
    args.extend_from_slice(&["--", "sh", "-c", &script]);
    let mut child = env.foil(&args).spawn().map_err(|e| e.to_string())?;
    std::thread::sleep(Duration::from_millis(delay_ms));
    let _ = child.kill(); // SIGKILL: no chance to clean up
    let _ = child.wait();
    let left = settle(token, Duration::from_secs(5));
    if !left.is_empty() {
        return Err(format!("mode {mode:?}, killed after {delay_ms} ms: still running:\n{}", left.join("\n")));
    }
    Ok(())
}

#[test]
#[cfg(target_os = "linux")] // reads /proc; macOS: tests/mac.rs
fn strict_runs_leave_nothing_when_foil_is_killed() {
    if !branches_available() {
        return;
    }
    let env = Env::new("kill-strict");
    let probe = env.foil(&["run", "--strict", "--", "true"]).output().unwrap();
    if !probe.status.success() {
        assert!(std::env::var_os("FOIL_REQUIRE_BRANCH").is_none(), "strict unavailable");
        return;
    }
    let dirs_before = proxy_dirs();
    let mut rng = 0x9e37_79b9_7f4a_7c15_u64;
    let mut failures = Vec::new();
    for i in 0..rounds() {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let delay = 10 + rng % 400;
        let token = format!("foilkill{}x{i}", std::process::id());
        let branch = format!("b{i}");
        let allow = format!("{token}.example");
        // Alternate: plain strict, strict in a branch, strict with the proxy.
        let mode: Vec<&str> = match i % 3 {
            0 => vec!["--strict"],
            1 => vec!["--strict", "-b", &branch],
            _ => vec!["--strict", "--allow-host", &allow],
        };
        if let Err(e) = round(&env, &mode, &token, delay) {
            failures.push(e);
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    // The proxy dies with foil (its dead-man's switch); a killed foil can't
    // remove the proxy's folder, so the next foil that starts a proxy does.
    let out = env.foil(&["run", "--strict", "--allow-host", "x.example", "--", "true"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let left = proxy_dirs().saturating_sub(dirs_before);
    assert_eq!(left, 0, "{left} foil-proxy-* folders left in the temp folder");
}

#[test]
fn branch_runs_in_a_box_leave_nothing_when_foil_is_killed() {
    if !branches_available() {
        return;
    }
    if std::env::var_os("FOIL_REQUIRE_CGROUP").is_none() {
        eprintln!("skipped: needs box mode (FOIL_REQUIRE_CGROUP)");
        return;
    }
    let env = Env::new("kill-box");
    let mut failures = Vec::new();
    for i in 0..rounds() {
        let token = format!("foilboxkill{}x{i}", std::process::id());
        let branch = format!("b{i}");
        // Without a PID namespace, children may outlive a SIGKILLed foil until
        // the branch is used again: the next foil to take the branch's lock
        // finds the old run's box and ends everything in it (by design, see
        // the stale-box tests). So: kill foil, then use the branch, then check.
        let script = format!("sh -c 'sleep 30; : {token}' & sh -c 'sleep 30; : {token}' & sleep 30; : {token}");
        let mut child = env.foil(&["run", "-b", &branch, "--", "sh", "-c", &script]).spawn().unwrap();
        std::thread::sleep(Duration::from_millis(20 + (i * 37) % 300));
        let _ = child.kill();
        let _ = child.wait();
        let out = env.foil(&["abort", &branch]).output().unwrap();
        if !out.status.success() {
            failures.push(format!("abort b{i}: {}", String::from_utf8_lossy(&out.stderr)));
        }
        let left = settle(&token, Duration::from_secs(5));
        if !left.is_empty() {
            failures.push(format!("b{i}: still running after the branch was used again:\n{}", left.join("\n")));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
