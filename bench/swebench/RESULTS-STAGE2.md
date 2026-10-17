# foil on SWE-bench Lite, stage 2: a real agent (2026-10-17)

**Verdict: Claude Code solved as many tasks inside foil's strict sandbox as
without it (19 / 31 vs 17 / 31), at the same cost, and foil's guarantees held
on every task.** The first run found a foil bug that handicapped the agent;
it was fixed (`6c6d7d1`) and the foil arm re-run.

Setup: Ubuntu 26.04 VM (4 vCPUs, 6 GB), Claude Code 2.1.302, model
`claude-haiku-4-5-20251001`, `--max-turns 60`, tools Bash/Read/Edit/Write/Glob/Grep,
`--permission-mode acceptEdits`. The 31 tasks are those from stage 1 whose
merged fix resolves in this environment (so a failure is the agent's, not the
environment's). The agent sees the issue text, the path of the project's Python
and how to run existing tests; never the hidden tests. Afterwards the hidden
tests are applied and run outside foil, as SWE-bench does.

- **without foil**: `claude -p ...` in a scratch checkout.
- **with foil**: `foil agent claude --allow-read <python env> --allow-read <uv pythons>
  --allow-read <repo .git> -- ...`: strict view (no home contents, no other
  repositories), PID namespace, network only to api.anthropic.com through foil's
  proxy, which injects the API key (the agent sees a placeholder). Then
  `foil commit`.

Reproduce: `agent.py LITE_JSONL WORKDIR STAGE1_RESULTS`, then `agent_summary.py`.

## Results

| | without foil | with foil (strict) |
| --- | --- | --- |
| Resolved | 17 / 31 | 19 / 31 |
| Median agent time | 178 s | 152 s |
| Median turns | 48 | 38 |
| Total API cost | $9.97 | $10.54 |
| Stopped by the 60-turn limit | 9 | 10 |

27 of 31 tasks had the same outcome in both arms; 3 were solved only with foil,
1 only without. An agent is not deterministic (each arm ran once), so a
difference of 2 tasks is noise, not an effect of foil. Per-task table:
`agent-summary.md`.

| foil check (foil arm) | Result |
| --- | --- |
| Real workspace unchanged until `foil commit` | 31 / 31 |
| `foil commit` succeeded | 31 / 31 |
| API key printed anywhere the agent's output reached | 0 |
| Hosts the proxy blocked | none (after the fix) |
| Tool errors that only the sandbox caused | 2 (a `git commit`, below) |

Transcript scan (all tool calls, both arms): the agent ran the project's Python
914 times with foil and 956 without, failing about as often (186 / 186: tests
that fail while it works); system `python3` 36 vs 22 times.

## What the first run found in foil (fixed in `6c6d7d1`)

The first foil run solved 16 / 31, but its transcripts showed the project's
Python failing with "No such file or directory" 80 times. The harness allowed
`~/swe/final/envs/<env>`, where `envs` is a symlink; strict mode showed allowed
paths only at their canonical location, so the given path did not exist in the
view. The agent fell back to system `python3` (no project libraries) and tried
`pip install` (blocked: pypi.org is not on the allowlist). `--allow-read` and
`--allow-write` now show a path given through a symlink at that path too. The
first run's results are kept in `agent-results-2026-10-17-run1-symlink-bug.jsonl`.

## Remaining friction

- `git commit` by the agent (2 attempts, 1 task) fails: the checkouts are git
  worktrees whose git folder lives outside the workspace, which the harness shows
  read-only. In a normal clone `.git` is inside the workspace and writable in the
  branch. foil was right to refuse.
