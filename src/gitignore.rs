//! Which changes git ignores: build output, `__pycache__`, `node_modules`.
//! Shared by every platform's branches.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::change::Change;

/// Which of `changes` git ignores in `workspace` (asked in one
/// `git check-ignore` call). Empty if git is missing or this is not a
/// repository. Files git tracks are never reported, even if a pattern
/// matches them. `is_dir` says whether a changed path is a folder in the
/// branch.
pub fn ignored(workspace: &Path, changes: &[Change], is_dir: impl Fn(&Path) -> bool) -> HashSet<PathBuf> {
    // A workspace that is itself ignored by an enclosing repository (a
    // scratch folder under `target/`, say) is not a project of that
    // repository: its rules would drop every change.
    if workspace_is_ignored(workspace) {
        return HashSet::new();
    }
    let mut input = Vec::new();
    for change in changes {
        input.extend_from_slice(change.path.as_os_str().as_bytes());
        // A trailing slash lets directory-only patterns like `build/`
        // match a directory that exists only in the branch.
        if is_dir(&change.path) {
            input.push(b'/');
        }
        input.push(0);
    }
    let child = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["check-ignore", "-z", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else { return HashSet::new() };
    // Feed stdin from another thread: git answers while we still write,
    // and two full pipes would wait on each other forever.
    let mut stdin = child.stdin.take().expect("piped");
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let output = child.wait_with_output();
    let _ = writer.join();
    // Exit 0: some paths are ignored; 1: none; 128: not a repository.
    let output = match output {
        Ok(output) if output.status.code() == Some(0) => output,
        _ => return HashSet::new(),
    };
    output
        .stdout
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| PathBuf::from(OsStr::from_bytes(p.strip_suffix(b"/").unwrap_or(p))))
        .collect()
}

/// Whether git ignores the workspace folder itself.
fn workspace_is_ignored(workspace: &Path) -> bool {
    let (Some(parent), Some(name)) = (workspace.parent(), workspace.file_name()) else {
        return false;
    };
    Command::new("git")
        .arg("-C")
        .arg(parent)
        .args(["check-ignore", "-q", "--"])
        .arg(name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}
