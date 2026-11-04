# SandboxFusion: its own sandbox vs foil

[SandboxFusion](https://github.com/bytedance/SandboxFusion) is ByteDance's
code-execution service for evaluating code models on HumanEval, MBPP,
LiveCodeBench and others. Its `lite` isolation builds a sandbox from parts,
as root:
- an overlay of `/` on a tmpfs;
- cgroups (`cgcreate`/`cgset`);
- a network namespace made by a script;
- `unshare --pid` and `chroot`.

foil does the same job as an ordinary user.

We sent HumanEval's 164 problems (prompt + canonical solution + tests +
`check()`) to its `/run_code` API in three modes on one machine:

- **none**: SandboxFusion with no sandbox (a baseline, not an option for
  untrusted code).
- **lite**: SandboxFusion's own sandbox, with the server running as root.
- **foil**: a 24-line `isolation: foil` mode (`add_foil_isolation.py`).
  - Each command runs under `foil run --strict`, with lite's limits (4 GB,
    1 CPU).
  - The server runs as an ordinary user.

Measured on 2026-11-03 on a 4-vCPU Lima VM (Ubuntu 26.04, Linux 7.0),
SandboxFusion at 469de08. `run.sh` reproduces it with `drive.py`; `results/`
has the reports.

## Results

All 164 programs passed in every mode at every level.

| At once | none | lite | foil |
|---:|---:|---:|---:|
| 1 | 27.5/s, p50 31 ms | 3.7/s, p50 265 ms | **14.4/s, p50 62 ms** |
| 8 | 67.9/s, p50 100 ms | 5.7/s, p50 1,359 ms | **43.0/s, p50 181 ms** |
| 32 | 60.2/s, p50 522 ms | 5.8/s, p50 5,364 ms | **34.5/s, p50 892 ms** |

foil runs these programs 4 to 7.5 times faster than `lite`, at 52–63% of
the speed of running them with no sandbox.

**What a program could do.** The probes are ordinary Python programs sent
like the others.

| Probe | none | lite | foil |
|---|---|---|---|
| list the home folder | the server user's: `.aws`, `.claude`, `.config`… | root's: `.bashrc`, `.profile`, `.ssh` | only what it was given: its Python and the tools on PATH, read-only |
| reach example.com | reached | blocked | blocked |
| leave `sleep` running after it exits | still running | stopped | stopped |

**What each mode leaves on the machine.** Each mode starts on a clean
machine. This was a 164-program run, followed by `kill -9` of the server
while 32 programs ran.

| | none | lite | foil |
|---|---|---|---|
| after normal runs | 1 stray process | 1–9 network namespaces, 2–42 cgroups | nothing |
| after `kill -9` of the server | 1 stray process | **81–92 overlay mounts** under `/tmp`, plus the above | nothing |

**Ease.**
- `lite` needs root (passwordless sudo for `mount`, `cgcreate`,
  `ip netns`), `cgroup-tools` and `iproute2`.
- On this host, every `lite` run failed until we fixed three things
  (`lite_on_cgroup_v2.py`):
  - it copies `/etc/resolv.conf` onto a dangling symlink;
  - it sets cgroup v1 limits on a cgroup v2 host;
  - it expects `/tmp` to be inside the overlay of `/`, but `/tmp` is its
    own tmpfs here.
- Each failed run also left its mounts and cgroups behind (3,000+ mounts
  before we noticed).
- foil mode ran as an ordinary user with no other setup.

## Caveats

- `lite` gives each program its own network namespace with a bridge, which
  is reachable if configured. foil gives none (loopback only); foil's
  `--allow-host` would allow named hosts through its proxy.
- `lite`'s numbers are with our three fixes; upstream may run it on a
  cgroup v1 host where it needs none.
- The home probe under `lite` lists root's home because the server runs as
  root. SandboxFusion's `set_uid` option (off by default) runs code as
  another user.
