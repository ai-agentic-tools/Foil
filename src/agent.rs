//! `foil agent NAME`: run a coding agent's whole session in a strict branch.
//!
//! The whole session, not each command it runs: agents edit files with
//! their own built-in tools, from their own process, so only wrapping the
//! process catches every change. A profile says what each agent needs:
//! which of its own state folders it must write, which hosts it talks to,
//! and which API key the network proxy should inject for it.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Profile {
    pub name: &'static str,
    /// The program to run, found on PATH.
    pub program: &'static str,
    /// State the agent must keep, relative to the home folder: folders
    /// (created if missing) and files (bound only if they exist).
    pub state: &'static [&'static str],
    /// Hosts the agent may reach directly (through the allowlist proxy).
    pub hosts: &'static [&'static str],
    /// API whose key the proxy injects, when that key is in foil's environment.
    pub inject: &'static str,
    /// Variables set inside the run.
    pub env: &'static [(&'static str, &'static str)],
}

pub const PROFILES: [Profile; 2] = [
    Profile {
        name: "claude",
        program: "claude",
        state: &[".claude", ".claude.json"],
        hosts: &["api.anthropic.com"],
        inject: "anthropic",
        env: &[("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"), ("DISABLE_AUTOUPDATER", "1")],
    },
    Profile {
        name: "codex",
        program: "codex",
        state: &[".codex"],
        hosts: &["api.openai.com"],
        inject: "openai",
        env: &[],
    },
];

pub fn profile(name: &str) -> io::Result<&'static Profile> {
    PROFILES.iter().find(|p| p.name == name).ok_or_else(|| {
        let known: Vec<&str> = PROFILES.iter().map(|p| p.name).collect();
        io::Error::new(io::ErrorKind::InvalidInput, format!("no agent profile {name}; known: {}", known.join(", ")))
    })
}

/// Where `program` is on PATH.
pub fn find_program(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(program)).find(|p| p.is_file())
}

/// What the agent's installation needs to be readable: the folder of the
/// real program file (after symlinks) and the one above it, which covers
/// npm packages and versioned installs under the home folder. Never the
/// home folder itself or a folder right in it (`~/.local`, `~/.cargo`):
/// those hold credentials. Then the program file alone stands in for them.
pub fn install_dirs(program: &Path, home: &Path) -> Vec<PathBuf> {
    let Ok(real) = program.canonicalize() else { return Vec::new() };
    let home = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
    let too_broad = |d: &Path| home.starts_with(d) || d.parent() == Some(home.as_path());
    let mut dirs: Vec<PathBuf> = real.ancestors().skip(1).take(2).map(Path::to_path_buf).collect();
    dirs.retain(|d| d != Path::new("/") && !["/usr", "/bin", "/opt"].iter().any(|s| d.starts_with(s)));
    let before = dirs.len();
    dirs.retain(|d| !too_broad(d));
    if dirs.len() < before && !dirs.iter().any(|d| real.starts_with(d)) {
        dirs.push(real);
    }
    dirs
}

/// The agent's state paths that exist (folders are created first).
pub fn state_paths(p: &Profile, home: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for rel in p.state {
        let path = home.join(rel);
        if !rel.trim_start_matches('.').contains('.') {
            // A folder name like ".claude" or ".codex".
            let _ = std::fs::create_dir_all(&path);
        }
        if path.exists() {
            paths.push(path);
        }
    }
    paths
}

/// `<agent>-<YYYYMMDD>-<HHMMSS>` in UTC: a fresh branch per session.
pub fn branch_name(agent: &str) -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (y, m, d) = civil(days as i64);
    format!("{agent}-{y:04}{m:02}{d:02}-{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian
/// (Howard Hinnant's algorithm).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::{civil, install_dirs};
    use std::path::{Path, PathBuf};

    #[test]
    fn install_dirs_never_show_the_home_folder() {
        let root = std::env::temp_dir().join(format!("foil-install-dirs-{}", std::process::id()));
        let home = root.join("home");
        for dir in ["bin", ".cargo/bin", ".local/share/claude/versions"] {
            std::fs::create_dir_all(home.join(dir)).unwrap();
        }
        for file in ["bin/codex", ".cargo/bin/tool", ".local/share/claude/versions/2.1"] {
            std::fs::write(home.join(file), "").unwrap();
        }
        let home = home.canonicalize().unwrap();
        let dirs = |rel: &str| install_dirs(&home.join(rel), &home);
        let under = |rel: &str| home.join(rel);
        // ~/bin/codex: not ~ or ~/bin, just the program.
        assert_eq!(dirs("bin/codex"), vec![under("bin/codex")]);
        // ~/.cargo/bin/tool: ~/.cargo/bin, never ~/.cargo (credentials.toml).
        assert_eq!(dirs(".cargo/bin/tool"), vec![under(".cargo/bin")]);
        // A versioned install deeper down keeps both folders.
        let versions: Vec<PathBuf> = vec![under(".local/share/claude/versions"), under(".local/share/claude")];
        assert_eq!(dirs(".local/share/claude/versions/2.1"), versions);
        assert!(install_dirs(Path::new("/nonexistent/x"), &home).is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn dates() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(19_723), (2024, 1, 1));
        assert_eq!(civil(20_739), (2026, 10, 13));
    }
}
