//! File operations relative to an open directory, never following symlinks.
//!
//! A commit writes into the workspace while other programs may be changing
//! it. If it worked with paths, a folder swapped for a symlink half-way
//! (`ws/src` -> `/home/me/.ssh`) would send the following writes outside
//! the workspace. Here every step starts from a directory handle already
//! opened inside the workspace, and every name is opened with `O_NOFOLLOW`:
//! a symlink where a folder should be is an error, never a detour.

use std::ffi::{CStr, CString, OsStr};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use libc::c_int;

fn cvt(rc: c_int) -> io::Result<c_int> {
    if rc == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

/// `name` as a C string (names come from directory listings: no NUL inside).
pub fn cname(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL byte in a name"))
}

/// Open the directory at `path` itself, refusing a symlink.
pub fn open_dir(path: &Path) -> io::Result<OwnedFd> {
    let c = cname(path.as_os_str())?;
    // SAFETY: valid NUL-terminated path; the result is checked.
    let fd = cvt(unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) })?;
    // SAFETY: a fresh descriptor we own.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Open the directory `name` inside `dir`, refusing a symlink.
pub fn open_dir_at(dir: RawFd, name: &CStr) -> io::Result<OwnedFd> {
    // SAFETY: valid descriptor and name; the result is checked.
    let fd = cvt(unsafe {
        libc::openat(dir, name.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
    })?;
    // SAFETY: a fresh descriptor we own.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// What `name` inside `dir` is (not following a symlink), or `None`.
pub fn stat_at(dir: RawFd, name: &CStr) -> io::Result<Option<libc::stat>> {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: valid descriptor, name and buffer; fstatat fills it on success.
    match cvt(unsafe { libc::fstatat(dir, name.as_ptr(), st.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) }) {
        Ok(_) => Ok(Some(unsafe { st.assume_init() })),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

pub fn is_dir(st: &libc::stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFDIR
}

/// The names in an open directory. Listed through `/proc/self/fd`, which
/// names the directory itself, wherever it is now.
pub fn names(dir: RawFd) -> io::Result<Vec<CString>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(format!("/proc/self/fd/{dir}"))? {
        out.push(cname(&entry?.file_name())?);
    }
    Ok(out)
}

/// Remove `name` inside `dir`, whatever it is, a directory tree included;
/// a missing name is fine. Symlinks are removed, never followed.
pub fn remove_at(dir: RawFd, name: &CStr) -> io::Result<()> {
    let Some(st) = stat_at(dir, name)? else { return Ok(()) };
    if is_dir(&st) {
        let sub = open_dir_at(dir, name)?;
        // Our own folder may have been made unwritable or unsearchable.
        // SAFETY: valid descriptor.
        let _ = unsafe { libc::fchmod(sub.as_raw_fd(), 0o700) };
        for child in names(sub.as_raw_fd())? {
            remove_at(sub.as_raw_fd(), &child)?;
        }
        // SAFETY: valid descriptor and name.
        cvt(unsafe { libc::unlinkat(dir, name.as_ptr(), libc::AT_REMOVEDIR) })?;
    } else {
        // SAFETY: valid descriptor and name.
        cvt(unsafe { libc::unlinkat(dir, name.as_ptr(), 0) })?;
    }
    Ok(())
}

/// Make the directory `name` inside `dir` (mode 0700 until it is filled).
pub fn mkdir_at(dir: RawFd, name: &CStr) -> io::Result<()> {
    // SAFETY: valid descriptor and name.
    cvt(unsafe { libc::mkdirat(dir, name.as_ptr(), 0o700) }).map(drop)
}

/// Create the file `name` inside `dir`, which must not exist yet (no
/// symlink can be planted in its place to redirect the write).
pub fn create_at(dir: RawFd, name: &CStr) -> io::Result<std::fs::File> {
    // SAFETY: valid descriptor and name; the result is checked.
    let fd = cvt(unsafe {
        libc::openat(
            dir,
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    })?;
    // SAFETY: a fresh descriptor we own.
    Ok(std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) }))
}

pub fn symlink_at(target: &OsStr, dir: RawFd, name: &CStr) -> io::Result<()> {
    let t = cname(target)?;
    // SAFETY: valid strings and descriptor.
    cvt(unsafe { libc::symlinkat(t.as_ptr(), dir, name.as_ptr()) }).map(drop)
}

/// A FIFO with exactly `mode` (the umask is set aside for the call).
pub fn mkfifo_at(dir: RawFd, name: &CStr, mode: u32) -> io::Result<()> {
    // SAFETY: umask has no preconditions; restored right after.
    let old = unsafe { libc::umask(0) };
    // SAFETY: valid descriptor and name.
    let rc = unsafe { libc::mkfifoat(dir, name.as_ptr(), mode as libc::mode_t) };
    // SAFETY: as above.
    unsafe { libc::umask(old) };
    cvt(rc).map(drop)
}

/// Another name, `new` in `new_dir`, for the file `old` in `old_dir`.
pub fn link_at(old_dir: RawFd, old: &CStr, new_dir: RawFd, new: &CStr) -> io::Result<()> {
    // SAFETY: valid descriptors and names; flags 0 does not follow symlinks.
    cvt(unsafe { libc::linkat(old_dir, old.as_ptr(), new_dir, new.as_ptr(), 0) }).map(drop)
}

/// Replace `to` with `from`, both inside `dir`, in one step.
pub fn rename_at(dir: RawFd, from: &CStr, to: &CStr) -> io::Result<()> {
    // SAFETY: valid descriptor and names.
    cvt(unsafe { libc::renameat(dir, from.as_ptr(), dir, to.as_ptr()) }).map(drop)
}

pub fn fchmod(fd: RawFd, mode: u32) -> io::Result<()> {
    // SAFETY: valid descriptor.
    cvt(unsafe { libc::fchmod(fd, mode as libc::mode_t) }).map(drop)
}

/// Set the extended attribute `name` = `value` on an open file or folder.
pub fn fsetxattr(fd: RawFd, name: &CStr, value: &[u8]) -> io::Result<()> {
    // SAFETY: valid descriptor and name; `value.len()` bytes are readable.
    cvt(unsafe { libc::fsetxattr(fd, name.as_ptr(), value.as_ptr().cast(), value.len(), 0) }).map(drop)
}
