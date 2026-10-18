//! Differential test of branches (family A of the quality plan): random
//! sequences of file operations run in a foil branch and in a plain copy
//! must give the same tree, and `foil diff` must show every path that
//! changed. Needs branches; skipped where they are unavailable unless
//! FOIL_REQUIRE_BRANCH is set.
//!
//! FOIL_DIFF_CASES sets the number of cases (default 60), FOIL_DIFF_SEED
//! the first seed; a failure prints the seed and the script to reproduce it.

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use common::{branches_available, copy_tree, differences, snapshot, text, Env};

/// xorshift64*: small, deterministic, good enough to pick operations.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
}

const NAMES: [&str; 8] = ["a", "b", "c d", "ünï", "dir", ".hidden", "x.txt", "L"];

/// A path of 1 to 3 parts from the name pool (one name is 200 bytes long).
fn path(rng: &mut Rng) -> String {
    let depth = 1 + rng.below(3);
    (0..depth)
        .map(|_| match rng.pick(&NAMES) {
            "L" => "L".repeat(200),
            name => name.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// One shell command; `|| true` because many will fail (missing parents,
/// wrong types), and they must fail the same way in both trees.
fn operation(rng: &mut Rng) -> String {
    let p = quote(&path(rng));
    let q = quote(&path(rng));
    let n = rng.below(5000);
    // Opening a FIFO to write waits for a reader forever: skip FIFOs.
    let w = format!("[ ! -p {p} ] &&");
    let cmd = match rng.below(16) {
        0 | 1 => format!("{w} printf 'v{n}\\n' > {p}"),
        2 => format!("{w} printf 'more{n}' >> {p}"),
        3 => format!("{w} yes {n} | head -c {} > {p}", n * 97),
        4 => format!("{w} truncate -s {} {p}", n % 300),
        5 | 6 => format!("mkdir -p {p}"),
        7 => format!("rm -rf {p}"),
        8 => format!("mv {p} {q}"),
        9 => format!("rm -rf {p} && ln -s {q} {p}"),
        10 => format!("ln {p} {q}"),
        11 => format!("chmod {} {p}", rng.pick(&["644", "755", "600", "444", "700", "555"])),
        12 => format!("mkfifo {p}"),
        13 => format!("python3 -c \"import os,sys; os.setxattr(sys.argv[1], 'user.k', b'{n}')\" {p}"),
        14 => format!("rm -rf {p} && mkdir -p {p} && printf 'in{n}' > {p}/inner"),
        _ => format!("cp -a {p} {q}"),
    };
    format!("{{ {cmd}; }} 2>/dev/null || true")
}

/// The starting tree: files, folders and a symlink, no hard links (writing
/// through a hard link of a file that existed before the branch is the one
/// known overlayfs difference; see the quality plan).
fn initial(rng: &mut Rng) -> String {
    let mut lines = vec!["mkdir -p dir/dir 'c d'".to_owned()];
    for _ in 0..(4 + rng.below(8)) {
        lines.push(operation_kind(rng, &[0, 3, 5, 9, 11, 13]));
    }
    lines.push("chmod -R u+rwX . 2>/dev/null; true".to_owned());
    lines.join("\n")
}

fn operation_kind(rng: &mut Rng, kinds: &[usize]) -> String {
    loop {
        let op = operation(rng);
        let kind = kinds[rng.below(kinds.len())];
        // Re-roll until the operation is of an allowed kind (cheap and simple).
        let ok = match kind {
            0 => op.contains("printf 'v"),
            3 => op.contains("| head -c"),
            5 => op.contains("mkdir -p") && !op.contains("rm -rf"),
            9 => op.contains("ln -s"),
            11 => op.contains("chmod 6") || op.contains("chmod 7"),
            13 => op.contains("setxattr"),
            _ => false,
        };
        if ok {
            return op;
        }
    }
}

/// Every path that differs between two snapshots.
fn changed(a: &std::collections::BTreeMap<PathBuf, common::Node>, b: &std::collections::BTreeMap<PathBuf, common::Node>) -> BTreeSet<PathBuf> {
    let keys: BTreeSet<&PathBuf> = a.keys().chain(b.keys()).collect();
    keys.into_iter().filter(|k| a.get(*k) != b.get(*k)).cloned().collect()
}

fn run_case(seed: u64) -> Result<(), String> {
    let mut rng = Rng(seed.max(1));
    let env = Env::new("diff");
    let setup = initial(&mut rng);
    let out = Env::sh(&env.ws(), &setup);
    if !out.status.success() {
        return Err(format!("setup failed: {}", text(&out.stderr)));
    }
    let ops: Vec<String> = (0..(5 + rng.below(25))).map(|_| operation(&mut rng)).collect();
    // Sometimes in two runs on the same branch, to test continuing a branch.
    let split = if rng.below(3) == 0 { rng.below(ops.len()) } else { ops.len() };
    let script = ops.join("\n");
    let report = |what: String| format!("seed {seed}: {what}\n--- setup ---\n{setup}\n--- script ---\n{script}");

    let reference = env.root.join("reference");
    copy_tree(&env.ws(), &reference);
    Env::sh(&reference, &script);
    let before = snapshot(&env.ws());
    let want = snapshot(&reference);

    for part in [&ops[..split], &ops[split..]] {
        if part.is_empty() {
            continue;
        }
        let out = env.run("t", &part.join("\n"));
        if !out.status.success() {
            return Err(report(format!("foil run failed: {}", text(&out.stderr))));
        }
    }
    let now = snapshot(&env.ws());
    if now != before {
        return Err(report(format!("the workspace changed before commit:\n{}", differences(&now, &before).join("\n"))));
    }

    // Every path that really changed is listed by foil diff, itself or as
    // part of a listed folder that was added, replaced or deleted.
    let out = env.foil(&["diff", "--all", "t"]).output().unwrap();
    let listed: Vec<(char, PathBuf)> = text(&out.stdout)
        .lines()
        .filter_map(|l| Some((l.chars().next()?, PathBuf::from(l.get(2..)?))))
        .collect();
    for path in changed(&before, &want) {
        let covered = listed.iter().any(|(kind, p)| {
            *p == path || (path.starts_with(p) && matches!(kind, 'A' | 'R' | 'D'))
        });
        let only_a_folder_mode = matches!(want.get(&path), Some(common::Node::Dir { .. }))
            && matches!(before.get(&path), Some(common::Node::Dir { .. }));
        if !covered && !only_a_folder_mode {
            return Err(report(format!("foil diff does not show {}:\n{}", path.display(), text(&out.stdout))));
        }
    }

    let out = env.foil(&["commit", "--all", "t"]).output().unwrap();
    if !out.status.success() {
        return Err(report(format!("commit failed: {}", text(&out.stderr))));
    }
    let got = snapshot(&env.ws());
    if got != want {
        return Err(report(format!("after commit the workspace differs from the plain copy:\n{}", differences(&got, &want).join("\n"))));
    }
    if Path::new(&env.root.join("home/branches/t")).exists() {
        return Err(report("the branch is still there after commit".to_owned()));
    }
    Ok(())
}

#[test]
fn branches_behave_like_a_plain_copy() {
    if !branches_available() {
        return;
    }
    let cases: u64 = std::env::var("FOIL_DIFF_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(60);
    let first: u64 = std::env::var("FOIL_DIFF_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    let failures: Vec<String> = (first..first + cases).filter_map(|seed| run_case(seed).err()).take(3).collect();
    assert!(failures.is_empty(), "{} failing case(s), first ones:\n\n{}", failures.len(), failures.join("\n\n"));
}
