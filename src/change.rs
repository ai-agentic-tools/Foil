//! What a branch changes in its workspace, as `foil diff` lists it. Shared
//! by every platform's branches.

use std::path::PathBuf;

/// One change a branch makes to its workspace, as `foil diff` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub kind: ChangeKind,
    /// Relative to the workspace.
    pub path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    /// Deleted and created again as a directory, or replaced by something
    /// of another type (a directory by a file, a file by a directory): only
    /// the new contents count.
    Replaced,
}

impl ChangeKind {
    pub fn letter(self) -> char {
        match self {
            ChangeKind::Added => 'A',
            ChangeKind::Modified => 'M',
            ChangeKind::Deleted => 'D',
            ChangeKind::Replaced => 'R',
        }
    }
}

/// What git keeps in `.git` as data: objects, refs, logs, the index and
/// the state of a merge or rebase. Git reads these but never runs them.
const GIT_DATA: [&str; 19] = [
    "objects", "refs", "logs", "index", "HEAD", "ORIG_HEAD", "FETCH_HEAD", "MERGE_HEAD", "MERGE_MSG", "MERGE_MODE",
    "AUTO_MERGE", "CHERRY_PICK_HEAD", "REVERT_HEAD", "COMMIT_EDITMSG", "packed-refs", "description", "rebase-merge",
    "rebase-apply", "sequencer",
];

impl Change {
    /// Whether git on your machine could run code from this change once it
    /// is committed: anything in a `.git` folder but git's data (above), so
    /// its config (`core.fsmonitor`, `core.hooksPath`, aliases), its hooks,
    /// and a new or replaced `.git` (which may be a file pointing elsewhere).
    /// Git runs these without asking the next time you use it in the
    /// workspace, so commit keeps them out unless asked (`--guarded`).
    /// `.git` in any case: macOS finds `.GIT` when git looks for `.git`.
    pub fn guarded(&self) -> bool {
        if self.runs_elsewhere() {
            return true;
        }
        let mut parts = self.path.iter().skip_while(|p| !p.eq_ignore_ascii_case(".git"));
        if parts.next().is_none() {
            return false;
        }
        match (parts.next(), parts.next()) {
            (None, _) => matches!(self.kind, ChangeKind::Added | ChangeKind::Replaced),
            (Some(info), Some(file)) if info == "info" && file == "exclude" => false,
            (Some(first), _) => !GIT_DATA.iter().any(|d| first == *d),
        }
    }
}

/// Files at the top of a workspace that tools outside foil act on without
/// asking: foil's own policy (an agent must not widen its next run's
/// rules), Claude Code's project settings (hooks run shell commands), MCP
/// server lists (each server is a command), editor tasks and settings that
/// run programs, and git hooks kept in the tree (husky). An agent in a
/// sandbox that changes these is reaching outside it, later.
const RUN_ELSEWHERE: [&str; 9] = [
    ".foil",
    ".claude/settings.json",
    ".claude/settings.local.json",
    ".claude/hooks",
    ".mcp.json",
    ".cursor/mcp.json",
    ".vscode/tasks.json",
    ".vscode/settings.json",
    ".husky",
];

impl Change {
    /// Whether this change is in (or is) one of `RUN_ELSEWHERE`'s paths.
    fn runs_elsewhere(&self) -> bool {
        let parts: Vec<String> = self.path.iter().map(|p| p.to_string_lossy().to_ascii_lowercase()).collect();
        RUN_ELSEWHERE.iter().any(|g| {
            let g: Vec<&str> = g.split('/').collect();
            parts.len() >= g.len() && parts.iter().zip(&g).all(|(a, b)| a == b)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Change, ChangeKind};

    #[test]
    fn files_other_tools_run_are_guarded() {
        assert!(guarded(ChangeKind::Modified, ".foil/policy"));
        assert!(guarded(ChangeKind::Deleted, ".foil"));
        assert!(guarded(ChangeKind::Added, ".claude/settings.json"));
        assert!(guarded(ChangeKind::Added, ".claude/settings.local.json"));
        assert!(guarded(ChangeKind::Added, ".claude/hooks/pre.sh"));
        assert!(guarded(ChangeKind::Modified, ".mcp.json"));
        assert!(guarded(ChangeKind::Modified, ".VSCode/tasks.json"));
        assert!(guarded(ChangeKind::Added, ".husky/pre-commit"));
        assert!(!guarded(ChangeKind::Added, ".claude/commands/review.md"));
        assert!(!guarded(ChangeKind::Modified, "CLAUDE.md"));
        assert!(!guarded(ChangeKind::Modified, "src/.foil.rs"));
        assert!(!guarded(ChangeKind::Modified, "docs/.mcp.json.md"));
        assert!(!guarded(ChangeKind::Modified, ".vscode/extensions.json"));
    }

    fn guarded(kind: ChangeKind, path: &str) -> bool {
        Change { kind, path: path.into() }.guarded()
    }

    #[test]
    fn git_config_and_hooks_are_guarded() {
        assert!(guarded(ChangeKind::Modified, ".git/config"));
        assert!(guarded(ChangeKind::Added, ".git/hooks"));
        assert!(guarded(ChangeKind::Added, ".git/hooks/post-commit"));
        assert!(guarded(ChangeKind::Added, ".git/info/attributes"));
        assert!(guarded(ChangeKind::Modified, ".git/modules/lib/config"));
        assert!(guarded(ChangeKind::Modified, "vendor/lib/.git/config"));
        assert!(guarded(ChangeKind::Replaced, ".git"));
        assert!(guarded(ChangeKind::Added, "sub/.git"));
        assert!(guarded(ChangeKind::Added, ".GIT/config"));
        assert!(guarded(ChangeKind::Added, ".git/Hooks/pre-commit"));
    }

    #[test]
    fn git_data_and_ordinary_files_are_not() {
        assert!(!guarded(ChangeKind::Added, ".git/objects/ab/cdef"));
        assert!(!guarded(ChangeKind::Modified, ".git/index"));
        assert!(!guarded(ChangeKind::Modified, ".git/refs/heads/main"));
        assert!(!guarded(ChangeKind::Modified, ".git/HEAD"));
        assert!(!guarded(ChangeKind::Modified, ".git/info/exclude"));
        assert!(!guarded(ChangeKind::Modified, ".git"));
        assert!(!guarded(ChangeKind::Deleted, ".git"));
        assert!(!guarded(ChangeKind::Modified, "src/.gitignore"));
        assert!(!guarded(ChangeKind::Modified, "docs/git/config"));
    }
}
