# foil on SWE-bench Lite: results of 2026-10-13

**Verdict: on all 35 tasks run, foil changed no test outcome.** With the
real fix applied inside a foil branch (`--protect --no-network --memory 2G`),
every target and regression test passed or failed exactly as it did without
foil; after `foil commit`, the same tests run on the real repository without
foil agreed again. Without the fix, the target tests failed inside foil, and
`foil abort` left the workspace byte-identical.

Setup: Ubuntu 26.04 VM (Linux 7.0, 4 vCPUs, 6 GB), foil 0.2.0 at `e64d8f2`
installed with `install.sh` (AppArmor restriction on, `foil setup` run by the
installer). 36 SWE-bench Lite tasks: 22 Django (3 per version 3.0–5.0, plus
one smoke-test task) and 14 sympy (one per version 1.0–1.13, plus one smoke-
test task); the "agent" is each
task's merged fix. Python 3.9 (3.11 for Django 5.0) via uv, not SWE-bench's
Docker images. Reproduce: `fetch.py`, then `run.py`, `extra.py`, `summarize.py`.

## Per-task checks

| Check | Passed |
| --- | --- |
| Fix under foil: every test outcome = baseline without foil | 35 / 35 |
| After commit, outside foil: every test outcome = baseline | 35 / 35 |
| Without the fix under foil: the target tests fail | 35 / 35 |
| After abort: workspace unchanged | 35 / 35 |
| While the branch exists: workspace unchanged | 35 / 35 |
| `foil diff` lists every file the patches touch | 35 / 35 |
| `foil commit` succeeds | 35 / 35 |

1 task skipped (sympy__sympy-11870: its tests run over 15 minutes on this
VM, with or without foil). 31 / 35 tasks are fully resolved in the baseline;
the other 4 fail identically with and without foil, because this environment
is not SWE-bench's (e.g. old sympy on Python 3.9). Full table: `summary.md`.

## Agents in parallel, conflicts, and kill -9 (django__django-11099)

| Experiment | Result |
| --- | --- |
| 6 branches of one checkout at once (3 with the fix, 3 without) | 1.97 s wall; fixed ones pass, unfixed fail, no branch sees another's files, workspace unchanged |
| Commit one fixed branch, then another changing the same files | first applied; second refused, naming exactly the 2 real files |
| SIGKILL foil during the full Django suite (4 workers) | 7 test processes running; 5 survive foil's death; `foil abort` stops all 5; workspace unchanged |

## Speed (Django checkout: 60 MB, 6 072 files)

| Private copy | min | median |
| --- | --- | --- |
| `cp -a` | 320 ms | 327 ms |
| `git worktree add` | 453 ms | 490 ms |
| `foil run -b` + `true` (with the ~systemd scope) | 18 ms | 25 ms |
| same, `FOIL_NO_SCOPE=1` | 14 ms | 17 ms |

The task's test run took 0.50 s without foil and 0.52 s inside foil
(median of 5, interleaved): no measurable overhead.

## What the run found in foil (fixed in `e64d8f2`)

Committing a branch also committed the run's `__pycache__/*.pyc`, and a
conflicting commit listed dozens of `.pyc` files before the real conflict.
`foil diff` and `foil commit` now leave out paths `.gitignore` ignores
(`--all` restores them). Before the fix the conflict above listed `.pyc`
files first; after it, exactly the two source files.
