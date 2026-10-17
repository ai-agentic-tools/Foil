"""SWE-bench Lite, stage 2: a real coding agent (Claude Code), with and without foil.

Stage 1 (run.py) used each task's merged fix as the "agent". Here Claude Code
reads the issue and writes its own fix. Each task runs twice:

  bare  no foil: Claude Code edits a scratch checkout directly.
  foil  `foil agent claude`: the whole session runs in a strict branch (only
        the workspace, the Python environment and Claude Code's own files are
        visible; network only to api.anthropic.com through foil's proxy, which
        adds the API key; the agent sees a placeholder). The workspace must
        stay unchanged until `foil commit`.

Then the task's hidden tests are applied and run outside foil, as SWE-bench
does. An agent is not deterministic, so single tasks may differ between the
two arms; the comparison is the resolved rate, plus whether foil ever got in
the agent's way (tool errors, invisible files, blocked hosts).

Usage: agent.py LITE_JSONL WORKDIR STAGE1_RESULTS [--model M] [--workers N] [--only ID ...]
Needs Claude Code on PATH and the API key in ~/.secrets/anthropic_key.
Writes WORKDIR/agent-results.jsonl (one line per task and arm).
"""

import argparse
import concurrent.futures
import json
import os
import re
import subprocess
import threading
import time

from run import FOIL, checkout, ensure_env, ensure_repo, fingerprint, outcome, sh, test_command, TEST_TIMEOUT

AGENT_TIMEOUT = 1200
MAX_TURNS = 60
TOOLS = "Bash,Read,Edit,Write,Glob,Grep"

PROMPT = """You are fixing an issue in the {repo} repository, checked out in the current folder.

<issue>
{problem}
</issue>

Change the source code so the issue is fixed. Do not edit or add tests; hidden tests will check your fix.
Python with the project's dependencies: {py} (e.g. `{py} -c "..."` to reproduce the issue).
{how_to_test}
When you are done, reply with one line summarizing the change."""


def how_to_test(task, py):
    if task["repo"] == "django/django":
        return f"Existing tests: `cd tests && PYTHONPATH=.. {py} runtests.py --settings=test_sqlite --parallel 1 <app>`."
    return f"Existing tests: `PYTHONPATH=. {py} bin/test <path/to/test_file.py>`."


def agent_command(task, env_dir, model, arm, ws, work):
    py = f"{env_dir}/bin/python"
    prompt = PROMPT.format(repo=task["repo"], problem=task["problem_statement"], py=py, how_to_test=how_to_test(task, py))
    claude_args = ["--model", model, "--max-turns", str(MAX_TURNS), "--output-format", "json",
                   "--permission-mode", "acceptEdits", "--allowedTools", TOOLS, "-p", prompt]
    if arm == "bare":
        return ["claude"] + claude_args
    repo_git = f"{work}/repos/{task['repo'].split('/')[1]}/.git"
    uv_pythons = os.path.expanduser("~/.local/share/uv/python")
    return [FOIL, "agent", "claude", "-b", branch_name(task),
            "--allow-read", env_dir, "--allow-read", uv_pythons, "--allow-read", repo_git,
            "--"] + claude_args


def branch_name(task):
    return task["instance_id"].replace("__", "-")[:56] + "-agent"


def run_agent(cmd, ws, env):
    start = time.monotonic()
    try:
        p = subprocess.run(cmd, cwd=ws, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                           stderr=subprocess.PIPE, text=True, errors="replace", timeout=AGENT_TIMEOUT)
        rc, out, err = p.returncode, p.stdout, p.stderr
    except subprocess.TimeoutExpired as e:
        rc, out, err = "timeout", e.stdout or "", e.stderr or ""
        out = out.decode(errors="replace") if isinstance(out, bytes) else out
        err = err.decode(errors="replace") if isinstance(err, bytes) else err
    took = time.monotonic() - start
    try:
        result = json.loads(out.strip().splitlines()[-1])
    except (ValueError, IndexError):
        result = {}
    return rc, result, err, took


def evaluate(task, ws, env_dir, patches):
    """Apply the hidden tests over the agent's work and run them, outside foil."""
    test_files = re.findall(r"^diff --git a/(\S+) b/", task["test_patch"], re.M)
    # The agent was told not to touch tests; restore them so the test patch applies.
    sh(f"git checkout -q HEAD -- {' '.join(test_files)} 2>/dev/null; git clean -qfd -- {' '.join(test_files)}",
       cwd=ws, check=False)
    rc, out, _ = sh(f"git apply {patches}/test.patch", cwd=ws, check=False)
    if rc != 0:
        return {"error": "test patch does not apply: " + out[-300:]}
    try:
        _, out, _ = sh(test_command(task, env_dir), cwd=ws, timeout=TEST_TIMEOUT, check=False)
    except subprocess.TimeoutExpired:
        return {"error": "tests timed out"}
    return outcome(task, out)


def run_arm(task, arm, work, model, key):
    tid = task["instance_id"]
    repo_path = ensure_repo(work, task["repo"])
    env_dir = ensure_env(work, task)
    patches = f"{work}/patches/{tid}"
    os.makedirs(patches, exist_ok=True)
    open(f"{patches}/test.patch", "w").write(task["test_patch"])
    rec = {"id": tid, "repo": task["repo"], "arm": arm, "model": model}

    ws = f"{work}/agent-ws/{arm}/{tid}"
    os.makedirs(os.path.dirname(ws), exist_ok=True)
    checkout(repo_path, ws, task["base_commit"])
    before = fingerprint(ws)
    env = dict(os.environ, ANTHROPIC_API_KEY=key, FOIL_HOME=f"{work}/foil-home-agent",
               CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1", DISABLE_AUTOUPDATER="1")
    if arm == "foil":
        sh(f"{FOIL} abort {branch_name(task)}", cwd=ws, env=env, check=False)

    rc, result, err, took = run_agent(agent_command(task, env_dir, model, arm, ws, work), ws, env)
    open(f"{patches}/agent-{arm}.stderr", "w").write(err)
    open(f"{patches}/agent-{arm}.json", "w").write(json.dumps(result, indent=1))
    rec.update(agent_rc=rc, agent_s=round(took, 1), turns=result.get("num_turns"),
               cost_usd=result.get("total_cost_usd"), agent_error=result.get("is_error"),
               summary=(result.get("result") or "")[-300:])
    rec["key_in_stderr"] = key in err

    if arm == "foil":
        rec["ws_unchanged_before_commit"] = fingerprint(ws) == before
        _, diff, _ = sh(f"{FOIL} diff {branch_name(task)}", cwd=ws, env=env, check=False)
        rec["branch_changes"] = len([l for l in diff.splitlines() if l[:1] in "AMDR"])
        rec["blocked_hosts"] = sorted(set(re.findall(r"blocked network access to (\S+)", err)))
        rc, out, _ = sh(f"{FOIL} commit {branch_name(task)}", cwd=ws, env=env, check=False)
        rec["commit_ok"] = rc == 0 or "no changes" in out or rec["branch_changes"] == 0
        if rec["branch_changes"] == 0:
            sh(f"{FOIL} abort {branch_name(task)}", cwd=ws, env=env, check=False)

    _, stat, _ = sh("git diff --stat | tail -1", cwd=ws, check=False)
    rec["diff_stat"] = stat.strip()
    _, patch, _ = sh("git diff", cwd=ws, check=False)
    open(f"{patches}/agent-{arm}.patch", "w").write(patch)
    rec["eval"] = evaluate(task, ws, env_dir, patches)
    rec["resolved"] = rec["eval"].get("resolved", False)
    sh(f"git -C {repo_path} worktree remove --force {ws}", check=False)
    return rec


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("lite")
    ap.add_argument("work")
    ap.add_argument("stage1", help="stage-1 results.jsonl: only tasks whose gold fix resolves there run")
    ap.add_argument("--model", default="claude-haiku-4-5-20251001")
    ap.add_argument("--workers", type=int, default=2)
    ap.add_argument("--only", nargs="*")
    args = ap.parse_args()
    key = open(os.path.expanduser("~/.secrets/anthropic_key")).read().strip()
    ok = {r["id"] for r in map(json.loads, open(args.stage1)) if r.get("baseline", {}).get("resolved")}
    tasks = [t for t in map(json.loads, open(args.lite)) if t["instance_id"] in (set(args.only) if args.only else ok)]
    args.work = os.path.abspath(args.work)
    out_path = f"{args.work}/agent-results.jsonl"
    done = set()
    if os.path.exists(out_path):
        done = {(r["id"], r["arm"]) for r in map(json.loads, open(out_path))}
    jobs = [(t, arm) for t in tasks for arm in ("foil", "bare") if (t["instance_id"], arm) not in done]
    print(f"{len(tasks)} tasks, {len(jobs)} runs to do, model {args.model}", flush=True)
    lock = threading.Lock()

    def one(job):
        task, arm = job
        try:
            rec = run_arm(task, arm, args.work, args.model, key)
        except Exception as err:
            rec = {"id": task["instance_id"], "arm": arm, "harness_error": str(err)[-2000:]}
        with lock:
            with open(out_path, "a") as f:
                f.write(json.dumps(rec) + "\n")
            print(f"{rec['id']} [{arm}] resolved={rec.get('resolved')} {rec.get('agent_s')} s "
                  f"${rec.get('cost_usd')} turns={rec.get('turns')} {rec.get('diff_stat', '')}"
                  f"{' ERROR ' + rec['harness_error'][:200] if 'harness_error' in rec else ''}", flush=True)

    with concurrent.futures.ThreadPoolExecutor(args.workers) as pool:
        list(pool.map(one, jobs))


if __name__ == "__main__":
    main()
