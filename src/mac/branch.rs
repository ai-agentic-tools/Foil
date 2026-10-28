//! macOS: workspace branches from APFS clones.
//!
//! macOS has no overlayfs and no mount namespaces, so a branch can't sit on
//! top of the workspace's own path. Instead APFS clones it: `clonefile`
//! copies a whole folder in one call without copying any data (clone and
//! original share every block until one side writes). A branch is two
//! clones, made one after the other when it is created:
//!
//! - `base/`: the workspace as it was, never written;
//! - `tree/`: where the command works (foil starts it there, in the same
//!   place relative to the workspace).
//!
//! With the base, every question is an exact comparison:
//! - **diff**: `tree` against `base`;
//! - **conflict**: a path the branch changed that also differs between the
//!   workspace and `base` now;
//! - **commit**: copy only those paths into the workspace, crash-safe (a
//!   journal first, as on Linux), through open folders that never follow
//!   symlinks (`at.rs`).
//!
//! No clock rules are needed, unlike the Linux design. The costs: cloning
//! grows with the number of files (about 185 ms per 10 000 on an Intel
//! Mac), the command works at another path, and the branch must be on the
//! same APFS volume as the workspace (`FOIL_HOME`).
//!
//! `box` records the marker of the branch's latest run (`procs.rs`); if
//! that run's foil was SIGKILLed, the next foil to take the lock stops
//! what it left running first.

use std::cell::RefCell;
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use crate::at;
pub use crate::change::{Change, ChangeKind};
use crate::isolate::Overlay;
use crate::procs::Marker;

/// A branch on disk.
pub struct Branch {
    pub name: String,
    /// The folder this branch is a clone of (canonical path).
    pub workspace: PathBuf,
    /// `$FOIL_HOME/branches/<name>`.
    dir: PathBuf,
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
    let home = std::env::var_os("HOME").ok_or_else(|| invalid("neither FOIL_HOME nor HOME is set".to_owned()))?;
    Ok(PathBuf::from(home).join(".local/share/foil"))
}

impl Branch {
    /// Open branch `name`, creating it (two clones of `workspace`) if new.
    pub fn open_or_create(name: &str, workspace: &Path) -> io::Result<Branch> {
        check_name(name)?;
        let workspace = workspace.canonicalize()?;
        if !workspace.is_dir() {
            return Err(invalid(format!("{} is not a directory", workspace.display())));
        }
        let root = home()?;
        fs::create_dir_all(root.join("branches"))?;
        let root = root.canonicalize()?;
        if root.starts_with(&workspace) || workspace.starts_with(&root) {
            let how = if root.starts_with(&workspace) { "is inside the workspace" } else { "contains the workspace" };
            return Err(invalid(format!(
                "branch storage {} {how} {}; set FOIL_HOME to a folder outside it",
                root.display(),
                workspace.display()
            )));
        }
        let branches = root.join("branches");
        // Clones can't cross volumes.
        if fs::metadata(&branches)?.dev() != fs::metadata(&workspace)?.dev() {
            return Err(invalid(format!(
                "branch storage {} is on another volume than {}; APFS clones can't cross volumes, \
                 so set FOIL_HOME to a folder on the workspace's volume",
                branches.display(),
                workspace.display()
            )));
        }
        let dir = branches.join(name);
        if !dir.exists() {
            let tmp = branches.join(format!(".new-{name}-{}", std::process::id()));
            let built = build_branch(&tmp, &workspace).and_then(|()| fs::rename(&tmp, &dir));
            if let Err(err) = built {
                let _ = remove_all(&tmp);
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
            io::ErrorKind::NotFound => io::Error::new(io::ErrorKind::NotFound, format!("no branch named {name}")),
            _ => err,
        })?;
        let dir = dir.canonicalize().unwrap_or(dir);
        Ok(Branch { name: name.to_owned(), workspace: PathBuf::from(OsStr::from_bytes(&raw)), dir })
    }

    /// Every branch, sorted by name.
    pub fn list() -> io::Result<Vec<Branch>> {
        let entries = match fs::read_dir(home()?.join("branches")) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };
        let mut branches: Vec<Branch> =
            entries.filter_map(|entry| Branch::open(entry.ok()?.file_name().to_str()?).ok()).collect();
        branches.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(branches)
    }

    /// Take the branch's lock (see the Linux version). Holding it means the
    /// foil of any earlier run is gone; whatever that run left running is
    /// stopped first.
    pub fn lock(&self) -> io::Result<File> {
        let file = File::create(self.dir.join("lock"))?;
        // SAFETY: flock on a descriptor we own; released when the file closes.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == -1 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                return Err(io::Error::new(err.kind(), format!("branch {} is in use by another foil", self.name)));
            }
            return Err(err);
        }
        if let Ok(raw) = fs::read(self.dir.join("box")) {
            let marker = Marker::from_denied(Path::new(OsStr::from_bytes(&raw)));
            if marker.exists() {
                marker.stop_all(std::time::Duration::ZERO);
                marker.remove();
            }
        }
        Ok(file)
    }

    fn base(&self) -> PathBuf {
        self.dir.join("base")
    }

    fn tree(&self) -> PathBuf {
        self.dir.join("tree")
    }

    /// The branch as a run sees it.
    pub fn overlay(&self) -> io::Result<Overlay> {
        Ok(Overlay { workspace: self.workspace.clone(), tree: self.tree(), store: home()?.canonicalize()?, dir: self.dir.clone() })
    }

    /// Remember the marker of the run that now holds the lock.
    pub fn record_box(&self, marker: &Path) -> io::Result<()> {
        fs::write(self.dir.join("box"), marker.as_os_str().as_bytes())
    }

    /// Stop (or continue) every process of the run using this branch: all
    /// of them carry its sandbox marker.
    pub fn freeze(&self, frozen: bool) -> io::Result<()> {
        let raw = fs::read(self.dir.join("box")).map_err(|_| io::Error::other(format!("branch {} has no running box to pause", self.name)))?;
        let marker = Marker::from_denied(Path::new(std::ffi::OsStr::from_bytes(&raw)));
        let signal = if frozen { libc::SIGSTOP } else { libc::SIGCONT };
        for pid in marker.members() {
            // SAFETY: signals a process of this run.
            unsafe { libc::kill(pid, signal) };
        }
        Ok(())
    }

    /// The moment the clones were complete: a tree file whose inode changed
    /// after it may have been changed by a run.
    fn created(&self) -> io::Result<SystemTime> {
        fs::symlink_metadata(self.dir.join("created"))?.modified()
    }

    /// The changes worth reviewing and committing, and how many were left
    /// out as ignored by git (unless `all`).
    pub fn changes(&self, all: bool) -> io::Result<(Vec<Change>, usize)> {
        let access = self.reading_access()?;
        self.changes_with(all, &access)
    }

    fn changes_with(&self, all: bool, access: &Access) -> io::Result<(Vec<Change>, usize)> {
        let changes = self.diff_with(access)?;
        if all {
            return Ok((changes, 0));
        }
        let tree = self.tree();
        let ignored = crate::gitignore::ignored(&self.workspace, &changes, |p| tree.join(p).symlink_metadata().is_ok_and(|m| m.is_dir()));
        let total = changes.len();
        let kept: Vec<Change> = changes.into_iter().filter(|c| !ignored.contains(&c.path)).collect();
        let hidden = total - kept.len();
        Ok((kept, hidden))
    }

    /// Every change, in path order (a folder before what is in it).
    pub fn diff(&self) -> io::Result<Vec<Change>> {
        let access = self.reading_access()?;
        self.diff_with(&access)
    }

    fn diff_with(&self, access: &Access) -> io::Result<Vec<Change>> {
        let walk = Walk { base: self.base(), tree: self.tree(), created: self.created()?, access };
        let mut out = Vec::new();
        walk.dir(Path::new(""), &mut out)?;
        Ok(out)
    }

    /// Access for reading the branch: folders and files a run closed may be
    /// opened for the moment only while no run uses the branch.
    fn reading_access(&self) -> io::Result<Access> {
        match self.lock() {
            Ok(lock) => Ok(Access::new(Some(lock), &self.name)),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Ok(Access::new(None, &self.name)),
            Err(err) => Err(err),
        }
    }

    /// Changed paths that also changed in the workspace since the branch
    /// was made: exactly those where the workspace no longer matches base.
    pub fn conflicts(&self, all: bool) -> io::Result<Vec<PathBuf>> {
        let access = self.reading_access()?;
        self.conflicts_with(all, &access)
    }

    fn conflicts_with(&self, all: bool, access: &Access) -> io::Result<Vec<PathBuf>> {
        self.conflicts_in(&self.changes_with(all, access)?.0, access)
    }

    /// Which of `changes` conflict (see `conflicts`).
    fn conflicts_in(&self, changes: &[Change], access: &Access) -> io::Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        for change in changes.iter().cloned() {
            let ws = self.workspace.join(&change.path);
            let base = self.base().join(&change.path);
            let deep = matches!(change.kind, ChangeKind::Deleted | ChangeKind::Replaced);
            // A workspace folder foil can't read counts as changed: the safe side.
            if !same(&ws, &base, deep, access).unwrap_or(false) {
                out.push(change.path);
            }
        }
        Ok(out)
    }

    /// Apply the branch to its workspace, then delete it (see the module
    /// comment). Refuses conflicting changes unless `force`; leaves out
    /// ignored paths unless `all`. Returns the number of changes applied.
    pub fn commit(self, force: bool, all: bool) -> io::Result<usize> {
        let lock = self.lock()?;
        let _workspace = self.workspace_lock()?;
        let access = Access::new(Some(lock), &self.name);
        let journal = self.dir.join("committing");
        let plan = match fs::read(&journal) {
            Ok(raw) => read_journal(&raw)?,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let plan = self.changes_with(all, &access)?.0;
                if !force {
                    let conflicts = self.conflicts_in(&plan, &access)?;
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
                // Sockets and devices can't be committed: refuse before anything changes.
                for change in plan.iter().filter(|c| c.kind != ChangeKind::Deleted) {
                    let t = self.tree().join(&change.path).symlink_metadata()?.file_type();
                    if t.is_socket() || t.is_block_device() || t.is_char_device() {
                        return Err(invalid(format!(
                            "{} is a special file (a socket or device); foil can't commit it: remove it in the branch, or abort",
                            change.path.display()
                        )));
                    }
                }
                write_durably(&journal, &journal_bytes(&plan))?;
                plan
            }
            Err(err) => return Err(err),
        };
        let mut commit = Committer { tree: self.tree(), access: &access, dirs: Vec::new(), links: Vec::new(), modes: Vec::new() };
        commit.dirs.push((PathBuf::new(), at::open_dir(&self.workspace)?));
        let mut applied = 0;
        for change in &plan {
            let step = commit.apply(change).and_then(|()| crash_point());
            step.map_err(|err| {
                io::Error::new(
                    err.kind(),
                    format!(
                        "commit of branch {} stopped half-way at {}: {err}\nthe workspace has some of its changes; \
                         fix the cause and run `foil commit {}` again to finish",
                        self.name,
                        change.path.display(),
                        self.name
                    ),
                )
            })?;
            applied += 1;
        }
        commit.finish()?;
        drop(commit);
        // SAFETY: sync has no preconditions.
        unsafe { libc::sync() };
        drop(access);
        discard(&self.dir)?;
        Ok(applied)
    }

    /// Whether a commit of this branch started and did not finish.
    pub fn commit_interrupted(&self) -> bool {
        self.dir.join("committing").exists()
    }

    /// Commits into one workspace take turns.
    fn workspace_lock(&self) -> io::Result<File> {
        let dir = home()?.join("locks");
        fs::create_dir_all(&dir)?;
        let file = File::create(dir.join(format!("{:016x}.lock", fnv1a(self.workspace.as_os_str().as_bytes()))))?;
        // SAFETY: flock on a descriptor we own; blocks until the other commit is done.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(file)
    }

    /// The changes as a unified diff (`diff -u` of base and tree).
    pub fn write_patch(&self, out: &mut impl Write, all: bool) -> io::Result<()> {
        let access = self.reading_access()?;
        for change in self.changes_with(all, &access)?.0 {
            let old = self.base().join(&change.path);
            let new = self.tree().join(&change.path);
            let is_file = |p: &Path| p.symlink_metadata().is_ok_and(|m| m.is_file());
            let (old_file, new_file) = match change.kind {
                ChangeKind::Added => (None, Some(&new)),
                ChangeKind::Deleted => (Some(&old), None),
                ChangeKind::Modified | ChangeKind::Replaced => (Some(&old), Some(&new)),
            };
            if !(old_file.is_none_or(|p| is_file(p)) && new_file.is_none_or(|p| is_file(p))) {
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
            if output.status.code() == Some(2) {
                return Err(io::Error::other(format!("diff: {}", String::from_utf8_lossy(&output.stderr).trim())));
            }
            out.write_all(&output.stdout)?;
        }
        Ok(())
    }

    /// Delete the branch. Refuses after an interrupted commit unless `force`.
    pub fn abort(self, force: bool) -> io::Result<()> {
        let _lock = self.lock()?;
        if self.commit_interrupted() && !force {
            return Err(io::Error::other(format!(
                "a commit of branch {} was interrupted, so {} has some of its changes; \
                 run `foil commit {}` to finish it, or `foil abort --force {}` to keep the workspace as it is",
                self.name,
                self.workspace.display(),
                self.name,
                self.name
            )));
        }
        discard(&self.dir)
    }
}

/// Clone `workspace` twice into a new branch folder `dir`.
fn build_branch(dir: &Path, workspace: &Path) -> io::Result<()> {
    fs::create_dir(dir)?;
    fs::write(dir.join("workspace"), workspace.as_os_str().as_bytes())?;
    // The two clones at once: each is mostly waiting on the file system.
    let base = dir.join("base");
    let other = std::thread::spawn({
        let workspace = workspace.to_path_buf();
        move || clone(&workspace, &base)
    });
    let tree = clone(workspace, &dir.join("tree"));
    other.join().map_err(|_| io::Error::other("cloning thread panicked"))??;
    tree?;
    // Written after the clones: any later change to a tree file changes
    // its inode time past this.
    fs::write(dir.join("created"), b"")?;
    Ok(())
}

/// `clonefile(from, to)`: a copy-on-write copy of a whole folder.
fn clone(from: &Path, to: &Path) -> io::Result<()> {
    let (f, t) = (cpath(from)?, cpath(to)?);
    // SAFETY: valid C strings.
    if unsafe { libc::clonefile(f.as_ptr(), t.as_ptr(), 0) } == -1 {
        let err = io::Error::last_os_error();
        let hint = match err.raw_os_error() {
            Some(libc::ENOTSUP) => " (the volume is not APFS)",
            Some(libc::EXDEV) => " (another volume; set FOIL_HOME on the workspace's volume)",
            _ => "",
        };
        return Err(io::Error::new(err.kind(), format!("cloning {}: {err}{hint}", from.display())));
    }
    Ok(())
}

/// Read access to what a run closed in the branch (`chmod 000`), and to
/// closed folders the base cloned from the workspace: such a folder or file
/// is opened for the moment (owner bits added) and closed again when the
/// access ends. Only while foil holds the branch lock, so a running
/// command never sees its folder modes change.
struct Access {
    lock: Option<File>,
    name: String,
    opened: RefCell<Vec<(PathBuf, u32)>>,
}

impl Access {
    fn new(lock: Option<File>, name: &str) -> Access {
        Access { lock, name: name.to_owned(), opened: RefCell::new(Vec::new()) }
    }

    /// Make `path` readable (and, for a folder, listable) by its owner.
    fn readable(&self, path: &Path) -> io::Result<()> {
        let m = path.symlink_metadata()?;
        if m.file_type().is_symlink() {
            return Ok(());
        }
        let need = if m.is_dir() { 0o500 } else { 0o400 };
        let mode = m.permissions().mode() & 0o7777;
        if mode & need == need {
            return Ok(());
        }
        if self.lock.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("branch {} is in use by a run that closed {}; try again when the run has ended", self.name, path.display()),
            ));
        }
        fs::set_permissions(path, fs::Permissions::from_mode(mode | need))?;
        self.opened.borrow_mut().push((path.to_path_buf(), mode));
        Ok(())
    }

    /// The mode `path` had before this access opened it.
    fn mode(&self, path: &Path, meta: &fs::Metadata) -> u32 {
        self.opened.borrow().iter().rev().find(|(p, _)| p == path).map_or(meta.permissions().mode() & 0o7777, |(_, m)| *m)
    }
}

impl Drop for Access {
    fn drop(&mut self) {
        for (path, mode) in self.opened.borrow().iter().rev() {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(*mode));
        }
    }
}

/// One comparison of `base` and `tree`.
struct Walk<'a> {
    base: PathBuf,
    tree: PathBuf,
    created: SystemTime,
    access: &'a Access,
}

impl Walk<'_> {
    /// Compare `base/rel` and `tree/rel`, appending what changed to `out`.
    fn dir(&self, rel: &Path, out: &mut Vec<Change>) -> io::Result<()> {
        let mut names: Vec<OsString> = Vec::new();
        for dir in [self.base.join(rel), self.tree.join(rel)] {
            if dir.symlink_metadata().is_ok_and(|m| m.is_dir()) {
                self.access.readable(&dir)?;
                for entry in fs::read_dir(&dir)? {
                    names.push(entry?.file_name());
                }
            }
        }
        names.sort();
        names.dedup();
        for name in names {
            let path = rel.join(&name);
            let (bp, tp) = (self.base.join(&path), self.tree.join(&path));
            match (bp.symlink_metadata().ok(), tp.symlink_metadata().ok()) {
                (None, Some(t)) => {
                    out.push(Change { kind: ChangeKind::Added, path: path.clone() });
                    if t.is_dir() {
                        self.added_below(&path, out)?;
                    }
                }
                (Some(_), None) => out.push(Change { kind: ChangeKind::Deleted, path }),
                (Some(b), Some(t)) => {
                    if kind_of(&b) != kind_of(&t) {
                        out.push(Change { kind: ChangeKind::Replaced, path: path.clone() });
                        if t.is_dir() {
                            self.added_below(&path, out)?;
                        }
                    } else if t.is_dir() {
                        let touched = ctime(&t) >= self.created;
                        let differs = b.mode() != t.mode() || (touched && xattrs(&bp) != xattrs(&tp));
                        if differs {
                            out.push(Change { kind: ChangeKind::Modified, path: path.clone() });
                        }
                        self.dir(&path, out)?;
                    } else {
                        let touched = ctime(&t) >= self.created;
                        // A new or removed hard link changes only the link count.
                        let differs = b.mode() != t.mode()
                            || b.len() != t.len()
                            || b.nlink() != t.nlink()
                            || (touched && (!self.same_contents(&bp, &tp, &t)? || xattrs(&bp) != xattrs(&tp)));
                        if differs {
                            out.push(Change { kind: ChangeKind::Modified, path });
                        }
                    }
                }
                (None, None) => {}
            }
        }
        Ok(())
    }

    /// Everything below a folder that is new in the branch.
    fn added_below(&self, rel: &Path, out: &mut Vec<Change>) -> io::Result<()> {
        let dir = self.tree.join(rel);
        self.access.readable(&dir)?;
        let mut names: Vec<OsString> = fs::read_dir(&dir)?.map(|e| e.map(|e| e.file_name())).collect::<Result<_, _>>()?;
        names.sort();
        for name in names {
            let path = rel.join(name);
            let is_dir = self.tree.join(&path).symlink_metadata()?.is_dir();
            out.push(Change { kind: ChangeKind::Added, path: path.clone() });
            if is_dir {
                self.added_below(&path, out)?;
            }
        }
        Ok(())
    }

    fn same_contents(&self, a: &Path, b: &Path, meta: &fs::Metadata) -> io::Result<bool> {
        if meta.is_file() {
            self.access.readable(a)?;
            self.access.readable(b)?;
        }
        same_contents(a, b, meta)
    }
}

#[derive(PartialEq)]
enum Kind {
    File,
    Dir,
    Symlink,
    Other,
}

fn kind_of(m: &fs::Metadata) -> Kind {
    let t = m.file_type();
    if t.is_symlink() {
        Kind::Symlink
    } else if t.is_dir() {
        Kind::Dir
    } else if t.is_file() {
        Kind::File
    } else {
        Kind::Other
    }
}

/// Whether two files (or symlinks) of the same kind hold the same bytes.
fn same_contents(a: &Path, b: &Path, meta: &fs::Metadata) -> io::Result<bool> {
    if meta.file_type().is_symlink() {
        return Ok(fs::read_link(a)? == fs::read_link(b)?);
    }
    if !meta.is_file() {
        return Ok(true);
    }
    let (mut fa, mut fb) = (File::open(a)?, File::open(b)?);
    let (mut ba, mut bb) = (vec![0u8; 64 * 1024], vec![0u8; 64 * 1024]);
    loop {
        let na = read_full(&mut fa, &mut ba)?;
        let nb = read_full(&mut fb, &mut bb)?;
        if na != nb || ba[..na] != bb[..nb] {
            return Ok(false);
        }
        if na == 0 {
            return Ok(true);
        }
    }
}

fn read_full(f: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// Whether workspace path `ws` and base path `base` are the same thing:
/// kind, mode, extended attributes, contents (and, for folders with `deep`,
/// everything inside). Both missing counts as same.
fn same(ws: &Path, base: &Path, deep: bool, access: &Access) -> io::Result<bool> {
    let (ma, mb) = match (ws.symlink_metadata().ok(), base.symlink_metadata().ok()) {
        (None, None) => return Ok(true),
        (Some(ma), Some(mb)) => (ma, mb),
        _ => return Ok(false),
    };
    if kind_of(&ma) != kind_of(&mb) || ma.mode() != access.mode(base, &mb) | (mb.mode() & !0o7777) || xattrs(ws) != xattrs(base) {
        return Ok(false);
    }
    if !ma.is_dir() {
        if mb.is_file() {
            access.readable(base)?;
        }
        return Ok(ma.len() == mb.len() && same_contents(ws, base, &ma)?);
    }
    if !deep {
        return Ok(true);
    }
    access.readable(base)?;
    let list = |d: &Path| -> io::Result<Vec<OsString>> {
        let mut v: Vec<OsString> = fs::read_dir(d)?.map(|e| e.map(|e| e.file_name())).collect::<Result<_, _>>()?;
        v.sort();
        Ok(v)
    };
    let (la, lb) = (list(ws)?, list(base)?);
    if la != lb {
        return Ok(false);
    }
    for name in la {
        if !same(&ws.join(&name), &base.join(&name), true, access)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Extended attributes of `path` (not following a symlink), sorted. macOS's
/// own bookkeeping (where a file came from) is left out.
fn xattrs(path: &Path) -> Vec<(Vec<u8>, Vec<u8>)> {
    let Ok(c) = cpath(path) else { return Vec::new() };
    let mut names = vec![0u8; 16 * 1024];
    // SAFETY: valid path; buffer length passed.
    let n = unsafe { libc::listxattr(c.as_ptr(), names.as_mut_ptr().cast(), names.len(), libc::XATTR_NOFOLLOW) };
    if n <= 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for name in names[..n as usize].split(|&b| b == 0).filter(|n| !n.is_empty() && !is_bookkeeping(n)) {
        let Ok(cn) = CString::new(name) else { continue };
        let mut value = vec![0u8; 64 * 1024];
        // SAFETY: valid path and name; buffer length passed.
        let len = unsafe { libc::getxattr(c.as_ptr(), cn.as_ptr(), value.as_mut_ptr().cast(), value.len(), 0, libc::XATTR_NOFOLLOW) };
        if len >= 0 {
            out.push((name.to_vec(), value[..len as usize].to_vec()));
        }
    }
    out.sort();
    out
}

fn is_bookkeeping(name: &[u8]) -> bool {
    name == b"com.apple.provenance" || name == b"com.apple.quarantine"
}

/// Give the open folder `to` exactly the extended attributes of `from`.
fn sync_xattrs(from: &Path, to: RawFd) -> io::Result<()> {
    let want = xattrs(from);
    let mut names = vec![0u8; 16 * 1024];
    // SAFETY: valid descriptor; buffer length passed.
    let n = unsafe { libc::flistxattr(to, names.as_mut_ptr().cast(), names.len(), 0) };
    for name in names[..n.max(0) as usize].split(|&b| b == 0).filter(|n| !n.is_empty() && !is_bookkeeping(n)) {
        if !want.iter().any(|(w, _)| w == name) {
            let cn = CString::new(name).map_err(|_| invalid("NUL in an attribute name".to_owned()))?;
            // SAFETY: valid descriptor and name.
            unsafe { libc::fremovexattr(to, cn.as_ptr(), 0) };
        }
    }
    for (name, value) in want {
        let cn = CString::new(name).map_err(|_| invalid("NUL in an attribute name".to_owned()))?;
        at::fsetxattr(to, &cn, &value)?;
    }
    Ok(())
}

/// One commit in progress: open folders of the workspace (kept open, so a
/// folder moved or swapped for a symlink mid-commit can't redirect later
/// writes), files already written under another name (hard links), and
/// folder modes to set at the end.
struct Committer<'a> {
    tree: PathBuf,
    access: &'a Access,
    dirs: Vec<(PathBuf, OwnedFd)>,
    links: Vec<((u64, u64), PathBuf)>,
    modes: Vec<(PathBuf, u32)>,
}

impl Committer<'_> {
    /// The open workspace folder `rel`, opened one component at a time from
    /// the root, never following a symlink.
    fn dir(&mut self, rel: &Path) -> io::Result<RawFd> {
        if let Some((_, fd)) = self.dirs.iter().find(|(p, _)| p == rel) {
            return Ok(fd.as_raw_fd());
        }
        let parent = self.dir(rel.parent().unwrap_or(Path::new("")))?;
        let name = at::cname(rel.file_name().ok_or_else(|| invalid("empty path".to_owned()))?)?;
        let fd = at::open_dir_at(parent, &name)?;
        let raw = fd.as_raw_fd();
        self.dirs.push((rel.to_path_buf(), fd));
        Ok(raw)
    }

    /// Forget open folders at or below `rel` (it is about to be removed).
    fn forget(&mut self, rel: &Path) {
        self.dirs.retain(|(p, _)| p.as_os_str().is_empty() || !p.starts_with(rel));
    }

    /// The open workspace folder `rel`, made writable for the commit if it
    /// is not (a read-only folder whose contents changed): its mode is put
    /// back at the end.
    fn writable_dir(&mut self, rel: &Path) -> io::Result<RawFd> {
        let fd = self.dir(rel)?;
        // SAFETY: fstat fills `st` for a valid descriptor.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut st) } == -1 {
            return Err(io::Error::last_os_error());
        }
        let mode = u32::from(st.st_mode) & 0o7777;
        // The mode to end with is the branch's (its tree), not the one seen
        // now: after a crash mid-commit, that may be the one foil set.
        let tree = self.tree.join(rel);
        let want = tree.symlink_metadata().map_or(mode, |m| self.access.mode(&tree, &m));
        if (mode & 0o300 != 0o300 || mode != want) && !self.modes.iter().any(|(p, _)| p == rel) {
            self.modes.push((rel.to_path_buf(), want));
        }
        if mode & 0o300 != 0o300 {
            at::fchmod(fd, mode | 0o700)?;
        }
        Ok(fd)
    }

    fn apply(&mut self, change: &Change) -> io::Result<()> {
        let parent_rel = change.path.parent().unwrap_or(Path::new("")).to_path_buf();
        let name = at::cname(change.path.file_name().ok_or_else(|| invalid("empty path".to_owned()))?)?;
        let d = self.writable_dir(&parent_rel)?;
        if change.kind == ChangeKind::Deleted {
            self.forget(&change.path);
            return at::remove_at(d, &name);
        }
        let from = self.tree.join(&change.path);
        let meta = from.symlink_metadata()?;
        let existing = at::stat_at(d, &name)?;
        let mode = self.access.mode(&from, &meta);
        let kind = meta.file_type();
        if kind.is_dir() {
            let keep = existing.is_some_and(|st| at::is_dir(&st)) && change.kind != ChangeKind::Replaced;
            if !keep {
                self.forget(&change.path);
                at::remove_at(d, &name)?;
                at::mkdir_at(d, &name)?;
            }
            let sub = self.dir(&change.path)?;
            at::fchmod(sub, 0o700)?;
            sync_xattrs(&from, sub)?;
            self.modes.retain(|(p, _)| *p != change.path);
            self.modes.push((change.path.clone(), mode));
        } else if kind.is_symlink() {
            at::remove_at(d, &name)?;
            at::symlink_at(fs::read_link(&from)?.as_os_str(), d, &name)?;
        } else if kind.is_file() {
            if existing.is_some_and(|st| at::is_dir(&st)) {
                self.forget(&change.path);
                at::remove_at(d, &name)?;
            }
            let tmp = c".foil-commit.tmp";
            at::remove_at(d, tmp)?;
            let id = (meta.dev(), meta.ino());
            let first = (meta.nlink() > 1).then(|| self.links.iter().find(|(i, _)| *i == id).map(|(_, p)| p.clone())).flatten();
            match first {
                // Another name of a file already written: link to it.
                Some(other) => {
                    let od = self.dir(other.parent().unwrap_or(Path::new("")))?;
                    let on = at::cname(other.file_name().unwrap_or_default())?;
                    at::link_at(od, &on, d, tmp)?;
                }
                None => {
                    self.access.readable(&from)?;
                    // Written next to the target, then renamed over it. A
                    // clone of the branch's file where possible (no data
                    // copied, extended attributes kept), else a copy.
                    let src = open_nofollow(&from)?;
                    // SAFETY: valid descriptors and name.
                    if unsafe { libc::fclonefileat(src.as_raw_fd(), d, tmp.as_ptr(), 0) } == -1 {
                        let out = at::create_at(d, tmp)?;
                        let flags = libc::COPYFILE_DATA | libc::COPYFILE_XATTR;
                        // SAFETY: two open descriptors; no state object.
                        if unsafe { libc::fcopyfile(src.as_raw_fd(), out.as_raw_fd(), std::ptr::null_mut(), flags) } == -1 {
                            return Err(io::Error::last_os_error());
                        }
                    }
                    let out = at::open_file_at(d, tmp)?;
                    at::fchmod(out.as_raw_fd(), mode)?;
                    if meta.nlink() > 1 {
                        self.links.push((id, change.path.clone()));
                    }
                }
            }
            at::rename_at(d, tmp, &name)?;
        } else if kind.is_fifo() {
            at::remove_at(d, &name)?;
            at::mkfifo_at(d, &name, mode)?;
        } else {
            return Err(invalid(format!("{} is a special file; foil can't commit it", change.path.display())));
        }
        Ok(())
    }

    /// Folder modes last, deepest first, so read-only folders could be filled.
    fn finish(&mut self) -> io::Result<()> {
        let modes = std::mem::take(&mut self.modes);
        for (path, mode) in modes.iter().rev() {
            let fd = self.dir(path)?;
            at::fchmod(fd, *mode)?;
        }
        Ok(())
    }
}

/// Open a file of the branch for reading, refusing a symlink.
fn open_nofollow(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path)
}

/// The journal: a version line, then one `<letter><path>` per change.
fn journal_bytes(plan: &[Change]) -> Vec<u8> {
    let mut out = b"foil-commit-mac 1\n".to_vec();
    for c in plan {
        out.push(c.kind.letter() as u8);
        out.extend_from_slice(c.path.as_os_str().as_bytes());
        out.push(0);
    }
    out
}

fn read_journal(raw: &[u8]) -> io::Result<Vec<Change>> {
    let rest = raw
        .strip_prefix(b"foil-commit-mac 1\n")
        .ok_or_else(|| invalid("unreadable commit journal; abort the branch with --force".to_owned()))?;
    rest.split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| {
            let kind = match p[0] {
                b'A' => ChangeKind::Added,
                b'M' => ChangeKind::Modified,
                b'D' => ChangeKind::Deleted,
                b'R' => ChangeKind::Replaced,
                _ => return Err(invalid("unreadable commit journal".to_owned())),
            };
            Ok(Change { kind, path: PathBuf::from(OsStr::from_bytes(&p[1..])) })
        })
        .collect()
}

fn write_durably(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut file = File::create(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&tmp, path)?;
    File::open(path.parent().unwrap_or(Path::new("/")))?.sync_all()
}

/// Test builds only: the same crash and failure hooks as on Linux.
fn crash_point() -> io::Result<()> {
    #[cfg(debug_assertions)]
    {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static DONE: AtomicUsize = AtomicUsize::new(0);
        let done = DONE.fetch_add(1, Ordering::SeqCst) + 1;
        // FOIL_TEST_PAUSE_AFTER=N with FOIL_TEST_PAUSE_FILE: after the Nth
        // change, wait until that file exists (lets a test act mid-commit).
        let pause = std::env::var("FOIL_TEST_PAUSE_AFTER").ok().and_then(|v| v.parse::<usize>().ok());
        if let (Some(n), Some(file)) = (pause, std::env::var_os("FOIL_TEST_PAUSE_FILE")) {
            if done == n {
                let _ = std::fs::write(Path::new(&file).with_extension("reached"), b"");
                for _ in 0..3000 {
                    if Path::new(&file).exists() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
        }
        if std::env::var("FOIL_TEST_FAIL_AFTER").ok().and_then(|v| v.parse::<usize>().ok()) == Some(done) {
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        if std::env::var("FOIL_TEST_CRASH_AFTER").ok().and_then(|v| v.parse::<usize>().ok()).is_some_and(|n| done >= n) {
            // SAFETY: sends a signal to ourselves.
            unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
        }
    }
    Ok(())
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3))
}

/// Remove a branch folder without making the caller wait: two clones of a
/// large workspace take seconds to delete. It is renamed into
/// `$FOIL_HOME/trash` (instant; the branch is gone from that moment) and
/// deleted there by a detached `foil __trash`.
fn discard(dir: &Path) -> io::Result<()> {
    let trash = home()?.join("trash");
    fs::create_dir_all(&trash)?;
    let nanos = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let name = format!("{}-{}-{nanos}", dir.file_name().unwrap_or_default().to_string_lossy(), std::process::id());
    let target = trash.join(name);
    if fs::rename(dir, &target).is_err() {
        return remove_all(dir);
    }
    let spawned = std::env::current_exe().and_then(|exe| {
        use std::os::unix::process::CommandExt;
        let mut cmd = Command::new(exe);
        cmd.arg("__trash").arg(&trash).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        // SAFETY: setsid is async-signal-safe; the remover outlives this foil quietly.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        cmd.spawn()
    });
    if spawned.is_err() {
        return remove_all(&target);
    }
    Ok(())
}

/// `foil __trash DIR`: delete everything in foil's trash folder.
pub fn empty_trash(dir: &Path) {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let _ = remove_all(&entry.path());
    }
}

fn remove_all(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
            make_writable(path);
            fs::remove_dir_all(path)
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
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
        Err(invalid(format!("bad branch name {name:?}: use 1-64 letters, digits, '.', '_' or '-', not starting with '.'")))
    }
}

fn ctime(meta: &fs::Metadata) -> SystemTime {
    SystemTime::UNIX_EPOCH + std::time::Duration::new(meta.ctime() as u64, meta.ctime_nsec() as u32)
}

fn cpath(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| invalid("NUL in path".to_owned()))
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

#[cfg(test)]
mod tests {
    use super::{journal_bytes, read_journal, Change, ChangeKind};
    use std::path::PathBuf;

    #[test]
    fn journal_round_trip() {
        let plan = vec![
            Change { kind: ChangeKind::Added, path: PathBuf::from("a/b") },
            Change { kind: ChangeKind::Deleted, path: PathBuf::from("c") },
            Change { kind: ChangeKind::Replaced, path: PathBuf::from("d e") },
        ];
        assert_eq!(read_journal(&journal_bytes(&plan)).unwrap(), plan);
    }
}
