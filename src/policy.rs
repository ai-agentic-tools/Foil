//! Policy files: the run options a workspace asks for, written down.
//!
//! `.foil/policy` in a workspace holds one rule per line: a `foil run`
//! option without its leading `--`, then its value, if it takes one.
//!
//! ```text
//! # Agents in this repository may reach GitHub's API and PyPI, nothing else.
//! allow-host api.github.com
//! allow-host pypi.org
//! allow-host files.pythonhosted.org
//! hide .env
//! hide config/credentials.json
//! timeout 30m
//! ```
//!
//! Strict runs (`foil agent`, `foil run --strict`, fanout) read it on
//! their own; `--policy FILE` names another file. A policy can only add
//! rules to the command line, never take the command line's away, and a
//! branch that changes `.foil/` is guarded: commit leaves the change out,
//! so an agent can't widen its own policy for the next run.
//!
//! Paths in `allow-read` and `allow-write` may start with `~/`; relative
//! paths are relative to the workspace. `hide` paths are inside the
//! workspace and may use `*` within a name (`hide .env*`).

use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};

/// Where a workspace keeps its policy, relative to it.
pub const FILE: &str = ".foil/policy";

/// The options a policy may set: (name, takes a value).
const RULES: [(&str, bool); 13] = [
    ("strict", false),
    ("no-network", false),
    ("allow-host", true),
    ("allow-read", true),
    ("allow-write", true),
    ("allow-env", true),
    ("setenv", true),
    ("inject-key", true),
    ("hide", true),
    ("timeout", true),
    ("memory", true),
    ("cpus", true),
    ("pids", true),
];

/// A parsed policy: the file it came from and its rules as `foil run` arguments.
#[derive(Debug, Default, Clone)]
pub struct Policy {
    pub path: PathBuf,
    pub args: Vec<OsString>,
    pub rules: usize,
}

/// Parse policy `text` (from `path`, whose paths are relative to `workspace`).
pub fn parse(text: &str, path: &Path, workspace: &Path, home: Option<&Path>) -> io::Result<Policy> {
    let mut policy = Policy { path: path.to_path_buf(), ..Policy::default() };
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let bad = |why: &str| io::Error::new(io::ErrorKind::InvalidData, format!("{}:{}: {why}: {line}", path.display(), n + 1));
        let (name, value) = match line.split_once(char::is_whitespace) {
            Some((name, value)) => (name, Some(value.trim())),
            None => (line, None),
        };
        let name = name.trim_start_matches("--");
        let Some(&(_, takes_value)) = RULES.iter().find(|(r, _)| *r == name) else {
            let known: Vec<&str> = RULES.iter().map(|r| r.0).collect();
            return Err(bad(&format!("unknown rule (known: {})", known.join(", "))));
        };
        match (takes_value, value) {
            (true, None) | (true, Some("")) => return Err(bad("missing value")),
            (false, Some(_)) => return Err(bad("takes no value")),
            _ => {}
        }
        policy.args.push(format!("--{name}").into());
        if let Some(value) = value {
            let value = match name {
                "allow-read" | "allow-write" => resolve(value, workspace, home).into_os_string(),
                "hide" => {
                    check_hide(value).map_err(|why| bad(&why))?;
                    value.into()
                }
                _ => value.into(),
            };
            policy.args.push(value);
        }
        policy.rules += 1;
    }
    Ok(policy)
}

/// Read `path` as a policy, or `None` if it doesn't exist.
pub fn load(path: &Path, workspace: &Path) -> io::Result<Option<Policy>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(io::Error::new(err.kind(), format!("{}: {err}", path.display()))),
    };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    parse(&text, path, workspace, home.as_deref()).map(Some)
}

fn resolve(value: &str, workspace: &Path, home: Option<&Path>) -> PathBuf {
    match (value.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => home.join(rest),
        _ if Path::new(value).is_absolute() => PathBuf::from(value),
        _ => workspace.join(value),
    }
}

/// A `hide` path must stay inside the workspace.
pub fn check_hide(value: &str) -> Result<(), String> {
    let path = Path::new(value.trim_end_matches('/'));
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err("hide takes a path inside the workspace, relative to it".to_owned());
    }
    if !path.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err("hide paths may not use . or ..".to_owned());
    }
    if path.components().any(|c| c.as_os_str().to_string_lossy().contains("**")) {
        return Err("hide matches * within one name only, not **".to_owned());
    }
    Ok(())
}

/// The paths in `workspace` that `pattern` names (`*` matches within one
/// name). Symlinks are never followed, nor hidden: what they point to is
/// either outside the view already or inside the workspace, to be hidden there.
pub fn expand(workspace: &Path, pattern: &str) -> Vec<PathBuf> {
    let mut found = vec![PathBuf::new()];
    for part in Path::new(pattern.trim_end_matches('/')).components() {
        let part = part.as_os_str().to_string_lossy().into_owned();
        let mut next = Vec::new();
        for rel in found {
            if !part.contains('*') {
                let candidate = rel.join(&part);
                if std::fs::symlink_metadata(workspace.join(&candidate)).is_ok() {
                    next.push(candidate);
                }
                continue;
            }
            let Ok(entries) = std::fs::read_dir(workspace.join(&rel)) else { continue };
            let mut names: Vec<String> = entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
            names.sort();
            for name in names.into_iter().filter(|n| glob(&part, n)) {
                next.push(rel.join(name));
            }
        }
        found = next;
    }
    found.retain(|rel| {
        !rel.as_os_str().is_empty()
            && std::fs::symlink_metadata(workspace.join(rel)).is_ok_and(|m| !m.file_type().is_symlink())
    });
    found
}

/// `*` matches any run of characters (none included); everything else matches itself.
pub fn glob(pattern: &str, name: &str) -> bool {
    let (p, n) = (pattern.as_bytes(), name.as_bytes());
    let (mut i, mut j, mut star, mut mark) = (0, 0, None, 0);
    while j < n.len() {
        if i < p.len() && p[i] == b'*' {
            star = Some(i);
            mark = j;
            i += 1;
        } else if i < p.len() && p[i] == n[j] {
            i += 1;
            j += 1;
        } else if let Some(s) = star {
            i = s + 1;
            mark += 1;
            j = mark;
        } else {
            return false;
        }
    }
    p[i..].iter().all(|&c| c == b'*')
}

/// Names that usually hold secrets. Templates (`.env.example`) don't.
pub fn looks_secret(name: &str, is_dir: bool) -> bool {
    let lower = name.to_ascii_lowercase();
    if is_dir {
        return matches!(lower.as_str(), "secrets" | ".secrets" | ".aws" | ".ssh" | ".gnupg");
    }
    let template = [".example", ".sample", ".template", ".dist", ".defaults"].iter().any(|t| lower.ends_with(t));
    if template {
        return false;
    }
    lower == ".env"
        || lower.starts_with(".env.")
        || lower.ends_with(".env")
        || [".pem", ".key", ".p12", ".pfx", ".jks", ".keystore", ".tfvars", ".kubeconfig"].iter().any(|e| lower.ends_with(e))
        || lower.starts_with("id_rsa")
        || lower.starts_with("id_ecdsa")
        || lower.starts_with("id_ed25519")
        || matches!(lower.as_str(), ".npmrc" | ".pypirc" | ".netrc" | ".git-credentials" | "credentials.json" | "credentials" | ".htpasswd" | "terraform.tfstate")
        || (lower.starts_with("service-account") && lower.ends_with(".json"))
}

/// Secret-looking paths in `workspace`, a few folders deep, skipping the
/// usual big folders. Relative to the workspace, sorted.
pub fn find_secrets(workspace: &Path) -> Vec<PathBuf> {
    const SKIP: [&str; 8] = [".git", "node_modules", "target", ".venv", "venv", "__pycache__", "dist", "build"];
    let mut found = Vec::new();
    let mut stack = vec![(PathBuf::new(), 0usize)];
    while let Some((rel, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(workspace.join(&rel)) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_symlink() {
                continue;
            }
            let child = rel.join(&name);
            if looks_secret(&name, kind.is_dir()) {
                found.push(child);
            } else if kind.is_dir() && depth < 4 && !SKIP.contains(&name.as_str()) {
                stack.push((child, depth + 1));
            }
        }
    }
    found.sort();
    found
}

/// The policy `foil policy init` writes for `workspace`.
pub fn template(secrets: &[PathBuf]) -> String {
    let mut out = String::from(
        "# foil policy: the rules every strict run in this workspace follows\n\
         # (foil agent, foil run --strict, foil fanout). One rule per line:\n\
         # a foil run option without its --, then its value.\n\
         # Review changes to this file like code: commit leaves out changes\n\
         # an agent makes to .foil/, so it can't widen its own rules.\n\
         \n\
         # Network: nothing but the agent's own API, unless listed here.\n\
         # allow-host api.github.com\n\
         # allow-host pypi.org\n\
         # allow-host files.pythonhosted.org\n\
         # allow-host registry.npmjs.org\n\
         \n\
         # Files the agent must not read, though they are in the workspace.\n",
    );
    if secrets.is_empty() {
        out.push_str("# hide .env\n");
    }
    for s in secrets {
        out.push_str(&format!("hide {}\n", s.display()));
    }
    out.push_str(
        "\n# Folders outside the workspace the agent may read or write.\n\
         # allow-read ~/.cache/pip\n\
         \n\
         # Environment variables passed in (everything else is left out).\n\
         # allow-env CI\n\
         \n\
         # Limits.\n\
         timeout 2h\n\
         # memory 8G\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_become_run_options() {
        let text = "# comment\nallow-host api.github.com\n\nhide .env\nallow-read ~/.cache/pip\nallow-write out\nstrict\n";
        let p = parse(text, Path::new("p"), Path::new("/ws"), Some(Path::new("/home/u"))).unwrap();
        let args: Vec<String> = p.args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(
            args,
            ["--allow-host", "api.github.com", "--hide", ".env", "--allow-read", "/home/u/.cache/pip", "--allow-write", "/ws/out", "--strict"]
        );
        assert_eq!(p.rules, 5);
    }

    #[test]
    fn bad_rules_are_refused_with_their_line() {
        for (text, why) in [
            ("branch x", "unknown rule"),
            ("workspace /", "unknown rule"),
            ("allow-host", "missing value"),
            ("strict yes", "takes no value"),
            ("hide /etc/passwd", "inside the workspace"),
            ("hide ../other/.env", ". or .."),
            ("hide **/.env", "not **"),
        ] {
            let err = parse(text, Path::new("p"), Path::new("/ws"), None).unwrap_err().to_string();
            assert!(err.contains(why) && err.contains("p:1"), "{text}: {err}");
        }
    }

    #[test]
    fn globs() {
        assert!(glob(".env*", ".env"));
        assert!(glob(".env*", ".env.local"));
        assert!(glob("*.pem", "server.pem"));
        assert!(!glob("*.pem", "server.pem.bak"));
        assert!(glob("a*b*c", "aXXbYYc"));
        assert!(!glob("a*b*c", "aXXbYY"));
    }

    #[test]
    fn secret_names() {
        for s in [".env", ".env.production", "prod.env", "server.pem", "id_ed25519", ".npmrc", "credentials.json", "service-account-prod.json", "terraform.tfstate"] {
            assert!(looks_secret(s, false), "{s}");
        }
        for s in [".env.example", ".env.sample", "README.md", "keys.rs", "environment.ts", "monkey.py"] {
            assert!(!looks_secret(s, false), "{s}");
        }
        assert!(looks_secret("secrets", true));
        assert!(!looks_secret("src", true));
    }

    #[test]
    fn expand_finds_files_and_skips_symlinks() {
        let root = std::env::temp_dir().join(format!("foil-policy-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::write(root.join(".env"), "A=1").unwrap();
        std::fs::write(root.join(".env.local"), "A=2").unwrap();
        std::fs::write(root.join("config/key.pem"), "k").unwrap();
        std::os::unix::fs::symlink("/etc/hosts", root.join(".env.link")).unwrap();
        assert_eq!(expand(&root, ".env*"), [PathBuf::from(".env"), PathBuf::from(".env.local")]);
        assert_eq!(expand(&root, "config/*.pem"), [PathBuf::from("config/key.pem")]);
        assert_eq!(expand(&root, "config/"), [PathBuf::from("config")]);
        assert!(expand(&root, "missing").is_empty());
        assert_eq!(find_secrets(&root), [PathBuf::from(".env"), PathBuf::from(".env.local"), PathBuf::from("config/key.pem")]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
