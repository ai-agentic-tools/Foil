# foil on SWE-bench Verified: a real agent, with and without foil (2026-11-06)

> Newer: `RESULTS-OPUS.md` (2026-11-08) reruns these 50 tasks three times per
> arm with Opus 5.5 and the false-conflict fix: 42.0 vs 42.7 mean, 150 / 150
> commits.

**Verdict: Claude Code solved 35 / 50 SWE-bench Verified tasks inside foil's
strict sandbox and 33 / 50 without it, at about the same cost and time. The
real workspace stayed unchanged until `foil commit` on every task, and the API
key never reached the agent.**

Setup: Ubuntu 26.04 VM (4 vCPUs, 6 GB), foil 0.4.1 (PR #13), Claude Code
2.1.302, model `claude-haiku-4-5-20251001`, `--max-turns 60`, two tasks at a
time. Same harness and arms as the Lite run (`RESULTS-STAGE2.md`): **without
foil** is `claude -p` in a scratch checkout; **with foil** is `foil agent
claude` in strict mode (no home contents, no other repositories, network only
to api.anthropic.com through foil's proxy, which holds the API key), then
`foil commit`. The hidden tests are applied and run outside foil afterwards.

Tasks: Django and sympy from `princeton-nlp/SWE-bench_Verified`, up to 4 per
Django version and 2 per sympy version (54). Stage 1 ran each task's merged
fix with and without foil; the 50 whose fix resolves in this environment went
to stage 2, so a failure there is the agent's, not the environment's.

Reproduce: `FOIL=path/to/foil verified.sh WORKDIR`, then `summarize.py
WORKDIR/results.jsonl` and `agent_summary.py WORKDIR/agent-results.jsonl`. Raw
results: `verified-results-2026-11-06.jsonl` (stage 1) and
`verified-agent-results-2026-11-06.jsonl` (stage 2).

## Stage 2: the agent

| | without foil | with foil (strict) |
| --- | --- | --- |
| Resolved | 33 / 50 | 35 / 50 |
| Median agent time | 136 s | 153 s |
| Median turns | 33.5 | 33.5 |
| Total cost | $13.60 | $14.96 |
| Stopped by the 60-turn limit or an error | 5 | 5 |

Total API cost for both arms: $28.56. 44 of 50 tasks had the same outcome in
both arms; 4 were solved only with foil, 2 only without. Each arm ran once and
an agent is not deterministic, so a gap of 2 tasks is noise, not an effect of
foil. On the Lite run it was also 2, the same way round (19 vs 17 of 31).

| foil check (foil arm) | Result |
| --- | --- |
| Real workspace unchanged until `foil commit` | 50 / 50 |
| `foil commit` succeeded | 49 / 50 |
| API key in anything the agent printed | 0 |
| Hosts the proxy blocked | none |

The one refused commit (`sympy__sympy-14976`, unsolved in both arms) was
refused as a conflict: foil reported `sympy/printing/pycode.py`, the file the
agent edited, as also changed in the workspace since the branch was made. The
workspace's content was unchanged (the fingerprint check passed), and the
harness checks the workspace out moments before it starts the branch. The likely
cause is foil's documented fallback: when the kernel's coarse clock trails by
more than 20 ms, the branch keeps the coarse time, so a file written just
before it can look newer. That is a false conflict. It errs on the safe side
(nothing is overwritten without `--force`), but it is a rough edge worth fixing.

## Stage 1: each task's merged fix

| Check | Passed |
| --- | --- |
| Fix under foil: every test outcome = baseline | 53 / 54 |
| After commit, outside foil: every test outcome = baseline | 53 / 54 |
| Without the fix under foil: the target tests fail | 54 / 54 |
| After abort: workspace unchanged | 54 / 54 |
| While the branch exists: workspace unchanged | 54 / 54 |
| foil diff lists every file the patches touch | 54 / 54 |
| foil commit succeeds | 54 / 54 |

The one mismatch (`sympy__sympy-13091`) is in the baseline run, not under
foil: `test_bug_sqrt` hit a `RecursionError` in the baseline run and passed
both under foil and after commit outside foil. Baseline resolved 50 / 54; the
other 4 fail the same way with and without foil.

## Per task (stage 2)

| Task | without foil | with foil |
| --- | --- | --- |
| django__django-10554 | no (319 s, 61 turns) | no (185 s, 61 turns) |
| django__django-10880 | yes (62 s, 18 turns) | yes (85 s, 26 turns) |
| django__django-10973 | yes (63 s, 17 turns) | yes (64 s, 16 turns) |
| django__django-11141 | no (79 s, 15 turns) | no (178 s, 47 turns) |
| django__django-11790 | no (164 s, 41 turns) | yes (219 s, 47 turns) |
| django__django-11815 | yes (97 s, 24 turns) | yes (179 s, 41 turns) |
| django__django-11820 | no (122 s, 42 turns) | no (109 s, 26 turns) |
| django__django-12754 | yes (236 s, 61 turns) | yes (185 s, 61 turns) |
| django__django-12965 | yes (170 s, 45 turns) | yes (74 s, 24 turns) |
| django__django-13012 | yes (206 s, 50 turns) | yes (175 s, 44 turns) |
| django__django-13023 | no (56 s, 18 turns) | yes (115 s, 34 turns) |
| django__django-13809 | yes (120 s, 39 turns) | yes (98 s, 21 turns) |
| django__django-13925 | yes (110 s, 35 turns) | yes (218 s, 61 turns) |
| django__django-13933 | yes (82 s, 21 turns) | yes (69 s, 17 turns) |
| django__django-13964 | yes (354 s, 61 turns) | yes (408 s, 52 turns) |
| django__django-14725 | no (364 s, 57 turns) | no (265 s, 59 turns) |
| django__django-14771 | yes (107 s, 27 turns) | yes (92 s, 18 turns) |
| django__django-14787 | yes (95 s, 25 turns) | yes (43 s, 11 turns) |
| django__django-14915 | yes (61 s, 16 turns) | yes (79 s, 19 turns) |
| django__django-15731 | yes (58 s, 18 turns) | yes (62 s, 17 turns) |
| django__django-15732 | no (200 s, 56 turns) | no (151 s, 40 turns) |
| django__django-15741 | yes (134 s, 38 turns) | yes (89 s, 23 turns) |
| django__django-15814 | yes (104 s, 27 turns) | yes (83 s, 27 turns) |
| django__django-16454 | no (253 s, 42 turns) | no (311 s, 38 turns) |
| django__django-16485 | yes (236 s, 25 turns) | yes (92 s, 16 turns) |
| django__django-16493 | yes (120 s, 29 turns) | yes (91 s, 20 turns) |
| django__django-16502 | no (158 s, 40 turns) | yes (152 s, 40 turns) |
| sympy__sympy-11618 | yes (106 s, 16 turns) | yes (70 s, 19 turns) |
| sympy__sympy-12096 | yes (161 s, 38 turns) | yes (158 s, 28 turns) |
| sympy__sympy-13031 | yes (182 s, 49 turns) | no (204 s, 61 turns) |
| sympy__sympy-14976 | no (147 s, 32 turns) | no (156 s, 41 turns) |
| sympy__sympy-15017 | yes (102 s, 29 turns) | yes (117 s, 33 turns) |
| sympy__sympy-15345 | yes (81 s, 25 turns) | yes (119 s, 37 turns) |
| sympy__sympy-15349 | yes (138 s, 15 turns) | yes (87 s, 13 turns) |
| sympy__sympy-16450 | yes (155 s, 33 turns) | yes (192 s, 26 turns) |
| sympy__sympy-16597 | no (216 s, 61 turns) | no (217 s, 61 turns) |
| sympy__sympy-18189 | yes (95 s, 15 turns) | yes (82 s, 20 turns) |
| sympy__sympy-18199 | no (126 s, 26 turns) | no (164 s, 44 turns) |
| sympy__sympy-19346 | yes (46 s, 12 turns) | yes (61 s, 15 turns) |
| sympy__sympy-19495 | yes (346 s, 53 turns) | yes (328 s, 50 turns) |
| sympy__sympy-20428 | no (272 s, 58 turns) | no (248 s, 54 turns) |
| sympy__sympy-20438 | no (222 s, 61 turns) | no (774 s, 6 turns) |
| sympy__sympy-21379 | no (174 s, 31 turns) | yes (155 s, 25 turns) |
| sympy__sympy-21596 | no (216 s, 45 turns) | no (222 s, 44 turns) |
| sympy__sympy-22080 | no (225 s, 50 turns) | no (224 s, 50 turns) |
| sympy__sympy-22456 | yes (351 s, 47 turns) | yes (284 s, 58 turns) |
| sympy__sympy-23262 | yes (72 s, 25 turns) | yes (76 s, 20 turns) |
| sympy__sympy-23413 | yes (128 s, 34 turns) | no (526 s, 41 turns) |
| sympy__sympy-23824 | yes (52 s, 9 turns) | yes (52 s, 8 turns) |
| sympy__sympy-23950 | yes (141 s, 49 turns) | yes (186 s, 46 turns) |
