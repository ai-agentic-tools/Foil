# Changelog

## Unreleased

## 0.3.0 — 2026-10-24

First public release.

- `--timeout TIME` for `foil run`, `foil agent` and `foil fanout`: the
  command gets SIGTERM, then SIGKILL after the grace period, and the run
  exits 124. A hung fanout attempt no longer holds up the others.
- `foil fanout --json`: the attempts, their checks and the best one, on stdout.
- `examples/django-username`: three Claude Code sessions fix a real Django
  bug (ticket #30257) in parallel strict branches; the best passing fix is
  committed. Verified end to end (~20 s, Haiku).
- `examples/best-of-n`: four agents on one failing test, the smallest passing
  fix committed, a rogue attempt contained (no API key needed; `claude.sh` for
  real Claude Code sessions).
- Limits and `foil session` name their systemd scope, so foil works when it
  is itself started by `systemd-run --scope`.
- `foil commit` counts changed paths, not folders overlayfs copied up to
  hold them ("applied 1 change", not "applied 66 changes").
- `fanout` records each attempt's own run time; `--json` gives `status` as a
  number and `timed_out` separately.
- Release binaries with SHA-256 checksums; `install.sh` can download one.
- Licensed MIT OR Apache-2.0.

Security and reliability fixes from a design review:

- Strict runs without a terminal get their own session: the command can no
  longer signal foil's process group (and through it the calling script)
  or reach a controlling terminal.
- Descriptors foil inherited are closed before a strict command starts.
- The proxy refuses internal addresses (loopback, private, link-local such
  as 169.254.169.254) for hosts allowed only through `*` or `*.domain`, and
  connects to the address it checked (no DNS rebinding window).
- The proxy streams request bodies instead of holding them in memory,
  bounds chunk-size lines, rejects bad lengths, caps open connections, and
  backs off when out of descriptors: the sandbox can't crash it.
- `--allow-host` values are checked up front (`host:99999` is an error, not
  "ports 80 and 443"); `[v6]:port` and trailing dots match.
- `foil agent` never shows the home folder or a folder right in it
  (`~/.cargo`, `~/.local`) as part of an agent's installation.
- `foil agent` warns when no API key is injected.
- `FOIL_GRACE_MS` applies inside strict runs too.
- foil no longer signals the proxy's PID after the cleanup already reaped it.
- The proxy reports readiness on a pipe: no 5 ms polling at start.

## 0.2.0 — 2026-10-23

- `--strict`, the allowlist proxy with API-key injection, `foil agent`.
- `foil fanout` and `foil pick`; `foil session`.
- Crash-safe, race-free commit (journal, open folder handles, no symlink
  following); conflict detection; `.gitignore`d build output left out.
- Runs inside Docker and Podman; platform matrix (Ubuntu 22.04–26.04,
  Debian 13, Fedora 44, AlmaLinux 9).

## 0.1.0 — 2026-10-11

- Branches (`run -b`, `diff`, `commit`, `abort`), `--protect`,
  `--no-network`, `--memory`/`--cpus`/`--pids`, cleanup box, `foil doctor`,
  `foil setup`.
