//! Workspace branches: run a command on a private copy-on-write layer of a
//! folder, then keep (commit) or throw away (abort) what it changed.
//!
//! A branch lives in `$FOIL_HOME/branches/<name>/`: `upper/` holds every
//! file the command created or changed, `work/` is overlayfs scratch space,
//! and `workspace` records which folder it branches. While a command runs,
//! the workspace path shows an overlay of `upper/` on top of the real folder,
//! so the command sees its own changes and the real folder stays untouched.
//!
//! The overlay is mounted inside a new mount namespace, owned by a new user
//! namespace that maps our own uid and gid to themselves: no root is needed,
//! the command keeps its identity, and no process outside the run sees the
//! mount. When the run's last process exits, the namespaces and the mount
//! vanish; the cgroup box makes sure that has happened before foil returns,
//! so commit and abort never race a writer.
//!
//! Needs Linux >= 5.11 (overlayfs in user namespaces) and unprivileged user
//! namespaces, which some systems restrict; see `isolate.rs`, which does the
//! mounting.
//!
//! `box` records the cgroup box of the branch's latest run. If that run's
//! foil was SIGKILLed, processes it started may still be writing through the
//! overlay; the next foil to take the branch's lock kills them first.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{symlink, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use crate::cgroup;
use crate::isolate::{c_path, Overlay};

/// A branch on disk.
pub struct Branch {
    pub name: String,
    /// The folder this branch is a copy-on-write view of (canonical path).
    pub workspace: PathBuf,
    /// `$FOIL_HOME/branches/<name>`.
    dir: PathBuf,
}

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
    /// A directory deleted and created again: only the new contents count.
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

/// Where branches are stored: `$FOIL_HOME`, else `$XDG_DATA_HOME/foil`,
/// else `~/.local/share/foil`.
pub fn home() -> io::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("FOIL_HOME") {
        return Ok(PathBuf::from(dir));
    }
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(dir).join("foil"));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| invalid("neither FOIL_HOME nor HOME is set".to_owned()))?;
    Ok(PathBuf::from(home).join(".local/share/foil"))
}

impl Branch {
    /// Open branch `name`, creating it for `workspace` if it does not exist.
    /// An existing branch continues where it left off, but only for the
    /// workspace it was created for.
    pub fn open_or_create(name: &str, workspace: &Path) -> io::Result<Branch> {
        check_name(name)?;
        let workspace = workspace.canonicalize()?;
        if !workspace.is_dir() {
            return Err(invalid(format!("{} is not a directory", workspace.display())));
        }
        // overlayfs rejects overlapping layers, and storage inside the
        // workspace would also show up in the workspace itself. Checked on
        // the fully resolved path, before anything is created.
        let root = resolve(&home()?)?;
        if root.starts_with(&workspace) {
            return Err(invalid(format!(
                "branch storage {} is inside the workspace {}; set FOIL_HOME to a folder outside it",
                root.display(),
                workspace.display()
            )));
        }
        let branches = root.join("branches");
        fs::create_dir_all(&branches)?;
        let dir = branches.join(name);
        if !dir.exists() {
            // Build the branch under a temporary name, then rename it into
            // place in one step: a branch directory is either complete or
            // absent, even if foil dies half-way or two foils race to create
            // it. (Names starting with '.' are never branch names.)
            let tmp = branches.join(format!(".new-{name}-{}", std::process::id()));
            let built = build_branch(&tmp, &workspace).and_then(|()| fs::rename(&tmp, &dir));
            if let Err(err) = built {
                let _ = remove_all(&tmp);
                // Losing the race to another foil is fine: use its branch.
                if !dir.exists() {
                    return Err(err);
                }
            }
        }
        let existing = Branch::open(name)?;
        if existing.workspace != workspace {
            return Err(invalid(format!(
                "branch {name} belongs to {}, not {}",
                existing.workspace.display(),
                workspace.display()
            )));
        }
        Ok(Branch { name: name.to_owned(), workspace, dir })
    }

    /// Open an existing branch.
    pub fn open(name: &str) -> io::Result<Branch> {
        check_name(name)?;
        let dir = home()?.join("branches").join(name);
        let raw = fs::read(dir.join("workspace")).map_err(|err| match err.kind() {
            io::ErrorKind::NotFound => {
                io::Error::new(io::ErrorKind::NotFound, format!("no branch named {name}"))
            }
            _ => err,
        })?;
        let workspace = PathBuf::from(OsStr::from_bytes(&raw));
        Ok(Branch { name: name.to_owned(), workspace, dir })
    }

    /// Every branch, sorted by name.
    pub fn list() -> io::Result<Vec<Branch>> {
        let entries = match fs::read_dir(home()?.join("branches")) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };
        let mut branches: Vec<Branch> = entries
            .filter_map(|entry| Branch::open(entry.ok()?.file_name().to_str()?).ok())
            .collect();
        branches.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(branches)
    }

    /// Take the branch's lock, so two runs, or a run and a commit, never use
    /// its layers at once. Held until the returned file is dropped.
    pub fn lock(&self) -> io::Result<File> {
        let file = File::create(self.dir.join("lock"))?;
        // SAFETY: flock on a descriptor we own; it is released when the file closes.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == -1 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                return Err(io::Error::new(
                    err.kind(),
                    format!("branch {} is in use by another foil", self.name),
                ));
            }
            return Err(err);
        }
        // Holding the lock means the foil of any earlier run is gone. If it
        // died without cleaning up, what it started may still be running.
        if let Ok(raw) = fs::read(self.dir.join("box")) {
            cgroup::reap_stale(Path::new(OsStr::from_bytes(&raw)));
        }
        Ok(file)
    }

    fn upper(&self) -> PathBuf {
        self.dir.join("upper")
    }

    /// The overlay that shows this branch over its workspace.
    pub fn overlay(&self) -> io::Result<Overlay> {
        Overlay::new(&self.workspace, &self.upper(), &self.dir.join("work"))
    }

    /// Remember `dir` as the cgroup box of the run that now holds the lock.
    pub fn record_box(&self, dir: &Path) -> io::Result<()> {
        fs::write(self.dir.join("box"), dir.as_os_str().as_bytes())
    }

    /// When the branch was created; a workspace path changed after this may
    /// conflict with the branch's own change to it.
    fn created(&self) -> io::Result<SystemTime> {
        fs::symlink_metadata(self.dir.join("workspace"))?.modified()
    }

    /// The changes worth reviewing and committing, in path order, and how
    /// many were left out: in a git repository, paths `.gitignore` ignores
    /// (build output, `__pycache__`, `node_modules`) are left out unless
    /// `all`. Outside a git repository nothing is left out.
    pub fn changes(&self, all: bool) -> io::Result<(Vec<Change>, usize)> {
        let changes = self.diff()?;
        if all {
            return Ok((changes, 0));
        }
        let ignored = self.ignored(&changes);
        let total = changes.len();
        let kept: Vec<Change> = changes.into_iter().filter(|c| !ignored.contains(&c.path)).collect();
        let hidden = total - kept.len();
        Ok((kept, hidden))
    }

    /// Which of `changes` git ignores in the workspace (asked in one
    /// `git check-ignore` call). Empty if git is missing or this is not a
    /// repository. Files git tracks are never reported, even if a pattern
    /// matches them.
    fn ignored(&self, changes: &[Change]) -> HashSet<PathBuf> {
        let mut input = Vec::new();
        for change in changes {
            input.extend_from_slice(change.path.as_os_str().as_bytes());
            // A trailing slash lets directory-only patterns like `build/`
            // match a directory that exists only in the branch.
            if self.upper().join(&change.path).symlink_metadata().is_ok_and(|m| m.is_dir()) {
                input.push(b'/');
            }
            input.push(0);
        }
        let child = Command::new("git")
            .arg("-C")
            .arg(&self.workspace)
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

    /// Every change this branch would make to its workspace, in path order.
    pub fn diff(&self) -> io::Result<Vec<Change>> {
        let mut changes = Vec::new();
        self.collect_changes(Path::new(""), &mut changes)?;
        Ok(changes)
    }

    fn collect_changes(&self, rel: &Path, out: &mut Vec<Change>) -> io::Result<()> {
        let mut entries: Vec<_> = fs::read_dir(self.upper().join(rel))?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let rel = rel.join(entry.file_name());
            let upper = entry.path();
            // DirEntry::metadata does not follow symlinks.
            let meta = entry.metadata()?;
            let existed = self.workspace.join(&rel).symlink_metadata().is_ok();
            let kind = if is_whiteout(&meta) {
                Some(ChangeKind::Deleted)
            } else if meta.is_dir() {
                // A directory is copied up whenever something inside it
                // changes; by itself that is not a change worth listing.
                match (existed, is_opaque(&upper)) {
                    (false, _) => Some(ChangeKind::Added),
                    (true, true) => Some(ChangeKind::Replaced),
                    (true, false) => None,
                }
            } else if existed {
                Some(ChangeKind::Modified)
            } else {
                Some(ChangeKind::Added)
            };
            if let Some(kind) = kind {
                out.push(Change { kind, path: rel.clone() });
            }
            if meta.is_dir() {
                self.collect_changes(&rel, out)?;
            }
        }
        Ok(())
    }

    /// Paths the branch changed that were also changed in the workspace
    /// since the branch was created, e.g. by another branch's commit.
    /// Judged by ctime, which (unlike mtime) cannot be set back by hand.
    pub fn conflicts(&self, all: bool) -> io::Result<Vec<PathBuf>> {
        let created = self.created()?;
        let mut conflicts = Vec::new();
        for change in self.changes(all)?.0 {
            let changed_since = match self.workspace.join(&change.path).symlink_metadata() {
                Ok(meta) => ctime(&meta) > created,
                // Gone now: deleted on both sides is no conflict; otherwise it is.
                Err(_) => change.kind != ChangeKind::Deleted && change.kind != ChangeKind::Added,
            };
            if changed_since {
                conflicts.push(change.path);
            }
        }
        Ok(conflicts)
    }

    /// Apply the branch's changes to its workspace, then delete the branch.
    /// Refuses if any change conflicts (see `conflicts`), unless `force`.
    /// Ignored paths (see `changes`) are dropped with the branch unless `all`.
    /// Each file is replaced atomically, but the commit as a whole is not:
    /// if it fails half-way, the branch is kept so it can be retried.
    pub fn commit(self, force: bool, all: bool) -> io::Result<usize> {
        // Held until the branch is gone, so no run can start in between.
        let _lock = self.lock()?;
        if !force {
            let conflicts = self.conflicts(all)?;
            if !conflicts.is_empty() {
                let list: Vec<String> = conflicts.iter().map(|p| format!("  {}", p.display())).collect();
                return Err(io::Error::other(format!(
                    "branch {} changes files that also changed in {} since the branch was made:\n{}\n\
                     review them, then use `foil commit --force {}` to overwrite, or `foil abort {}`",
                    self.name,
                    self.workspace.display(),
                    list.join("\n"),
                    self.name,
                    self.name
                )));
            }
        }
        let skip = if all { HashSet::new() } else { self.ignored(&self.diff()?) };
        let applied = apply(&self.upper(), &self.workspace, Path::new(""), &skip)?;
        remove_all(&self.dir)?;
        Ok(applied)
    }

    /// Write the branch's changes as a unified diff (via `diff -u`):
    /// text for regular files, a one-line note for everything else.
    pub fn write_patch(&self, out: &mut impl Write, all: bool) -> io::Result<()> {
        for change in self.changes(all)?.0 {
            let old = self.workspace.join(&change.path);
            let new = self.upper().join(&change.path);
            let is_file = |p: &Path| p.symlink_metadata().is_ok_and(|m| m.is_file());
            let (old_file, new_file) = match change.kind {
                ChangeKind::Added => (None, Some(&new)),
                ChangeKind::Deleted => (Some(&old), None),
                ChangeKind::Modified | ChangeKind::Replaced => (Some(&old), Some(&new)),
            };
            let old_ok = old_file.is_none_or(|p| is_file(p));
            let new_ok = new_file.is_none_or(|p| is_file(p));
            if !(old_ok && new_ok) {
                // Directories (their files are listed on their own), symlinks
                // and anything else that is not text.
                if !new.is_dir() || change.kind != ChangeKind::Added {
                    writeln!(out, "foil: {} {} (not a regular file)", change.kind.letter(), change.path.display())?;
                }
                continue;
            }
            let rel = change.path.display();
            let output = Command::new("diff")
                .arg("-u")
                .arg(format!("--label=a/{rel}"))
                .arg(format!("--label=b/{rel}"))
                .arg(old_file.map_or(Path::new("/dev/null"), |p| p.as_path()))
                .arg(new_file.map_or(Path::new("/dev/null"), |p| p.as_path()))
                .output()
                .map_err(|err| io::Error::new(err.kind(), format!("diff: {err}")))?;
            // diff exits 0 for no differences, 1 for differences, 2 for trouble.
            if output.status.code() == Some(2) {
                return Err(io::Error::other(format!("diff: {}", String::from_utf8_lossy(&output.stderr).trim())));
            }
            out.write_all(&output.stdout)?;
        }
        Ok(())
    }

    /// Delete the branch and everything in it. The workspace is untouched.
    pub fn abort(self) -> io::Result<()> {
        let _lock = self.lock()?;
        remove_all(&self.dir)
    }
}

/// The parts of a new branch, created inside `dir`.
fn build_branch(dir: &Path, workspace: &Path) -> io::Result<()> {
    fs::create_dir(dir)?;
    fs::create_dir(dir.join("upper"))?;
    fs::create_dir(dir.join("work"))?;
    fs::write(dir.join("workspace"), workspace.as_os_str().as_bytes())
}

/// `path` made absolute with symlinks and `..` resolved, even when its last
/// parts do not exist yet: the deepest existing ancestor is canonicalized
/// and the rest is applied on top of it.
fn resolve(path: &Path) -> io::Result<PathBuf> {
    let path = std::path::absolute(path)?;
    let base = path.ancestors().find(|a| a.exists()).unwrap_or(Path::new("/"));
    let mut resolved = base.canonicalize()?;
    for part in path.strip_prefix(base).unwrap_or(Path::new("")).components() {
        match part {
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(name) => resolved.push(name),
            _ => {}
        }
    }
    Ok(resolved)
}

/// Copy the changes recorded in `upper` onto `target`, depth first,
/// leaving out the paths in `skip` (relative to the workspace; `rel` is
/// where `upper` sits). Returns the number of changes applied.
fn apply(upper: &Path, target: &Path, rel: &Path, skip: &HashSet<PathBuf>) -> io::Result<usize> {
    let mut applied = 0;
    for entry in fs::read_dir(upper)? {
        let entry = entry?;
        let rel = rel.join(entry.file_name());
        if skip.contains(&rel) {
            continue;
        }
        let from = entry.path();
        let to = target.join(entry.file_name());
        let meta = entry.metadata()?;
        let kind = meta.file_type();
        if is_whiteout(&meta) {
            remove_any(&to)?;
            applied += 1;
        } else if kind.is_dir() {
            let opaque = is_opaque(&from);
            if opaque || !to.symlink_metadata().is_ok_and(|m| m.is_dir()) {
                remove_any(&to)?;
                fs::create_dir(&to)?;
                applied += 1;
            }
            applied += apply(&from, &to, &rel, skip)?;
            // Only after filling it: the mode may make the directory read-only.
            fs::set_permissions(&to, fs::Permissions::from_mode(meta.mode() & 0o7777))?;
        } else if kind.is_symlink() {
            remove_any(&to)?;
            symlink(fs::read_link(&from)?, &to)?;
            applied += 1;
        } else if kind.is_file() {
            if to.symlink_metadata().is_ok_and(|m| m.is_dir()) {
                fs::remove_dir_all(&to)?;
            }
            // Write next to the target, then rename over it: readers see the
            // old file or the new one, never half of each.
            let mut tmp_name = OsStr::new(".foil-commit-").to_os_string();
            tmp_name.push(entry.file_name());
            let tmp = target.join(tmp_name);
            fs::copy(&from, &tmp)?;
            fs::rename(&tmp, &to)?;
            applied += 1;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("cannot commit special file {}", from.display()),
            ));
        }
    }
    Ok(applied)
}

/// Overlayfs records a deleted file as a character device with number 0:0.
fn is_whiteout(meta: &fs::Metadata) -> bool {
    meta.file_type().is_char_device() && meta.rdev() == 0
}

/// Overlayfs marks a directory that hides everything below it (deleted and
/// recreated) with the xattr `user.overlay.opaque = "y"` (in userxattr mode).
fn is_opaque(dir: &Path) -> bool {
    let Ok(path) = c_path(dir) else { return false };
    let mut value = [0u8; 1];
    // SAFETY: valid NUL-terminated name and path; the buffer is 1 byte long.
    let n = unsafe {
        libc::lgetxattr(
            path.as_ptr(),
            c"user.overlay.opaque".as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
        )
    };
    n == 1 && value[0] == b'y'
}

/// Remove a file, symlink or directory tree; a missing path is fine.
fn remove_any(path: &Path) -> io::Result<()> {
    let result = match path.symlink_metadata() {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(err) => Err(err),
    };
    match result {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// `remove_dir_all` that also copes with directories we own but may not
/// write: overlayfs leaves `work/work` with mode 000.
fn remove_all(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
            make_writable(path);
            fs::remove_dir_all(path)
        }
        other => other,
    }
}

fn make_writable(dir: &Path) {
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            make_writable(&entry.path());
        }
    }
}

fn check_name(name: &str) -> io::Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
    if ok {
        Ok(())
    } else {
        Err(invalid(format!(
            "bad branch name {name:?}: use 1-64 letters, digits, '.', '_' or '-', not starting with '.'"
        )))
    }
}

/// When the file's inode last changed (content, name or metadata).
fn ctime(meta: &fs::Metadata) -> SystemTime {
    let since_epoch = std::time::Duration::new(meta.ctime() as u64, meta.ctime_nsec() as u32);
    SystemTime::UNIX_EPOCH + since_epoch
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}
