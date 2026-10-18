//! Helpers shared by the resilience and differential tests: a scratch
//! workspace with its own branch store, and a snapshot of a folder tree that
//! captures everything a commit must reproduce.

#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

/// A fresh workspace (`ws`), branch store (`home`) and scratch space,
/// removed on drop.
pub struct Env {
    pub root: PathBuf,
}

impl Env {
    pub fn new(test: &str) -> Env {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("foil-{test}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("ws")).unwrap();
        std::fs::create_dir_all(root.join("home")).unwrap();
        Env { root: root.canonicalize().unwrap() }
    }

    pub fn ws(&self) -> PathBuf {
        self.root.join("ws")
    }

    pub fn foil(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_foil"));
        cmd.args(args).current_dir(self.ws()).env("FOIL_HOME", self.root.join("home"));
        cmd
    }

    /// `foil run -b <branch> -- sh -c <script>`.
    pub fn run(&self, branch: &str, script: &str) -> Output {
        self.foil(&["run", "-b", branch, "--", "sh", "-c", script]).output().unwrap()
    }

    /// `sh -c <script>` in `dir`, without foil.
    pub fn sh(dir: &Path, script: &str) -> Output {
        Command::new("sh").args(["-c", script]).current_dir(dir).output().unwrap()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = Command::new("chmod").args(["-R", "u+rwx"]).arg(&self.root).status();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

pub fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Whether branches work here; skips (or fails, if FOIL_REQUIRE_BRANCH is
/// set) otherwise.
pub fn branches_available() -> bool {
    let env = Env::new("probe");
    let out = env.run("probe", "true");
    if out.status.success() {
        return true;
    }
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("cannot isolate the command") && std::env::var_os("FOIL_REQUIRE_BRANCH").is_none(),
        "branches unavailable: {stderr}"
    );
    eprintln!("skipped: branches unavailable here: {stderr}");
    false
}

/// One entry of a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Dir { mode: u32, xattrs: Vec<(Vec<u8>, Vec<u8>)> },
    /// `link` is the first path (in sorted order) of the file's hard-link
    /// group inside the tree, so groups compare across trees.
    File { mode: u32, content: Vec<u8>, xattrs: Vec<(Vec<u8>, Vec<u8>)>, link: Option<PathBuf> },
    Symlink(PathBuf),
    Fifo { mode: u32 },
    Other,
}

/// Every path under `root` (relative) with what a commit must reproduce:
/// type, permission bits, content, symlink target, user xattrs, and which
/// names share a file. Not times or owners.
pub fn snapshot(root: &Path) -> BTreeMap<PathBuf, Node> {
    let mut out = BTreeMap::new();
    let mut inodes: HashMap<(u64, u64), PathBuf> = HashMap::new();
    walk(root, Path::new(""), &mut out, &mut inodes);
    out
}

fn walk(root: &Path, rel: &Path, out: &mut BTreeMap<PathBuf, Node>, inodes: &mut HashMap<(u64, u64), PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(root.join(rel)).unwrap().map(|e| e.unwrap()).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let rel = rel.join(entry.file_name());
        let path = root.join(&rel);
        let meta = std::fs::symlink_metadata(&path).unwrap();
        let mode = meta.mode() & 0o7777;
        let kind = meta.file_type();
        let node = if kind.is_dir() {
            // Read a directory we can't list by making it listable first.
            let restore = mode & 0o500 != 0o500;
            if restore {
                std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(mode | 0o700)).unwrap();
            }
            walk(root, &rel, out, inodes);
            if restore {
                std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(mode)).unwrap();
            }
            Node::Dir { mode, xattrs: xattrs(&path) }
        } else if kind.is_symlink() {
            Node::Symlink(std::fs::read_link(&path).unwrap())
        } else if kind.is_file() {
            let content = if mode & 0o400 == 0 { b"<unreadable>".to_vec() } else { std::fs::read(&path).unwrap() };
            let link = (meta.nlink() > 1).then(|| inodes.entry((meta.dev(), meta.ino())).or_insert(rel.clone()).clone());
            Node::File { mode, content, xattrs: xattrs(&path), link }
        } else if kind.is_fifo() {
            Node::Fifo { mode }
        } else {
            Node::Other
        };
        out.insert(rel, node);
    }
}

fn xattrs(path: &Path) -> Vec<(Vec<u8>, Vec<u8>)> {
    let c = CString::new(path.as_os_str().as_bytes()).unwrap();
    let mut names = vec![0u8; 4096];
    // SAFETY: valid path; buffer length passed.
    let n = unsafe { libc::llistxattr(c.as_ptr(), names.as_mut_ptr().cast(), names.len()) };
    if n <= 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for name in names[..n as usize].split(|&b| b == 0).filter(|n| n.starts_with(b"user.")) {
        let cname = CString::new(name).unwrap();
        let mut value = vec![0u8; 65536];
        // SAFETY: valid path and name; buffer length passed.
        let len = unsafe { libc::lgetxattr(c.as_ptr(), cname.as_ptr(), value.as_mut_ptr().cast(), value.len()) };
        if len >= 0 {
            out.push((name.to_vec(), value[..len as usize].to_vec()));
        }
    }
    out.sort();
    out
}

/// The paths where two snapshots differ, for failure messages.
pub fn differences(a: &BTreeMap<PathBuf, Node>, b: &BTreeMap<PathBuf, Node>) -> Vec<String> {
    let mut out = Vec::new();
    for (path, node) in a {
        match b.get(path) {
            None => out.push(format!("only in first: {}", path.display())),
            Some(other) if other != node => out.push(format!("differs: {}\n    {node:?}\n    {other:?}", path.display())),
            _ => {}
        }
    }
    for path in b.keys().filter(|p| !a.contains_key(*p)) {
        out.push(format!("only in second: {}", path.display()));
    }
    out
}

/// `cp -a` of `from` to `to`.
pub fn copy_tree(from: &Path, to: &Path) {
    let status = Command::new("cp").arg("-a").arg(from).arg(to).status().unwrap();
    assert!(status.success());
}
