//! macOS: the sandbox, from Seatbelt.
//!
//! Seatbelt is macOS's built-in sandbox. A profile (a small Scheme-like
//! rules file) says what a process and everything it starts may read,
//! write, connect to and signal; the kernel enforces it, and the process
//! can't drop it. `/usr/bin/sandbox-exec -p PROFILE command` starts a
//! command under a profile. In a profile the last matching rule wins, so
//! each block below first denies broadly and then allows what it needs.
//!
//! Every run gets a profile, even a plain one, because the profile is also
//! how foil finds the run's processes again (see `procs.rs`).
//!
//! What the Linux version gets from namespaces, this one gets from rules:
//! - a branch run may write its branch tree but not the real workspace or
//!   foil's store (on Linux the overlay hides the real folder instead);
//! - `--protect`: no writes outside the branch, temp folders and
//!   `--allow-write` paths;
//! - `--no-network`: loopback only;
//! - `--strict`: no reads of any home folder or other volume except the
//!   branch and allowed paths, an empty private home, no network except
//!   foil's proxy, signals and process information only within the run, and
//!   a clean environment.
//!
//! Weaker than Linux: the run shares the machine's loopback, process IDs
//! and system services (Mach services stay reachable), and a strict run can
//! still see that other processes exist.

use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::procs::Marker;

/// What the user asked for on the command line (same fields as on Linux).
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub no_network: bool,
    pub protect: bool,
    pub allow_write: Vec<PathBuf>,
    pub strict: bool,
    pub allow_read: Vec<PathBuf>,
    pub allow_env: Vec<String>,
    pub setenv: Vec<(String, String)>,
    /// Linux only (the proxy's socket folder); unused here.
    pub proxy_dir: Option<PathBuf>,
    /// The loopback port of foil's proxy, the only address a strict run may reach.
    pub proxy_port: Option<u16>,
}

/// Variables a strict run keeps from foil's environment.
const BASIC_ENV: [&str; 17] = [
    "PATH", "USER", "LOGNAME", "SHELL", "TERM", "COLORTERM", "TERM_PROGRAM", "LANG", "LANGUAGE", "TZ", "NO_COLOR",
    "FORCE_COLOR", "COLUMNS", "LINES", "EDITOR", "TMPDIR", "__CF_USER_TEXT_ENCODING",
];

/// Paths a write-restricted run may still write: temp folders and devices.
const SCRATCH: [&str; 2] = ["/private/tmp", "/private/var/tmp"];

/// A branch as a run sees it: the real folder, the clone the command works
/// in, and foil's store (which the command must not touch).
#[derive(Debug, Clone)]
pub struct Overlay {
    pub workspace: PathBuf,
    pub tree: PathBuf,
    pub store: PathBuf,
    /// The branch's own folder, for a strict run's private home.
    pub dir: PathBuf,
}

/// How to run one command.
#[derive(Debug, Clone)]
pub struct Plan {
    pub profile: String,
    pub marker: Marker,
    pub cwd: Option<PathBuf>,
    env: Option<Vec<(OsString, OsString)>>,
    /// A private home made for this run only (removed afterwards).
    pub scratch_home: Option<PathBuf>,
    /// `--memory`: the watchdog stops the run past this many bytes.
    pub memory: Option<u64>,
}

impl Plan {
    /// The plan for these options. Always `Some`: even a plain run gets a
    /// profile, so that its processes can be found and stopped.
    pub fn new(options: &Options, overlay: Option<Overlay>) -> io::Result<Option<Plan>> {
        if !options.allow_write.is_empty() && !options.protect && !options.strict {
            return Err(invalid("--allow-write only makes sense with --protect or --strict".to_owned()));
        }
        if !options.allow_read.is_empty() && !options.strict {
            return Err(invalid("--allow-read only makes sense with --strict".to_owned()));
        }
        let isolated = overlay.is_some() || options.protect || options.no_network || options.strict;
        if foil_is_sandboxed() {
            if isolated {
                return Err(io::Error::other(
                    "foil itself runs inside a macOS sandbox (Seatbelt), and sandboxes can't nest; \
                     run foil outside it (for example, not from inside another agent's sandbox)",
                ));
            }
            return Ok(None);
        }
        let marker = Marker::create()?;
        let cwd = std::env::current_dir()?;
        let mut p = String::from("(version 1)\n(allow default)\n");
        p.push_str(&marker.rule());

        let canon = |path: &Path, flag: &str| {
            path.canonicalize().map_err(|err| io::Error::new(err.kind(), format!("{flag} {}: {err}", path.display())))
        };
        let allow_write: Vec<PathBuf> = options.allow_write.iter().map(|p| canon(p, "--allow-write")).collect::<Result<_, _>>()?;
        let allow_read: Vec<PathBuf> = options.allow_read.iter().map(|p| canon(p, "--allow-read")).collect::<Result<_, _>>()?;

        // Where the command starts: the same place inside the branch tree.
        let canonical_cwd = cwd.canonicalize().ok();
        let run_cwd = overlay.as_ref().map(|o| match canonical_cwd.as_deref().and_then(|c| c.strip_prefix(&o.workspace).ok()) {
            Some(rel) if !rel.as_os_str().is_empty() => o.tree.join(rel),
            _ => o.tree.clone(),
        });

        if let Some(o) = &overlay {
            // The real folder and foil's own store stay untouched until commit.
            p.push_str(&format!("(deny file-write* (subpath {}) (subpath {}))\n", quote(&o.workspace), quote(&o.store)));
            p.push_str(&format!("(allow file-write* (subpath {}))\n", quote(&o.tree)));
        }

        // A strict run's private home: empty, writable, holding links to the
        // allowed paths that live under the real home.
        let real_home = std::env::var_os("HOME").map(PathBuf::from).filter(|h| h.is_absolute());
        let mut scratch_home = None;
        let private_home = if options.strict {
            let home = match &overlay {
                Some(o) => o.dir.join("home"),
                None => {
                    let h = marker.denied.with_extension("home");
                    scratch_home = Some(h.clone());
                    h
                }
            };
            std::fs::create_dir_all(&home)?;
            if let Some(real) = &real_home {
                for path in allow_read.iter().chain(&allow_write) {
                    if let Ok(rel) = path.strip_prefix(real) {
                        let link = home.join(rel);
                        if let Some(parent) = link.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        let _ = std::fs::remove_file(&link);
                        let _ = std::os::unix::fs::symlink(path, &link);
                    }
                }
            }
            Some(home)
        } else {
            None
        };

        if options.protect || options.strict {
            p.push_str("(deny file-write*)\n(allow file-write*\n");
            if let Some(o) = &overlay {
                p.push_str(&format!("  (subpath {})\n", quote(&o.tree)));
            }
            for s in SCRATCH {
                p.push_str(&format!("  (subpath {})\n", quote(Path::new(s))));
            }
            if let Some(tmp) = std::env::var_os("TMPDIR").map(PathBuf::from).and_then(|t| t.canonicalize().ok()) {
                p.push_str(&format!("  (subpath {})\n", quote(&tmp)));
            }
            if let Some(home) = &private_home {
                p.push_str(&format!("  (subpath {})\n", quote(&home.canonicalize()?)));
            }
            for path in &allow_write {
                p.push_str(&format!("  (subpath {})\n", quote(path)));
            }
            p.push_str("  (literal \"/dev/null\") (literal \"/dev/zero\") (literal \"/dev/dtracehelper\")\n");
            p.push_str("  (regex #\"^/dev/tty\") (regex #\"^/dev/fd/\") (regex #\"^/dev/ttys\"))\n");
        }

        if options.strict {
            // Reads: no home folder of anyone, no other volume, except what
            // the run needs. Folders above an allowed path may be looked at
            // (not listed), so paths to it resolve.
            p.push_str("(deny file-read* (subpath \"/Users\") (subpath \"/Volumes\") (subpath \"/private/var/root\"))\n");
            if let Some(home) = real_home.as_ref().and_then(|h| h.canonicalize().ok()) {
                p.push_str(&format!("(deny file-read* (subpath {}))\n", quote(&home)));
            }
            let mut readable: Vec<PathBuf> = Vec::new();
            match (&overlay, &run_cwd) {
                (Some(o), _) => readable.push(o.tree.clone()),
                (None, _) => readable.push(cwd.canonicalize()?),
            }
            readable.extend(allow_read.iter().cloned());
            readable.extend(allow_write.iter().cloned());
            if let Some(home) = &private_home {
                readable.push(home.canonicalize()?);
            }
            // Tools on PATH that live under a home folder (~/.cargo/bin, ~/.local/bin).
            if let Some(path) = std::env::var_os("PATH") {
                readable.extend(std::env::split_paths(&path).filter(|d| d.starts_with("/Users")).filter_map(|d| d.canonicalize().ok()));
            }
            p.push_str("(allow file-read*\n");
            for path in &readable {
                p.push_str(&format!("  (subpath {})\n", quote(path)));
            }
            p.push_str(")\n(allow file-read-metadata\n");
            let mut ancestors: Vec<&Path> = readable.iter().flat_map(|r| r.ancestors().skip(1)).collect();
            ancestors.sort();
            ancestors.dedup();
            for a in ancestors {
                p.push_str(&format!("  (literal {})\n", quote(a)));
            }
            p.push_str(")\n");
            // Only processes of this run may be signalled or inspected.
            p.push_str("(deny signal)\n(allow signal (target same-sandbox))\n");
            p.push_str("(deny process-info*)\n(allow process-info* (target same-sandbox))\n");
        }

        if options.strict || options.no_network {
            p.push_str("(deny network*)\n");
            if options.strict {
                // Only foil's proxy (and Unix sockets the run itself makes inside its own paths).
                if let Some(port) = options.proxy_port {
                    p.push_str(&format!("(allow network-outbound (remote ip \"localhost:{port}\"))\n"));
                }
            } else {
                // Loopback, and Unix sockets as on Linux, but not the DNS
                // resolver's: a name lookup is a way out too. (A broad
                // `network*` rule with a local address lets traffic out:
                // each direction is allowed on its own.)
                p.push_str("(allow network-bind (local ip \"localhost:*\"))\n(allow network-inbound (local ip \"localhost:*\"))\n");
                p.push_str("(allow network-outbound (remote ip \"localhost:*\"))\n(allow network-outbound (remote unix-socket))\n");
                p.push_str("(deny network-outbound (remote unix-socket (path-literal \"/private/var/run/mDNSResponder\")))\n");
            }
        }

        let env = if options.strict {
            let mut env: Vec<(OsString, OsString)> = std::env::vars_os()
                .filter(|(k, _)| {
                    let k = k.to_string_lossy();
                    BASIC_ENV.contains(&k.as_ref()) || k.starts_with("LC_") || options.allow_env.iter().any(|a| *a == k)
                })
                .collect();
            if let Some(home) = &private_home {
                env.push(("HOME".into(), home.clone().into()));
            }
            for (k, v) in &options.setenv {
                env.retain(|(name, _)| name != k.as_str());
                env.push((k.into(), v.into()));
            }
            Some(env)
        } else {
            None
        };
        if std::env::var_os("FOIL_DEBUG_PROFILE").is_some() {
            eprintln!("{p}");
        }
        Ok(Some(Plan { profile: p, marker, cwd: run_cwd, env, scratch_home, memory: None }))
    }

    pub fn is_strict(&self) -> bool {
        self.env.is_some()
    }

    /// The environment the command gets, if not foil's own.
    pub fn env(&self) -> Option<&[(OsString, OsString)]> {
        self.env.as_deref()
    }

    /// The program and arguments to exec for `argv`: under `sandbox-exec`.
    pub fn wrap(&self, argv: &[OsString]) -> (OsString, Vec<OsString>) {
        let mut args = vec![OsString::from("-p"), OsString::from(&self.profile)];
        args.extend_from_slice(argv);
        (OsString::from("/usr/bin/sandbox-exec"), args)
    }

    /// Remove what this run made for itself (its marker files, a scratch home).
    pub fn cleanup(&self) {
        self.marker.remove();
        if let Some(home) = &self.scratch_home {
            let _ = std::fs::remove_dir_all(home);
        }
    }
}

/// `path` as a Seatbelt string literal.
pub fn quote(path: &Path) -> String {
    let mut out = String::from("\"");
    for &b in path.as_os_str().as_bytes() {
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            _ => out.push(b as char),
        }
    }
    out.push('"');
    out
}

/// Whether foil itself runs inside a Seatbelt sandbox (then it can't start one).
pub fn foil_is_sandboxed() -> bool {
    extern "C" {
        fn sandbox_check(pid: libc::pid_t, operation: *const std::ffi::c_char, kind: std::ffi::c_int, ...) -> std::ffi::c_int;
    }
    // SAFETY: a null operation with SANDBOX_FILTER_NONE asks "is it sandboxed at all".
    unsafe { sandbox_check(libc::getpid(), std::ptr::null(), 0) == 1 }
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

#[cfg(test)]
mod tests {
    use super::quote;
    use std::path::Path;

    #[test]
    fn quoting() {
        assert_eq!(quote(Path::new("/a b/c")), "\"/a b/c\"");
        assert_eq!(quote(Path::new("/a\"b\\c")), "\"/a\\\"b\\\\c\"");
    }
}
