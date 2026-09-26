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
    group.finish();
}

criterion_group!(benches, spawn_true);
criterion_main!(benches);
