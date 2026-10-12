//! What does wrapping a command in `foil run` cost compared to running it directly?

use std::process::Command;

use criterion::{criterion_group, criterion_main, Criterion};

fn spawn_true(c: &mut Criterion) {
    let mut group = c.benchmark_group("spawn_true");
    group.bench_function("direct", |b| {
        b.iter(|| Command::new("true").status().unwrap())
    });
    group.bench_function("foil_run", |b| {
        b.iter(|| {
            Command::new(env!("CARGO_BIN_EXE_foil"))
                .args(["run", "--", "true"])
                .status()
                .unwrap()
        })
    });
    // A run inside a branch: user + mount namespaces and an overlay mount.
    // Needs unprivileged user namespaces; the workspace lives in the temp dir.
    // FOIL_NO_SCOPE: time the branch itself, not the systemd scope foil
    // would otherwise add when it has no delegated cgroup (~12 ms).
    let root = std::env::temp_dir().join(format!("foil-bench-{}", std::process::id()));
    std::fs::create_dir_all(root.join("ws")).unwrap();
    let run_in_branch = || {
        Command::new(env!("CARGO_BIN_EXE_foil"))
            .args(["run", "--branch", "bench", "--", "true"])
            .current_dir(root.join("ws"))
            .env("FOIL_HOME", root.join("home"))
            .env("FOIL_NO_SCOPE", "1")
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
    };
    // Timing a failure would be meaningless: skip where branches are blocked.
    if run_in_branch().success() {
        group.bench_function("foil_run_branch", |b| b.iter(run_in_branch));
    } else {
        eprintln!("skipped foil_run_branch: branches unavailable here");
    }
    let _ = Command::new("chmod").args(["-R", "u+rwx"]).arg(&root).status();
    let _ = std::fs::remove_dir_all(&root);
    group.finish();
}

criterion_group!(benches, spawn_true);
criterion_main!(benches);
