# OpenSandbox's create benchmark: Docker vs foil

OpenSandbox publishes how fast it creates sandboxes
([performance.md](https://github.com/opensandbox-group/OpenSandbox/blob/main/docs/architecture/fast-sandbox/performance.md)).
We ran its benchmark script against two OpenSandbox servers on the same
machine, through the same unchanged Python SDK:

- **docker**: OpenSandbox's own server 1.2.0 with its Docker runtime
  (`ubuntu:24.04` plus OpenSandbox's execd image);
- **foil**: `foil serve` 0.4.0, where every sandbox is a branch of a
  6,000-file Django checkout.

`create_bench.py` is OpenSandbox's
[`create-bench.py`](https://github.com/opensandbox-group/OpenSandbox/blob/main/scripts/fast-sandbox-env/create-bench.py)
with one change: `Sandbox.create(image)` in place of
`create_from_template()`, since templates are a Firecracker feature. The
method is otherwise unchanged:
- 2 untimed warmups, then 100 timed creates in batches of N at once, with
  5 s between batches;
- each sample runs until the SDK's own readiness check (execd `/ping`,
  polled every 10 ms) passes;
- each sandbox is killed afterwards, outside the timing;
- the SDK uses a 10 s request timeout;
- p50/p90/p99 are nearest-rank, over successful creates only, and failures
  are counted.

Measured on 2026-11-03 on a 4-vCPU, 6 GB Lima VM (Ubuntu 26.04, Linux 7.0)
on an Intel Mac. `run.sh` reproduces it; `results/` has every sample.

## Results

| At once | docker: ok | docker: p50 / p99 | foil: ok | foil: p50 / p99 |
|---:|---:|---:|---:|---:|
| 1 | 100/100 | 1,143 / 1,405 ms | 100/100 | **37 / 62 ms** |
| 10 | 100/100 | 3,130 / 4,214 ms | 100/100 | **236 / 380 ms** |
| 20 | 99/100 | 5,666 / 10,298 ms | 100/100 | **568 / 846 ms** |
| 50 | **17/100** | 7,000 / 10,027 ms | **100/100** | 1,741 / 2,379 ms |

**Reliability.**
- At 50 at once, 83 Docker creates passed the SDK's 10 s request timeout.
- The server still created those 83 containers, and nothing deleted them.
  They were still running when the benchmark ended, and we removed them by
  hand.
- foil left no branches or processes behind at any level.

**Ease.**

| | docker | foil |
|---|---|---|
| Needs | Docker daemon (root-equivalent), `opensandbox-server` (Python), a config file | one 3 MB binary |
| Images | execd (182 MB) and ubuntu:24.04 (119 MB) | none |
| Start | `opensandbox-server --config osb.toml` | `foil serve -w DIR` |
| In the sandbox | an empty container | your repo, untracked files included, with `diff`/`commit` |

## What this does and doesn't show

- **This is foil against OpenSandbox's Docker runtime on one small
  machine.** It is not a comparison with its Firecracker fast-sandbox. The
  published Firecracker numbers are from a cluster of two 192-CPU nodes:
  - 64 ms p50 one at a time;
  - 102.7 ms p50 at 10 at once;
  - 51/100 at 20 at once.

  Firecracker gives every sandbox its own kernel, a far stronger boundary
  than foil's namespaces on the host kernel. Compare speed and ease here,
  not security.
- **foil's times grow with how many sandboxes start at once.** That's about
  23 ms per extra concurrent start on this machine. Each sandbox's
  `foil run` makes its own systemd scope, and systemd makes them one at a
  time. Starting `foil serve` under `foil session` cut the 10-at-once p50
  from 233 to 141 ms in a quick check. Making that the default is a planned
  fix.
