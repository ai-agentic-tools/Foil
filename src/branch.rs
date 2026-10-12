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
//! namespaces, which some systems restrict; `explain` turns failures into hints.

use std::ffi::{CStr, CString, OsStr};
use std::fs::{self, File};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{symlink, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use crate::supervisor::cvt;

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

/// Everything the forked child needs to enter a branch, built in advance
/// because the child may not allocate memory between fork and exec.
#[derive(Clone)]
pub struct Prepared {
    /// The workspace path: the overlay is mounted right on top of it.
    target: CString,
    /// `lowerdir=…,upperdir=…,workdir=…,userxattr`.
    options: CString,
    /// The directory the command starts in, looked up again after the mount.
    cwd: CString,
    /// `"<uid> <uid> 1"`: map our own uid to itself in the new user namespace.
    uid_map: CString,
    gid_map: CString,
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
        Ok(file)
    }

    fn upper(&self) -> PathBuf {
        self.dir.join("upper")
    }

    /// Build the buffers `enter` needs, for a command starting in the current directory.
    pub fn prepare(&self) -> io::Result<Prepared> {
        let upper = self.upper();
        let work = self.dir.join("work");
        for path in [&self.workspace, &upper, &work] {
            // These characters separate or escape overlayfs mount options.
            if path.as_os_str().as_bytes().iter().any(|b| b",:\\".contains(b)) {
                return Err(invalid(format!(
                    "overlayfs cannot use a path containing ',', ':' or '\\': {}",
                    path.display()
                )));
            }
        }
        let mut options = b"lowerdir=".to_vec();
        options.extend_from_slice(self.workspace.as_os_str().as_bytes());
        options.extend_from_slice(b",upperdir=");
        options.extend_from_slice(upper.as_os_str().as_bytes());
        options.extend_from_slice(b",workdir=");
        options.extend_from_slice(work.as_os_str().as_bytes());
        // Mark whiteouts and opaque directories with user.* xattrs, which an
        // unprivileged mount may write (trusted.* needs real root).
        options.extend_from_slice(b",userxattr");

        // SAFETY: getuid and getgid cannot fail.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        Ok(Prepared {
            target: c_path(&self.workspace)?,
            options: CString::new(options).map_err(|_| invalid("NUL byte in a path".to_owned()))?,
            cwd: c_path(&std::env::current_dir()?)?,
            uid_map: CString::new(format!("{uid} {uid} 1")).expect("digits only"),
            gid_map: CString::new(format!("{gid} {gid} 1")).expect("digits only"),
        })
    }

    /// The changes this branch would make to its workspace, in path order.
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

    /// Apply the branch's changes to its workspace, then delete the branch.
    /// Each file is replaced atomically, but the commit as a whole is not:
    /// if it fails half-way, the branch is kept so it can be retried.
    pub fn commit(self) -> io::Result<usize> {
        // Held until the branch is gone, so no run can start in between.
        let _lock = self.lock()?;
        let applied = apply(&self.upper(), &self.workspace)?;
        remove_all(&self.dir)?;
        Ok(applied)
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

/// Move the calling process (forked, not yet exec'd) into the branch: a new
/// user namespace mapping our own uid and gid, a new mount namespace, and
/// the overlay mounted over the workspace. Runs between fork and exec, so it
/// makes raw syscalls on prebuilt buffers only: no allocation.
pub fn enter(p: &Prepared) -> io::Result<()> {
    // SAFETY: plain syscalls on NUL-terminated buffers that outlive the calls.
    unsafe {
        cvt(libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS))?;
        // An unprivileged process must give up setgroups before writing gid_map.
        write_file(c"/proc/self/setgroups", b"deny")?;
        write_file(c"/proc/self/uid_map", p.uid_map.as_bytes())?;
        write_file(c"/proc/self/gid_map", p.gid_map.as_bytes())?;
        // Make every mount private, so ours never propagates anywhere else.
        cvt(libc::mount(
            c"none".as_ptr(),
            c"/".as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        ))?;
        cvt(libc::mount(
            c"overlay".as_ptr(),
            p.target.as_ptr(),
            c"overlay".as_ptr(),
            0,
            p.options.as_ptr().cast(),
        ))?;
        // A working directory inside the workspace still points at the real
        // folder underneath the overlay; look the path up again.
        cvt(libc::chdir(p.cwd.as_ptr()))?;
    }
    Ok(())
}

/// Whether `enter` works here at all, tried in a throwaway child. Used only
/// after a run failed to start, to tell a branch problem from a command problem.
pub fn can_enter(p: &Prepared) -> bool {
    let p = p.clone();
    let mut cmd = Command::new("true");
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // SAFETY: `enter` only makes raw syscalls on prebuilt buffers.
    unsafe { cmd.pre_exec(move || enter(&p)) };
    cmd.status().is_ok_and(|status| status.success())
}

/// Wrap an error from entering a branch with a hint about the likely cause.
pub fn explain(err: io::Error) -> io::Error {
    let read = |path: &str| fs::read_to_string(path).map(|s| s.trim().to_owned()).unwrap_or_default();
    let hint = if read("/proc/sys/kernel/apparmor_restrict_unprivileged_userns") == "1" {
        "AppArmor restricts unprivileged user namespaces here \
         (kernel.apparmor_restrict_unprivileged_userns=1)"
    } else if read("/proc/sys/user/max_user_namespaces") == "0" {
        "user namespaces are disabled here (user.max_user_namespaces=0)"
    } else if err.raw_os_error() == Some(libc::EINVAL) {
        "overlayfs rejected the layers; FOIL_HOME must be on a filesystem with \
         user xattrs, such as ext4, xfs, btrfs, or tmpfs on Linux >= 6.6"
    } else {
        "branches need Linux >= 5.11 and unprivileged user namespaces, which \
         container runtimes often block (e.g. Docker's default seccomp profile)"
    };
    io::Error::other(format!("cannot enter the branch: {err}\n  hint: {hint}"))
}

/// Copy the changes recorded in `upper` onto `target`, depth first.
/// Returns the number of changes applied.
fn apply(upper: &Path, target: &Path) -> io::Result<usize> {
    let mut applied = 0;
    for entry in fs::read_dir(upper)? {
        let entry = entry?;
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
            applied += apply(&from, &to)?;
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

/// Write all of `data` to the file at `path` with raw syscalls (no allocation).
unsafe fn write_file(path: &CStr, data: &[u8]) -> io::Result<()> {
    let fd = cvt(libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC))?;
    let written = libc::write(fd, data.as_ptr().cast(), data.len());
    let err = io::Error::last_os_error();
    libc::close(fd);
    if written == -1 {
        return Err(err);
    }
    // A short write sets no errno; report it as such (no allocation needed).
    if written as usize != data.len() {
        return Err(io::ErrorKind::WriteZero.into());
    }
    Ok(())
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

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| invalid(format!("NUL byte in path {}", path.display())))
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}
