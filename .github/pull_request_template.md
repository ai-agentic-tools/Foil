<!-- One topic per pull request. See CONTRIBUTING.md. -->

## What and why

<!-- What changes, and the problem it fixes or the case it adds. Link the issue if there is one. -->

## How it is tested

<!-- The test that fails without this change, and where you ran it (kernel, distribution, or macOS version). -->

## Checklist

- [ ] `cargo test` and `cargo clippy --all-targets -- -D warnings` pass
- [ ] A test fails without this change (a change to a SECURITY.md guarantee: an escape attempt in `tests/strict.rs`)
- [ ] A line in CHANGELOG.md under "Unreleased", if users will notice
- [ ] Any new `unsafe` block has a `// SAFETY:` comment
