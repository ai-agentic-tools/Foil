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

use std::collections::{HashMap, HashSet};
use std::ffi::{CString, OsStr};
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use crate::at;
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

pub use crate::change::{Change, ChangeKind};

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

    /// Freeze (or thaw) every process of the run using this branch, through
    /// its cgroup box: frozen processes keep their memory but get no CPU.
    pub fn freeze(&self, frozen: bool) -> io::Result<()> {
        let raw = fs::read(self.dir.join("box")).map_err(|_| io::Error::other(format!("branch {} has no running box to pause", self.name)))?;
        let freeze = Path::new(OsStr::from_bytes(&raw)).join("cgroup.freeze");
        fs::write(&freeze, if frozen { "1" } else { "0" })
            .map_err(|err| io::Error::new(err.kind(), format!("can't pause branch {} ({}): {err}", self.name, freeze.display())))
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

    /// Which of `changes` git ignores in the workspace (see `gitignore.rs`).
    fn ignored(&self, changes: &[Change]) -> HashSet<PathBuf> {
        let upper = self.upper();
        crate::gitignore::ignored(&self.workspace, changes, |p| upper.join(p).symlink_metadata().is_ok_and(|m| m.is_dir()))
    }

    /// Every change this branch would make to its workspace, in path order.
    pub fn diff(&self) -> io::Result<Vec<Change>> {
        let (_lock, _access) = self.open_for_reading()?;
        let mut changes = Vec::new();
        self.collect_changes(Path::new(""), &mut changes).map_err(|err| self.explain(err))?;
        Ok(changes)
    }

    /// Get read access to the whole upper layer for a diff or patch. Folders
    /// the command made unreadable are opened (see `Access`) only while no
    /// run uses the branch, so a running command never sees its files
    /// change mode under it. Inside a commit, which already holds the lock
    /// and its own `Access`, there is nothing to do.
    fn open_for_reading(&self) -> io::Result<(Option<File>, Access)> {
        if ACCESS_OPEN.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok((None, Access::open(&self.upper(), &self.dir.join("modes"), false)?));
        }
        let lock = self.lock().ok();
        let may_open = lock.is_some();
        Ok((lock, Access::open(&self.upper(), &self.dir.join("modes"), may_open)?))
    }

    /// A clearer error for a folder we could not read in a branch in use.
    fn explain(&self, err: io::Error) -> io::Error {
        if err.kind() != io::ErrorKind::PermissionDenied {
            return err;
        }
        io::Error::new(
            err.kind(),
            format!(
                "branch {} is in use, and its command made a folder unreadable; \
                 run foil diff again when the run has ended",
                self.name
            ),
        )
    }

    fn collect_changes(&self, rel: &Path, out: &mut Vec<Change>) -> io::Result<()> {
        let mut entries: Vec<_> = fs::read_dir(self.upper().join(rel))?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let rel = rel.join(entry.file_name());
            let upper = entry.path();
            // DirEntry::metadata does not follow symlinks.
            let meta = entry.metadata()?;
            let before = self.workspace.join(&rel).symlink_metadata().ok();
            let existed = before.is_some();
            // A folder that became a file or link, or the other way round.
            let type_changed = before.as_ref().is_some_and(|b| b.is_dir() != meta.is_dir());
            let kind = if is_whiteout(&meta) {
                Some(ChangeKind::Deleted)
            } else if type_changed {
                Some(ChangeKind::Replaced)
            } else if meta.is_dir() {
                // A directory is copied up whenever something inside it
                // changes; by itself that is not a change worth listing.
                match (existed, is_opaque(&upper)) {
                    (false, _) => Some(ChangeKind::Added),
                    (true, true) => Some(ChangeKind::Replaced),
                    // Unless its own mode or extended attributes changed:
                    // commit applies those, so diff must show them.
                    (true, false) => {
                        let old = before.as_ref().expect("existed");
                        let mode_changed = real_mode(&meta) != old.mode() & 0o7777;
                        let xattrs_changed = user_xattrs(&upper) != user_xattrs(&self.workspace.join(&rel));
                        (mode_changed || xattrs_changed).then_some(ChangeKind::Modified)
                    }
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
    ///
    /// "At or after" the branch's creation, not strictly after: the kernel
    /// stamps most changes with a coarse clock that only advances every
    /// tick, so a change made right after the branch can carry the very
    /// same time. The branch's own time is an exact one (see
    /// `build_branch`), so changes made before it are older.
    pub fn conflicts(&self, all: bool) -> io::Result<Vec<PathBuf>> {
        let created = self.created()?;
        let mut conflicts = Vec::new();
        for change in self.changes(all)?.0 {
            let changed_since = match self.workspace.join(&change.path).symlink_metadata() {
                Ok(meta) => ctime(&meta) >= created,
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
    ///
    /// Crash-safe: the decision what to apply is first written to a journal
    /// (`committing`) in the branch, then every change is applied (each file
    /// replaced atomically), the workspace is synced to disk, and only then
    /// is the branch deleted. If foil dies, the machine loses power or a
    /// write fails half-way, the branch and its journal remain, and running
    /// `foil commit` again finishes the same commit: applying a change twice
    /// gives the same result.
    pub fn commit(self, force: bool, all: bool) -> io::Result<usize> {
        // Held until the branch is gone, so no run can start in between.
        let _lock = self.lock()?;
        // One commit at a time per workspace: otherwise two branches could
        // both pass the conflict check and then overwrite each other.
        let _workspace = self.workspace_lock()?;
        let journal = self.dir.join("committing");
        // The command may have left folders we can't list or files we can't
        // read in the branch; open them while we work, with the real modes kept.
        let access = Access::open(&self.upper(), &self.dir.join("modes"), true)?;
        let skip = match fs::read(&journal) {
            // An interrupted commit: finish it as it was decided.
            Ok(raw) => read_journal(&raw)?,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                check_committable(&self.upper())?;
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
                // The branch's files must be on disk before the journal says
                // to copy them, and the journal before the first change.
                sync_fs(&self.upper())?;
                write_durably(&journal, &journal_bytes(&skip))?;
                skip
            }
            Err(err) => return Err(err),
        };
        let mut links = HashMap::new();
        let root = at::open_dir(&self.workspace)?;
        let applied = apply(&self.upper(), &root, Path::new(""), &skip, &mut links, &access).map_err(|err| {
            io::Error::new(
                err.kind(),
                format!(
                    "commit of branch {} stopped half-way: {err}\nthe workspace has some of its changes; \
                     fix the cause and run `foil commit {}` again to finish",
                    self.name, self.name
                ),
            )
        })?;
        sync_fs(&self.workspace)?;
        drop(access);
        remove_all(&self.dir)?;
        Ok(applied)
    }

    /// Whether a commit of this branch was started and did not finish.
    pub fn commit_interrupted(&self) -> bool {
        self.dir.join("committing").exists()
    }

    /// The lock that serializes commits into this branch's workspace,
    /// `$FOIL_HOME/locks/<hash of the workspace path>`. Waits for it.
    fn workspace_lock(&self) -> io::Result<File> {
        let locks = home()?.join("locks");
        fs::create_dir_all(&locks)?;
        let file = File::create(locks.join(format!("{:016x}", fnv1a(self.workspace.as_os_str().as_bytes()))))?;
        loop {
            // SAFETY: flock on a descriptor we own; released when the file closes.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                return Ok(file);
            }
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }

    /// Write the branch's changes as a unified diff (via `diff -u`):
    /// text for regular files, a one-line note for everything else.
    pub fn write_patch(&self, out: &mut impl Write, all: bool) -> io::Result<()> {
        let changes = self.changes(all)?.0;
        let (_lock, _access) = self.open_for_reading()?;
        for change in changes {
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
    /// After an interrupted commit the workspace already has some of the
    /// branch's changes, so this refuses unless `force`.
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
        remove_all(&self.dir)
    }
}

/// The parts of a new branch, created inside `dir`.
fn build_branch(dir: &Path, workspace: &Path) -> io::Result<()> {
    fs::create_dir(dir)?;
    fs::create_dir(dir.join("upper"))?;
    fs::create_dir(dir.join("work"))?;
    // Before Linux 6.13 every change is stamped by the coarse clock, so a
    // change made in the same tick just before the branch would carry the
    // branch's own time and look like a conflict. Start on a fresh tick
    // (at most one tick, a few ms; give up after 20 ms on a stalled clock).
    let tick = coarse_now();
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(20);
    while coarse_now() <= tick && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_micros(200));
    }
    let marker = dir.join("workspace");
    fs::write(&marker, workspace.as_os_str().as_bytes())?;
    // The branch's time (see `conflicts`), with two clocks in play: most
    // changes are stamped by a coarse clock that steps once per tick and can
    // trail real time (by 100+ ms on tickless kernels), but since Linux 6.13
    // a file looked at since its last change gets an exact stamp.
    //
    // A new file's coarse stamp is a safe branch time: coarse stamps only
    // grow and exact ones are real time, so every later change is stamped
    // at or after it. But a change made just *before* the branch may carry
    // an exact stamp above it, a false conflict. So: look at the marker and
    // write it again to give it an exact stamp, and wait (briefly) until the
    // coarse clock has caught up with it; then earlier changes are older and
    // later ones at or after it. If the coarse clock trails by more than
    // 20 ms, keep the coarse stamp: never a missed conflict, at worst a
    // false one.
    let coarse = fs::symlink_metadata(&marker)?.modified()?;
    fs::symlink_metadata(&marker)?;
    fs::write(&marker, workspace.as_os_str().as_bytes())?;
    let exact = fs::symlink_metadata(&marker)?.modified()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(20);
    while coarse_now() < exact {
        if std::time::Instant::now() > deadline {
            // Too far behind: fall back to the coarse time.
            return set_mtime(&marker, coarse);
        }
        std::thread::sleep(std::time::Duration::from_micros(200));
    }
    Ok(())
}

/// The kernel's coarse clock: the time file changes are usually stamped with.
fn coarse_now() -> SystemTime {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime fills `ts`.
    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME_COARSE, &mut ts) };
    SystemTime::UNIX_EPOCH + std::time::Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// Set a file's modification time (the branch time is read from it).
fn set_mtime(path: &Path, time: SystemTime) -> io::Result<()> {
    File::options().write(true).open(path)?.set_modified(time)
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

/// Copy the changes recorded in `upper` onto the directory `dir` (open,
/// inside the workspace), depth first, leaving out the paths in `skip`
/// (relative to the workspace; `rel` is where `upper` sits). Returns the
/// number of changes applied. Every step is relative to an open directory
/// and never follows a symlink (see `at.rs`): even if the workspace changes
/// while the commit runs, nothing is written outside it. Applying twice
/// gives the same result, which is what lets a commit be resumed. `links`
/// remembers files with several names in the branch (hard links), so they
/// are linked in the workspace too instead of copied apart.
fn apply(
    upper: &Path,
    dir: &OwnedFd,
    rel: &Path,
    skip: &HashSet<PathBuf>,
    links: &mut HashMap<(u64, u64), (OwnedFd, CString)>,
    access: &Access,
) -> io::Result<usize> {
    let d = dir.as_raw_fd();
    let mut applied = 0;
    let mut entries: Vec<_> = fs::read_dir(upper)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    // One commit at a time per workspace, so one temporary name per folder is enough.
    let tmp = c".foil-commit.tmp";
    for entry in entries {
        // A folder that only holds changes (overlayfs copied it up so it could
        // hold them) is not a change itself; its contents are counted.
        let mut counts = true;
        let rel = rel.join(entry.file_name());
        if skip.contains(&rel) {
            continue;
        }
        let from = entry.path();
        let name = at::cname(&entry.file_name())?;
        let meta = entry.metadata()?;
        let kind = meta.file_type();
        let mode = access.mode(&from, &meta);
        let existing = at::stat_at(d, &name)?;
        if is_whiteout(&meta) {
            at::remove_at(d, &name)?;
        } else if kind.is_dir() {
            let keep = existing.is_some_and(|st| at::is_dir(&st)) && !is_opaque(&from);
            counts = !keep;
            if !keep {
                at::remove_at(d, &name)?;
                at::mkdir_at(d, &name)?;
            }
            let sub = at::open_dir_at(d, &name)?;
            // A folder made read-only by an earlier, interrupted attempt
            // must be writable again to be filled.
            at::fchmod(sub.as_raw_fd(), 0o700)?;
            applied += apply(&from, &sub, &rel, skip, links, access)?;
            copy_xattrs_to(&from, sub.as_raw_fd())?;
            // Only after filling it: the mode may make the folder read-only.
            at::fchmod(sub.as_raw_fd(), mode)?;
        } else if kind.is_symlink() {
            at::remove_at(d, &name)?;
            at::symlink_at(fs::read_link(&from)?.as_os_str(), d, &name)?;
        } else if kind.is_file() {
            if existing.is_some_and(|st| at::is_dir(&st)) {
                at::remove_at(d, &name)?;
            }
            // Written next to the target, then renamed over it: readers see
            // the old file or the new one, never half of each.
            at::remove_at(d, tmp)?;
            let first_name = (meta.nlink() > 1).then(|| links.get(&(meta.dev(), meta.ino()))).flatten();
            match first_name {
                Some((first_dir, first)) => at::link_at(first_dir.as_raw_fd(), first, d, tmp)?,
                None => {
                    let mut out = at::create_at(d, tmp)?;
                    io::copy(&mut File::open(&from)?, &mut out)?;
                    // Before the mode: setting a user xattr needs write permission.
                    copy_xattrs_to(&from, out.as_raw_fd())?;
                    at::fchmod(out.as_raw_fd(), mode)?;
                    if meta.nlink() > 1 {
                        links.insert((meta.dev(), meta.ino()), (dir.try_clone()?, name.clone()));
                    }
                }
            }
            at::rename_at(d, tmp, &name)?;
        } else if kind.is_fifo() {
            at::remove_at(d, &name)?;
            at::mkfifo_at(d, &name, mode)?;
        } else {
            unreachable!("check_committable refuses other file types");
        }
        applied += usize::from(counts);
        crash_point()?;
    }
    Ok(applied)
}

/// Copy `from`'s extended attributes (overlayfs's own left out) onto the
/// open file or folder `fd`.
fn copy_xattrs_to(from: &Path, fd: RawFd) -> io::Result<()> {
    for (name, value) in user_xattrs(from) {
        let name = CString::new(name).expect("split at NUL");
        if let Err(err) = at::fsetxattr(fd, &name, &value) {
            // Some file systems don't take every namespace; not worth failing a commit.
            if err.raw_os_error() != Some(libc::ENOTSUP) && err.raw_os_error() != Some(libc::EPERM) {
                return Err(err);
            }
        }
    }
    Ok(())
}

/// While it lives, every folder in a branch's upper layer can be listed and
/// every file read, even if the command removed our own permission (we own
/// them, so we may give it back). The real modes are written to `record`
/// before anything is changed, so they survive a crash: the next `open`
/// reads them back. Dropping restores them and removes the record.
struct Access {
    record: PathBuf,
    /// The real permission bits of every path we opened up.
    real: Vec<(PathBuf, u32)>,
    outer: bool,
}

/// Whether an `Access` is open in this process.
static ACCESS_OPEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The real permission bits of files and folders an `Access` opened, by
/// device and inode: one file may have several names (hard links), and a
/// `diff` inside a `commit` must see what the commit's `Access` opened.
static REAL_MODES: std::sync::Mutex<Option<HashMap<(u64, u64), u32>>> = std::sync::Mutex::new(None);

/// The real permission bits of the upper-layer entry with metadata `meta`.
fn real_mode(meta: &fs::Metadata) -> u32 {
    let modes = REAL_MODES.lock().unwrap_or_else(|e| e.into_inner());
    modes.as_ref().and_then(|m| m.get(&(meta.dev(), meta.ino())).copied()).unwrap_or(meta.mode() & 0o7777)
}

fn remember_mode(path: &Path, mode: u32) {
    if let Ok(meta) = path.symlink_metadata() {
        let mut modes = REAL_MODES.lock().unwrap_or_else(|e| e.into_inner());
        modes.get_or_insert_with(HashMap::new).entry((meta.dev(), meta.ino())).or_insert(mode);
    }
}

impl Access {
    /// With `may_open` false (the branch is in use), nothing is changed.
    fn open(upper: &Path, record: &Path, may_open: bool) -> io::Result<Access> {
        let mut access = Access { record: record.to_owned(), real: Vec::new(), outer: false };
        // Within one that is already open (diff inside commit), do nothing:
        // the outer one restores everything when it is done.
        if !may_open || ACCESS_OPEN.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Ok(access);
        }
        access.outer = true;
        // Left by a foil that died while the paths were open: those are the real modes.
        if let Ok(raw) = fs::read(record) {
            for item in raw.split(|&b| b == 0).filter(|i| !i.is_empty()) {
                let (mode, path) = item.split_at(item.iter().position(|&b| b == b' ').unwrap_or(0));
                let mode = std::str::from_utf8(mode).ok().and_then(|m| u32::from_str_radix(m, 8).ok());
                if let (Some(mode), Some(path)) = (mode, path.get(1..)) {
                    let path = PathBuf::from(OsStr::from_bytes(path));
                    remember_mode(&path, mode);
                    access.real.push((path, mode));
                }
            }
        }
        access.open_dir(upper)?;
        Ok(access)
    }

    fn open_dir(&mut self, dir: &Path) -> io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            let mode = meta.mode() & 0o7777;
            let path = entry.path();
            let need = if meta.is_dir() { 0o700 } else if meta.is_file() { 0o600 } else { continue };
            if mode & need != need && !self.real.iter().any(|(p, _)| *p == path) {
                self.real.push((path.clone(), mode));
                self.save()?;
                remember_mode(&path, mode);
                fs::set_permissions(&path, fs::Permissions::from_mode(mode | need))?;
            }
            if meta.is_dir() {
                self.open_dir(&path)?;
            }
        }
        Ok(())
    }

    fn save(&self) -> io::Result<()> {
        let mut out = Vec::new();
        for (path, mode) in &self.real {
            out.extend_from_slice(format!("{mode:o} ").as_bytes());
            out.extend_from_slice(path.as_os_str().as_bytes());
            out.push(0);
        }
        write_durably(&self.record, &out)
    }

    /// The real permission bits of `path`, whose current metadata is `meta`.
    fn mode(&self, path: &Path, meta: &fs::Metadata) -> u32 {
        // By path first (the record), then by file: another name of a file
        // this Access opened has the same inode and the same real mode.
        self.real.iter().find(|(p, _)| p == path).map_or_else(|| real_mode(meta), |(_, m)| *m)
    }
}

impl Drop for Access {
    fn drop(&mut self) {
        if !self.outer {
            return;
        }
        ACCESS_OPEN.store(false, std::sync::atomic::Ordering::SeqCst);
        *REAL_MODES.lock().unwrap_or_else(|e| e.into_inner()) = None;
        if self.real.is_empty() {
            return;
        }
        // Children first, so a folder is closed again only after its contents.
        let mut real = self.real.clone();
        real.sort_by(|a, b| b.0.cmp(&a.0));
        for (path, mode) in real {
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(mode));
        }
        let _ = fs::remove_file(&self.record);
    }
}

/// Refuse a branch holding anything `apply` can't recreate (sockets,
/// device files), before the commit changes anything.
fn check_committable(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        let kind = meta.file_type();
        if kind.is_dir() {
            check_committable(&entry.path())?;
        } else if !(is_whiteout(&meta) || kind.is_file() || kind.is_symlink() || kind.is_fifo()) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("cannot commit special file {} (a socket or device); remove it and commit again", entry.path().display()),
            ));
        }
    }
    Ok(())
}

/// A path's extended attributes, overlayfs's own bookkeeping left out,
/// sorted: what a commit copies, for comparing two paths.
fn user_xattrs(path: &Path) -> Vec<(Vec<u8>, Vec<u8>)> {
    let Ok(c) = c_path(path) else { return Vec::new() };
    let mut names = vec![0u8; 4096];
    // SAFETY: valid path; the buffer's length is passed.
    let n = unsafe { libc::llistxattr(c.as_ptr(), names.as_mut_ptr().cast(), names.len()) };
    if n <= 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for name in names[..n as usize].split(|&b| b == 0).filter(|n| !n.is_empty()) {
        if name.starts_with(b"user.overlay.") || name.starts_with(b"trusted.overlay.") {
            continue;
        }
        let cname = std::ffi::CString::new(name).expect("split at NUL");
        let mut value = vec![0u8; 65536];
        // SAFETY: valid path and name; the buffer's length is passed.
        let len = unsafe { libc::lgetxattr(c.as_ptr(), cname.as_ptr(), value.as_mut_ptr().cast(), value.len()) };
        if len >= 0 {
            out.push((name.to_vec(), value[..len as usize].to_vec()));
        }
    }
    out.sort();
    out
}

/// The journal: a version line, then the paths the commit leaves out.
fn journal_bytes(skip: &HashSet<PathBuf>) -> Vec<u8> {
    let mut paths: Vec<&PathBuf> = skip.iter().collect();
    paths.sort();
    let mut out = b"foil-commit 1\n".to_vec();
    for path in paths {
        out.extend_from_slice(path.as_os_str().as_bytes());
        out.push(0);
    }
    out
}

fn read_journal(raw: &[u8]) -> io::Result<HashSet<PathBuf>> {
    let rest = raw
        .strip_prefix(b"foil-commit 1\n")
        .ok_or_else(|| invalid("unreadable commit journal; abort the branch with --force".to_owned()))?;
    Ok(rest.split(|&b| b == 0).filter(|p| !p.is_empty()).map(|p| PathBuf::from(OsStr::from_bytes(p))).collect())
}

/// Write `bytes` to `path` so that after a crash it holds all of them or
/// does not exist: a temporary file, synced, renamed, and the folder synced.
fn write_durably(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut file = File::create(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&tmp, path)?;
    File::open(path.parent().unwrap_or(Path::new("/")))?.sync_all()
}

/// Flush everything written to the file system that holds `path`.
fn sync_fs(path: &Path) -> io::Result<()> {
    let dir = File::open(path)?;
    // SAFETY: syncfs on a descriptor we own.
    if unsafe { libc::syncfs(dir.as_raw_fd()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// 64-bit FNV-1a: a stable, dependency-free hash for lock file names.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3))
}

/// Test builds only: `FOIL_TEST_CRASH_AFTER=N` kills foil (SIGKILL, like a
/// crash) right after the Nth change a commit applies; `FOIL_TEST_FAIL_AFTER=N`
/// makes the Nth change fail with an I/O error (EIO), like a failing disk.
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
        let Some(limit) = std::env::var("FOIL_TEST_CRASH_AFTER").ok().and_then(|v| v.parse::<usize>().ok()) else {
            return Ok(());
        };
        if done >= limit {
            // SAFETY: sends a signal to ourselves.
            unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
        }
    }
    Ok(())
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
