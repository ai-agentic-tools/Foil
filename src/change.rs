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

#[cfg(test)]
mod tests {
    use super::{Change, ChangeKind};

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
