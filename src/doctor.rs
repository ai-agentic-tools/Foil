//! `foil doctor` (what works on this machine, and how to fix what doesn't)
//! and `foil setup` (the one-time AppArmor step Ubuntu >= 23.10 needs).
//!
//! Ubuntu lets a program create user namespaces only if an AppArmor profile
//! allows it (kernel.apparmor_restrict_unprivileged_userns=1). `foil setup`
//! installs such a profile for the foil binary at its current path; browsers
//! and container tools ship the same kind of profile. It must be run as root
//! once per install location.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::cgroup::Cgroup;
use crate::isolate::{self, Options, Overlay, Plan};

const APPARMOR_DIR: &str = "/etc/apparmor.d";
const RESTRICT: &str = "/proc/sys/kernel/apparmor_restrict_unprivileged_userns";

/// Whether AppArmor restricts unprivileged user namespaces here.
pub fn apparmor_restricts() -> bool {
    fs::read_to_string(RESTRICT).is_ok_and(|v| v.trim() == "1")
}

/// The AppArmor profile name and file for the foil binary at `exe`,
/// e.g. `foil.usr.local.bin.foil` (AppArmor's own naming convention).
fn profile_name(exe: &Path) -> String {
    let flat: String = exe
        .to_string_lossy()
        .trim_start_matches('/')
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '.' })
        .collect();
    format!("foil.{flat}")
}

/// The profile text: foil stays unconfined, but may create user namespaces.
pub fn profile(exe: &Path) -> String {
    let name = profile_name(exe);
    format!(
        "# Installed by `foil setup`: lets {exe} create user namespaces\n\
         # (for branches, --protect and --no-network). Remove this file and run\n\
         # `apparmor_parser -R` on it to undo.\n\
         abi <abi/4.0>,\n\
         include <tunables/global>\n\
         \n\
         profile {name} \"{exe}\" flags=(unconfined) {{\n  userns,\n\n  include if exists <local/{name}>\n}}\n",
        exe = exe.display()
    )
}

/// `foil setup [--print]`.
pub fn setup(print_only: bool) -> io::Result<()> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let text = profile(&exe);
    if print_only {
        print!("{text}");
        return Ok(());
    }
    if !apparmor_restricts() {
        println!("nothing to do: this system does not restrict user namespaces");
        return Ok(());
    }
    // SAFETY: geteuid cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "installing an AppArmor profile needs root; run:\n  sudo {} setup\n\
                 (or `foil setup --print` to see the profile first)",
                exe.display()
            ),
        ));
    }
    // The profile trusts whatever binary is at this path.
    if let Some(owner) = replaceable_by_non_root(&exe) {
        eprintln!(
            "foil: warning: {} can be replaced by {owner}, who would then also get user \
             namespaces; consider installing foil to /usr/local/bin first",
            exe.display()
        );
    }
    let file = Path::new(APPARMOR_DIR).join(profile_name(&exe));
    fs::write(&file, text)?;
    let status = Command::new("apparmor_parser")
        .args(["-r", "-W"])
        .arg(&file)
        .status()
        .map_err(|err| io::Error::new(err.kind(), format!("apparmor_parser: {err}")))?;
    if !status.success() {
        return Err(io::Error::other(format!("apparmor_parser failed on {}", file.display())));
    }
    println!("installed and loaded {}", file.display());
    println!("{} may now create user namespaces; `foil doctor` to check", exe.display());
    Ok(())
}

/// Who other than root could replace `exe` (it or its folder is theirs, or
/// writable by group or others), if anyone.
fn replaceable_by_non_root(exe: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    for path in [Some(exe), exe.parent()].into_iter().flatten() {
        let meta = fs::metadata(path).ok()?;
        if meta.uid() != 0 {
            return Some(format!("uid {}", meta.uid()));
        }
        if meta.mode() & 0o022 != 0 {
            return Some("group or other users".to_owned());
        }
    }
    None
}

/// One line of the doctor's report.
struct Check {
    ok: bool,
    what: &'static str,
    detail: String,
}

/// `foil doctor`: probe every feature for real. Returns whether branches work.
pub fn doctor() -> io::Result<bool> {
    let mut checks = Vec::new();
    let release = fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let version = kernel_version(&release);
    checks.push(Check {
        ok: version >= (5, 11),
        what: "kernel",
        detail: format!("Linux {} (branches need >= 5.11, --protect >= 5.12)", release.trim()),
    });

    let boxed = Cgroup::create().is_some();
    let limits = limits_available();
    let in_container = crate::container::runtime().is_some();
    checks.push(Check {
        ok: boxed || limits.is_ok() || in_container,
        what: "cleanup box",
        detail: if boxed {
            "own cgroup per run: leftovers are always found and killed".to_owned()
        } else if in_container {
            "no cgroup box inside a container; --strict runs still end every process they start \
             (their PID namespace dies with them); other runs use subreaper cleanup"
                .to_owned()
        } else if limits.is_ok() {
            "branch, isolated and limited runs get one through a systemd scope (~12 ms); \
             plain `foil run` falls back to subreaper cleanup"
                .to_owned()
        } else {
            "no delegated cgroup and no systemd user session: subreaper cleanup only, which a \
             process that double-forks into a new session under another user can escape, and \
             leftovers of a SIGKILLed foil can't be found"
                .to_owned()
        },
    });

    let scratch = Scratch::new()?;
    let probe = |options: Options, overlay: Option<Overlay>| -> io::Result<()> {
        let plan = Plan::new(&options, overlay)?.expect("asks for isolation");
        isolate::probe(&plan)
    };
    let fix = |err: &io::Error| format!("{err}\n      fix: {}", isolate::hint(err));
    let branches = probe(Options::default(), Some(scratch.overlay()?));
    checks.push(Check {
        ok: branches.is_ok(),
        what: "branches",
        detail: match &branches {
            Ok(()) => format!("copy-on-write branches work (stored in {})", scratch.home.display()),
            Err(err) => fix(err),
        },
    });
    let protect = probe(Options { protect: true, ..Options::default() }, None);
    checks.push(Check {
        ok: protect.is_ok(),
        what: "--protect",
        detail: match &protect {
            Ok(()) => "read-only view of everything outside the workspace works".to_owned(),
            Err(err) => fix(err),
        },
    });
    let strict = probe(Options { strict: true, ..Options::default() }, None);
    checks.push(Check {
        ok: strict.is_ok(),
        what: "--strict",
        detail: match &strict {
            Ok(()) => "deny-by-default view, PID/IPC isolation, no privilege gain work".to_owned(),
            Err(err) => fix(err),
        },
    });
    let offline = probe(Options { no_network: true, ..Options::default() }, None);
    checks.push(Check {
        ok: offline.is_ok(),
        what: "--no-network",
        detail: match &offline {
            Ok(()) => "network isolation works".to_owned(),
            Err(err) => fix(err),
        },
    });

    checks.push(Check {
        ok: limits.is_ok(),
        what: "limits",
        detail: match limits {
            Ok(controllers) => format!("--memory/--cpus/--pids via systemd (controllers: {controllers})"),
            Err(why) if in_container => format!("{why}; in a container, use the container's own limits (docker run --memory/--cpus/--pids-limit)"),
            Err(why) => why,
        },
    });

    if let Some(runtime) = crate::container::runtime() {
        let blockers = crate::container::blockers(&scratch.home);
        checks.push(Check {
            ok: blockers.is_empty(),
            what: "container",
            detail: if blockers.is_empty() {
                format!("inside {runtime}, nothing blocks foil")
            } else {
                let lines: Vec<String> = blockers
                    .iter()
                    .map(|b| format!("{}: blocks {}\n      fix: {}", b.what, b.blocks, b.fix))
                    .collect();
                format!("inside {runtime}; {} blocker(s):\n    {}", blockers.len(), lines.join("\n    "))
            },
        });
    }
    for check in &checks {
        println!("{} {:<13} {}", if check.ok { "ok " } else { "NO " }, check.what, check.detail);
    }
    if apparmor_restricts() && branches.is_err() {
        println!("\nThis system restricts user namespaces (Ubuntu >= 23.10). Fix once with:\n  sudo {} setup",
            std::env::current_exe()?.display());
    }
    Ok(branches.is_ok())
}

/// Whether `--memory` etc. can work: systemd-run, a user session, and the
/// controllers delegated to the user manager.
fn limits_available() -> Result<String, String> {
    if !crate::limits::on_path("systemd-run") {
        return Err("systemd-run not found: resource limits need systemd".to_owned());
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    if !runtime.is_some_and(|dir| dir.join("bus").exists()) && std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
        return Err("no systemd user session (no session bus)".to_owned());
    }
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let file = format!("/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/cgroup.controllers");
    let controllers = fs::read_to_string(&file).unwrap_or_default();
    let have: Vec<&str> = controllers.split_whitespace().collect();
    let missing: Vec<&str> = ["memory", "cpu", "pids"].into_iter().filter(|c| !have.contains(c)).collect();
    if missing.is_empty() {
        Ok(have.join(" "))
    } else {
        Err(format!("controllers not delegated to your user manager: {}", missing.join(", ")))
    }
}

fn kernel_version(release: &str) -> (u32, u32) {
    let mut parts = release.trim().split(['.', '-']).map(|p| p.parse().unwrap_or(0));
    (parts.next().unwrap_or(0), parts.next().unwrap_or(0))
}

/// Throwaway branch-like folders inside the branch store, so the probe uses
/// the same filesystem real branches will. Removed on drop.
struct Scratch {
    home: PathBuf,
    dir: PathBuf,
}

impl Scratch {
    fn new() -> io::Result<Scratch> {
        let home = crate::branch::home()?;
        let dir = home.join(format!(".doctor-{}", std::process::id()));
        for part in ["lower", "upper", "work"] {
            fs::create_dir_all(dir.join(part))?;
        }
        Ok(Scratch { home, dir })
    }

    fn overlay(&self) -> io::Result<Overlay> {
        let lower = self.dir.join("lower").canonicalize()?;
        let dir = self.dir.canonicalize()?;
        Overlay::new(&lower, &dir.join("upper"), &dir.join("work"))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Overlayfs leaves work/work with mode 000.
        let _ = Command::new("chmod").args(["-R", "u+rwx"]).arg(&self.dir).status();
        let _ = fs::remove_dir_all(&self.dir);
    }
}
