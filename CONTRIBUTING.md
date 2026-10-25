# Contributing

Thanks for helping. Bug reports with a reproducer are the most valuable
contribution: the exact command, `foil doctor` output, and what you expected.

## Building and testing

foil is Linux-only (namespaces, overlayfs, pidfd, cgroup v2). On macOS or
Windows, use a Linux VM (e.g. [Lima](https://lima-vm.io): `limactl start
template://ubuntu`), with the checkout mounted into it.

```sh
cargo build
sudo target/debug/foil setup        # Ubuntu >= 23.10: allow user namespaces
cargo test                          # tests that need a missing feature skip themselves
systemd-run --user --scope -q env FOIL_REQUIRE_CGROUP=1 FOIL_REQUIRE_LIMITS=1 \
    FOIL_REQUIRE_BRANCH=1 cargo test   # what CI runs: nothing may skip
cargo clippy --all-targets -- -D warnings
```

Longer checks: `tests/differential.rs` compares foil with a plain copy on
random file trees (`FOIL_DIFF_CASES=10000` for a long run);
`bench/foilbench.py` measures start, diff and commit times;
`bench/matrix/lima-matrix.sh` runs the suite on other distributions and
kernels; `contrib/docker/test-in-container.sh` runs it inside Docker.

## Changes

- One topic per pull request, with a test that fails without the change.
  A fix to a guarantee in [SECURITY.md](SECURITY.md) needs a test in
  `tests/strict.rs` that attempts the escape.
- Keep the binary small and dependency-light: new crates need a reason.
- Comments explain *why*: which kernel behaviour, which race, which
  failure the code is there for.
- `unsafe` blocks carry a `// SAFETY:` comment saying why the call is sound.
- Add a line to [CHANGELOG.md](CHANGELOG.md) under "Unreleased".
