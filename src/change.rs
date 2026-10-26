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
