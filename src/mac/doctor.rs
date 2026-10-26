//! macOS: `foil doctor` and `foil setup`.
//!
//! Every check runs foil for real, in a scratch workspace next to the
//! branch store (clones can't cross volumes), and reports what works and
//! what is weaker than on Linux.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

struct Check {
    ok: bool,
    what: &'static str,
    detail: String,
}

/// `foil setup`: nothing is needed on macOS.
pub fn setup(_print_only: bool) -> io::Result<()> {
    println!("foil needs no setup on macOS (Seatbelt and APFS clones are built in); `foil doctor` checks this machine");
    Ok(())
}

/// `foil doctor`. Returns whether branches work.
pub fn doctor() -> io::Result<bool> {
    let mut checks = Vec::new();
    let version = sysctl_string("kern.osproductversion").unwrap_or_else(|| "unknown".to_owned());
    let major: u32 = version.split('.').next().and_then(|v| v.parse().ok()).unwrap_or(0);
    checks.push(Check { ok: major >= 12, what: "macOS", detail: format!("{version} (needs 12 or newer)") });

    let sandboxed = crate::isolate::foil_is_sandboxed();
    let seatbelt = Path::new("/usr/bin/sandbox-exec").exists() && !sandboxed;
    checks.push(Check {
        ok: seatbelt,
        what: "seatbelt",
        detail: if sandboxed {
            "foil itself runs inside a sandbox, and sandboxes can't nest: run foil outside it".to_owned()
        } else if seatbelt {
            "sandbox-exec works; every run gets a profile".to_owned()
        } else {
            "/usr/bin/sandbox-exec is missing".to_owned()
        },
    });

    let scratch = Scratch::new()?;
    let branches = scratch.foil(&["run", "-b", "probe", "--", "sh", "-c", "echo x > new.txt"], &[]).and_then(|_| {
        if scratch.ws.join("new.txt").exists() {
            return Err("the run wrote to the real workspace".to_owned());
        }
        let diff = scratch.foil_output(&["diff", "probe"])?;
        if diff.trim() == "A new.txt" {
            Ok(())
        } else {
            Err(format!("unexpected diff: {diff:?}"))
        }
    });
    checks.push(Check {
        ok: branches.is_ok(),
        what: "branches",
        detail: match &branches {
            Ok(()) => format!("APFS clone branches work (stored in {})", crate::branch::home()?.display()),
            Err(why) => format!("{why} (branches need APFS, with FOIL_HOME on the workspace's volume)"),
        },
    });

    let outside = scratch.root.join("outside.txt");
    let protect = scratch
        .foil(&["run", "--protect", "--", "sh", "-c", &format!("echo x > '{}'", outside.display())], &[])
        .map_or(Ok(()), |()| Err("a write outside was allowed".to_owned()))
        .and_then(|()| if outside.exists() { Err("a write outside was allowed".to_owned()) } else { Ok(()) });
    checks.push(Check {
        ok: protect.is_ok(),
        what: "--protect",
        detail: protect.map_or_else(|why| why, |()| "writes outside the branch and temp folders are refused".to_owned()),
    });

    let home = std::env::var("HOME").unwrap_or_default();
    let strict = scratch
        .foil(&["run", "--strict", "--setenv", &format!("REAL_HOME={home}"), "--", "sh", "-c", "ls \"$REAL_HOME\""], &[])
        .map_or(Ok(()), |()| Err("the real home folder could be listed".to_owned()));
    checks.push(Check {
        ok: strict.is_ok(),
        what: "--strict",
        detail: strict.map_or_else(
            |why| why,
            |()| {
                "home folders hidden, private home, clean environment, signals only within the run; \
                 other processes stay visible (weaker than Linux)"
                    .to_owned()
            },
        ),
    });

    let offline = scratch
        .foil(&["run", "--no-network", "--", "/usr/bin/curl", "-s", "-m", "3", "-o", "/dev/null", "https://example.com"], &[])
        .map_or(Ok(()), |()| Err("a connection got out".to_owned()));
    checks.push(Check {
        ok: offline.is_ok(),
        what: "--no-network",
        detail: offline.map_or_else(|why| why, |()| "only loopback is reachable".to_owned()),
    });

    let pidfile = scratch.root.join("detached.pid");
    let cleanup = scratch
        .foil(&["run", "--", "sh", "-c", &format!("(nohup sleep 60 >/dev/null 2>&1 & echo $! > '{}')", pidfile.display())], &[])
        .and_then(|()| {
            let pid: i32 = fs::read_to_string(&pidfile).ok().and_then(|p| p.trim().parse().ok()).ok_or("no pid recorded")?;
            // SAFETY: kill with signal 0 only checks for existence.
            if unsafe { libc::kill(pid, 0) } == 0 {
                unsafe { libc::kill(pid, libc::SIGKILL) };
                return Err("a detached process survived the run".to_owned());
            }
            Ok(())
        });
    checks.push(Check {
        ok: cleanup.is_ok(),
        what: "cleanup",
        detail: cleanup.map_or_else(
            |why| why,
            |()| "every process a run starts is found through its profile and stopped, detached ones too".to_owned(),
        ),
    });

    checks.push(Check {
        ok: true,
        what: "limits",
        detail: "--memory (a watchdog, not a kernel limit) and --timeout; --cpus/--pids need Linux".to_owned(),
    });

    for check in &checks {
        println!("{} {:<13} {}", if check.ok { "ok " } else { "NO " }, check.what, check.detail);
    }
    Ok(branches.is_ok())
}

fn sysctl_string(name: &str) -> Option<String> {
    let out = Command::new("/usr/sbin/sysctl").args(["-n", name]).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// A scratch workspace and branch store, next to the real store.
struct Scratch {
    root: PathBuf,
    ws: PathBuf,
    home: PathBuf,
}

impl Scratch {
    fn new() -> io::Result<Scratch> {
        let store = crate::branch::home()?;
        fs::create_dir_all(&store)?;
        let root = store.join(format!(".doctor-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let ws = root.join("ws");
        let home = root.join("home");
        fs::create_dir_all(&ws)?;
        fs::create_dir_all(&home)?;
        Ok(Scratch { root: root.canonicalize()?, ws: ws.canonicalize()?, home: home.canonicalize()? })
    }

    fn command(&self, args: &[&str], env: &[(&str, &str)]) -> io::Result<Command> {
        let mut cmd = Command::new(std::env::current_exe()?);
        cmd.args(args).current_dir(&self.ws).env("FOIL_HOME", &self.home).env("FOIL_GRACE_MS", "200");
        cmd.envs(env.iter().copied()).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        Ok(cmd)
    }

    /// Run foil; Ok if it exited 0, else its error output.
    fn foil(&self, args: &[&str], env: &[(&str, &str)]) -> Result<(), String> {
        let out = self.command(args, env).and_then(|mut c| c.output()).map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
        }
    }

    fn foil_output(&self, args: &[&str]) -> Result<String, String> {
        let out = self.command(args, &[]).and_then(|mut c| c.output()).map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = Command::new("chmod").args(["-R", "u+rwx"]).arg(&self.root).status();
        let _ = fs::remove_dir_all(&self.root);
    }
}
