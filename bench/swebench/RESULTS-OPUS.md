# foil on SWE-bench Verified: Opus 5.5, three runs (2026-11-08)

**Verdict: over three runs of the same 50 SWE-bench Verified tasks, Claude Code
(Opus 5.5) solved a mean of 42.0 / 50 inside foil's strict sandbox (41–43) and
42.7 / 50 without it (41–44). The ranges overlap, so the sandbox didn't cost
anything we could measure. In all 150 sandboxed runs, the real workspace stayed
unchanged until `foil commit`, every commit succeeded, and the API key never
reached the agent.**

This follows up the single Haiku 4.5 run in `RESULTS-VERIFIED.md` (35 vs 33 of
50). It changes three things:

1. **A stronger model:** `claude-opus-5-5` instead of Haiku 4.5.
2. **Three runs per arm instead of one,** so this report gives a mean and a
   range rather than one draw from a noisy agent.
3. **foil with the false-conflict fix (PR #16).** In the Haiku run, one commit
   was refused as a conflict when nothing had conflicted. Here 150 of 150
   commits succeeded.

Setup:

- **Machine:** Ubuntu 26.04 VM (Linux 7.0, 4 vCPUs, 6 GB).
- **Software:** foil 0.5.0 plus PR #16, Claude Code 2.1.302, `--max-turns 100`.
- **Run limits:** 30-minute limit per agent run, two tasks at a time.
- **Same as `RESULTS-VERIFIED.md`:** the tasks, both arms, and how they are
  graded. That means the 50 tasks whose merged fix resolves in this
  environment, and hidden tests applied and run outside foil afterwards.
- **The arms:**
  - **without foil** is `claude -p` in a scratch checkout.
  - **with foil** is `foil agent claude` in strict mode, then `foil commit`.
    Strict mode means no home contents, no other repositories, and network
    only to api.anthropic.com through foil's proxy, which holds the key.

Reproduce: `FOIL=path/to/foil python3 agent.py TASKS WORKDIR STAGE1 --model
claude-opus-5-5 --max-turns 100 --tag opus-rN` for N = 1, 2, 3, then
`repeat_summary.py WORKDIR/agent-results-opus-r*.jsonl`. Raw results are in
`verified-agent-results-2026-11-08-opus-r{1,2,3}.jsonl`.

## Resolved

| | run 1 | run 2 | run 3 | mean | range |
| --- | --- | --- | --- | --- | --- |
| without foil | 43 / 50 | 44 / 50 | 41 / 50 | 42.7 | 41–44 |
| with foil (strict) | 41 / 50 | 43 / 50 | 42 / 50 | 42.0 | 41–43 |

| | without foil | with foil |
| --- | --- | --- |
| Solved in every run | 41 | 41 |
| Solved in some runs | 4 | 2 |
| Solved in no run | 5 | 7 |
| Median agent time | 17 s | 17 s |
| Total cost, 3 runs | $14.03 | $15.54 |
| Stopped by the 30-minute limit | 1 of 150 | 2 of 150 |

Total API cost for all six arm-runs: $29.57.

The two arms differ by 2 of 150 task-runs. Every task outside the four below
had the same result in all six runs, three per arm.

- **`sympy__sympy-20428`:** 2 of 3 without foil, 0 of 3 with foil. Two fixes
  look right here, and only one passes the hidden tests:
  - Fix `__bool__` in `expressiondomain.py`: passes.
  - Strip zeros in `densearith.py`: fails.

  Without foil, the agent chose the first fix twice and the second once (the
  one run it lost). With foil, it chose the second all three times. Neither
  arm's transcripts show a sandbox error. This is the model picking between
  two plausible fixes, not the sandbox getting in the way.
- **`sympy__sympy-20438`:** 1 of 3 without foil, 0 of 3 with foil.
- **`sympy__sympy-22080`:** 1 of 3 without foil, 2 of 3 with foil.
- **`sympy__sympy-16597`:** 1 of 3 without foil, 1 of 3 with foil. It also
  caused all three 30-minute timeouts.

A search of every sandboxed transcript found no `permission denied`,
`operation not permitted`, `read-only file system`, or blocked host. Nothing
the agent tried was refused by the sandbox.

## foil checks (all 150 sandboxed runs)

| Check | Result |
| --- | --- |
| Real workspace unchanged until `foil commit` | 150 / 150 |
| `foil commit` succeeded | 150 / 150 |
| API key in anything the agent printed | 0 |
| Hosts the proxy blocked | none |

## The VM clock (disclosure)

Two things set this VM's wall clock against each other:

- Lima's guest agent stepped the clock about 340 ms back every 10 seconds.
- chrony slewed it the other way.

foil decides whether a file changed after the branch was made by comparing
times, so a clock that jumps backwards can make an old change look new. That
caused the Haiku run's one refused commit.

**What PR #16 changes in foil:**

- foil records how far the clock has been set back since each branch was
  made.
- It widens the conflict check by that much, so a set-back clock can't hide a
  real change.
- When the coarse clock lags, it keeps a fingerprint of each file changed just
  before the branch, so those files don't count as changes.

When a step makes the call unclear, foil still refuses to commit rather than
overwrite.

**What we changed in the VM for this run:**

- We stopped chrony and reset the kernel's tick and frequency adjustments.
  That ended the tug-of-war between the two.
- The guest agent kept running. During the three runs (23:30 to 02:31 UTC,
  about 3 hours), it stepped the clock three times, each by about 100–110 ms.
- No commit was refused.

If you run foil in a VM, let one time service own the clock. See README,
Limits.

## Per task

`y` = resolved, `.` = not, one letter per run.

| Task | without foil | with foil |
| --- | --- | --- |
| django__django-10554 | yyy | yyy |
| django__django-10880 | yyy | yyy |
| django__django-10973 | yyy | yyy |
| django__django-11141 | ... | ... |
| django__django-11790 | yyy | yyy |
| django__django-11815 | yyy | yyy |
| django__django-11820 | ... | ... |
| django__django-12754 | yyy | yyy |
| django__django-12965 | yyy | yyy |
| django__django-13012 | yyy | yyy |
| django__django-13023 | yyy | yyy |
| django__django-13809 | yyy | yyy |
| django__django-13925 | yyy | yyy |
| django__django-13933 | yyy | yyy |
| django__django-13964 | yyy | yyy |
| django__django-14725 | yyy | yyy |
| django__django-14771 | yyy | yyy |
| django__django-14787 | yyy | yyy |
| django__django-14915 | yyy | yyy |
| django__django-15731 | yyy | yyy |
| django__django-15732 | yyy | yyy |
| django__django-15741 | yyy | yyy |
| django__django-15814 | yyy | yyy |
| django__django-16454 | yyy | yyy |
| django__django-16485 | yyy | yyy |
| django__django-16493 | yyy | yyy |
| django__django-16502 | ... | ... |
| sympy__sympy-11618 | yyy | yyy |
| sympy__sympy-12096 | yyy | yyy |
| sympy__sympy-13031 | yyy | yyy |
| sympy__sympy-14976 | yyy | yyy |
| sympy__sympy-15017 | yyy | yyy |
| sympy__sympy-15345 | yyy | yyy |
| sympy__sympy-15349 | yyy | yyy |
| sympy__sympy-16450 | yyy | yyy |
| sympy__sympy-16597 | y.. | .y. |
| sympy__sympy-18189 | yyy | yyy |
| sympy__sympy-18199 | ... | ... |
| sympy__sympy-19346 | yyy | yyy |
| sympy__sympy-19495 | yyy | yyy |
| sympy__sympy-20428 | yy. | ... |
| sympy__sympy-20438 | .y. | ... |
| sympy__sympy-21379 | yyy | yyy |
| sympy__sympy-21596 | ... | ... |
| sympy__sympy-22080 | .y. | .yy |
| sympy__sympy-22456 | yyy | yyy |
| sympy__sympy-23262 | yyy | yyy |
| sympy__sympy-23413 | yyy | yyy |
| sympy__sympy-23824 | yyy | yyy |
| sympy__sympy-23950 | yyy | yyy |

Five tasks went unsolved in every run of both arms: django-11141,
django-11820, django-16502, sympy-18199 and sympy-21596. So 50 / 50 is not
within reach for this model and harness, with or without foil.
