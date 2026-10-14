"""Verify foil on SWE-bench Lite tasks (Django and sympy), without an AI agent.

For each task, the "agent" is the task's own merged fix (the gold patch).
Each task runs four times:

  baseline  no foil: apply test patch + gold patch in a scratch checkout, run
            the FAIL_TO_PASS and PASS_TO_PASS tests.
  neg       foil, branch `neg`, --protect --no-network --memory: apply the
            test patch only. FAIL_TO_PASS must still fail; then `foil abort`,
            and the workspace must be unchanged.
  gold      foil, branch `gold`, same flags: test patch + gold patch. Every
            test must have the same outcome as in the baseline.
  commit    `foil commit gold`, then the same tests run on the real workspace
            without foil: outcomes must match the baseline again.

Usage: run.py LITE_JSONL WORKDIR [--per-version N] [--only ID ...]
Writes WORKDIR/results.jsonl (one line per task) and prints a summary.
"""

import argparse
import collections
import concurrent.futures
import json
import os
import re
import shutil
import subprocess
import sys
import threading
import time

FOIL = os.environ.get("FOIL", "foil")
FOIL_FLAGS = ["--protect", "--no-network", "--memory", "2G"]
TEST_TIMEOUT = 900

REPOS = {
    "django/django": "https://github.com/django/django.git",
    "sympy/sympy": "https://github.com/sympy/sympy.git",
}


def python_for(task):
    if task["repo"] == "django/django" and task["version"] >= "5.0":
        return "3.11"
    return "3.9"


def deps_for(task):
    if task["repo"] == "django/django":
        return ["asgiref", "sqlparse", "pytz"]
    return ["mpmath==1.3.0"]


def sh(cmd, cwd=None, env=None, timeout=None, check=True):
    """Run a shell command; returns (rc, combined output, seconds)."""
    start = time.monotonic()
    p = subprocess.run(cmd, shell=True, cwd=cwd, env=env, timeout=timeout,
                       stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, errors="replace")
    took = time.monotonic() - start
    if check and p.returncode != 0:
        raise RuntimeError(f"failed ({p.returncode}): {cmd}\n{p.stdout[-3000:]}")
    return p.returncode, p.stdout, took


# ---------------------------------------------------------------- tests

def test_command(task, env_dir):
    """The shell command that runs this task's tests, from the workspace root."""
    py = f"{env_dir}/bin/python"
    files = re.findall(r"^diff --git a/(\S+) b/", task["test_patch"], re.M)
    if task["repo"] == "django/django":
        labels = []
        for f in files:
            if f.startswith("tests/") and f.endswith(".py"):
                label = f[len("tests/"):-len(".py")].replace("/", ".")
                labels.append(label)
        labels = sorted(set(labels))
        return f"cd tests && PYTHONPATH=.. {py} runtests.py --verbosity 2 --settings=test_sqlite --parallel 1 {' '.join(labels)}"
    tests = sorted({f for f in files if f.endswith(".py")})
    # Old sympy triggers SyntaxWarnings on first compile, printed mid-line.
    return f"PYTHONWARNINGS=ignore PYTHONPATH=. {py} bin/test -C --verbose {' '.join(tests)}"


DJANGO_STATUS = re.compile(r" \.\.\. (ok|FAIL|ERROR|skipped|expected failure|unexpected success)")


def parse(task, output):
    """Map test id -> passed (bool), in SWE-bench's id format."""
    results = {}
    if task["repo"] == "django/django":
        pending = None
        for line in output.splitlines():
            m = re.match(r"^(\w+) \(([\w.]+)\)", line)
            status = DJANGO_STATUS.search(line)
            if m:
                name, inner = m.groups()
                keys = [f"{name} ({inner})"]
                # Python >= 3.11 prints (module.Class.test); SWE-bench may use either.
                if inner.endswith("." + name):
                    keys.append(f"{name} ({inner[: -len(name) - 1]})")
                pending = None if status else keys
                if status:
                    for k in keys:
                        results[k] = status.group(1) in ("ok", "skipped", "expected failure")
            elif pending and status:
                # A docstring line: SWE-bench names the test by that line's text.
                passed = status.group(1) in ("ok", "skipped", "expected failure")
                for k in pending + [line[: status.start()].strip()]:
                    results[k] = passed
                pending = None
    else:
        for line in output.splitlines():
            m = re.match(r"^(test_\w+) (ok|F|E|f|s|X|w|T)\b", line)
            if m:
                results[m.group(1)] = m.group(2) in ("ok", "f", "s")
    return results


def outcome(task, output):
    """{"f2p": {id: bool}, "p2p": {...}, "resolved": bool}."""
    parsed = parse(task, output)
    f2p = {t: parsed.get(t, False) for t in json.loads(task["FAIL_TO_PASS"])}
    p2p = {t: parsed.get(t, False) for t in json.loads(task["PASS_TO_PASS"])}
    return {"f2p": f2p, "p2p": p2p, "resolved": all(f2p.values()) and all(p2p.values())}


# ---------------------------------------------------------------- setup

def ensure_repo(work, repo):
    path = f"{work}/repos/{repo.split('/')[1]}"
    if not os.path.isdir(path):
        sh(f"git clone -q {REPOS[repo]} {path}", timeout=3600)
    return path


def ensure_env(work, task):
    name = f"{task['repo'].split('/')[1]}-py{python_for(task)}"
    env_dir = f"{work}/envs/{name}"
    if not os.path.isdir(env_dir):
        uv = os.path.expanduser("~/.local/bin/uv")
        sh(f"{uv} venv -q --python {python_for(task)} {env_dir}", timeout=1800)
        sh(f"{uv} pip install -q --python {env_dir}/bin/python {' '.join(deps_for(task))}", timeout=1800)
    return env_dir


def checkout(repo_path, dest, commit):
    if os.path.exists(dest):
        sh(f"git -C {repo_path} worktree remove --force {dest}", check=False)
        shutil.rmtree(dest, ignore_errors=True)
    sh(f"git -C {repo_path} worktree add -q --detach {dest} {commit}")


def fingerprint(ws):
    """Content hash of the working tree (not .git), to prove it is unchanged."""
    _, out, _ = sh("git status --porcelain --untracked-files=all && git diff | sha1sum", cwd=ws)
    return out


# ---------------------------------------------------------------- one task

def run_task(task, work):
    tid = task["instance_id"]
    repo_path = ensure_repo(work, task["repo"])
    env_dir = ensure_env(work, task)
    patches = f"{work}/patches/{tid}"
    os.makedirs(patches, exist_ok=True)
    open(f"{patches}/test.patch", "w").write(task["test_patch"])
    open(f"{patches}/gold.patch", "w").write(task["patch"])
    tests = test_command(task, env_dir)
    apply_test = f"git apply {patches}/test.patch"
    apply_gold = f"git apply {patches}/gold.patch"
    rec = {"id": tid, "repo": task["repo"], "version": task["version"]}

    # Baseline, no foil.
    base = f"{work}/base/{tid}"
    checkout(repo_path, base, task["base_commit"])
    sh(f"{apply_test} && {apply_gold}", cwd=base)
    try:
        _, out, took = sh(tests, cwd=base, timeout=TEST_TIMEOUT, check=False)
    except subprocess.TimeoutExpired:
        sh(f"git -C {repo_path} worktree remove --force {base}", check=False)
        rec["skipped"] = f"baseline tests took over {TEST_TIMEOUT} s on this VM"
        return rec
    open(f"{patches}/baseline.log", "w").write(out)
    rec["baseline"] = outcome(task, out)
    rec["baseline_s"] = round(took, 1)
    sh(f"git -C {repo_path} worktree remove --force {base}", check=False)

    # The workspace foil branches.
    ws = f"{work}/ws/{tid}"
    checkout(repo_path, ws, task["base_commit"])
    before = fingerprint(ws)
    foil_env = dict(os.environ, FOIL_HOME=f"{work}/foil-home")
    neg_b, gold_b = f"{tid}-neg", f"{tid}-gold"
    foil = f"{FOIL} run -b {{b}} {' '.join(FOIL_FLAGS)} -- sh -c"

    # Negative: tests only, no fix.
    sh(f"{FOIL} abort {neg_b}", cwd=ws, env=foil_env, check=False)
    _, out, took = sh(f"{foil.format(b=neg_b)} '{apply_test} && {tests}'", cwd=ws, env=foil_env,
                      timeout=TEST_TIMEOUT, check=False)
    neg = outcome(task, out)
    rec["neg_f2p_fail"] = not all(neg["f2p"].values())
    rec["neg_s"] = round(took, 1)
    sh(f"{FOIL} abort {neg_b}", cwd=ws, env=foil_env)
    rec["neg_ws_unchanged"] = fingerprint(ws) == before

    # Gold under foil.
    sh(f"{FOIL} abort {gold_b}", cwd=ws, env=foil_env, check=False)
    _, out, took = sh(f"{foil.format(b=gold_b)} '{apply_test} && {apply_gold} && {tests}'", cwd=ws,
                      env=foil_env, timeout=TEST_TIMEOUT, check=False)
    open(f"{patches}/foil.log", "w").write(out)
    rec["foil"] = outcome(task, out)
    rec["foil_s"] = round(took, 1)
    rec["foil_matches_baseline"] = same(rec["foil"], rec["baseline"])
    rec["gold_ws_unchanged"] = fingerprint(ws) == before
    _, diff, _ = sh(f"{FOIL} diff {gold_b}", cwd=ws, env=foil_env)
    changed = {line[2:] for line in diff.splitlines() if line[:1] in "AMD"}
    patched = set(re.findall(r"^diff --git a/(\S+) b/", task["test_patch"] + task["patch"], re.M))
    rec["diff_covers_patches"] = patched <= changed

    # Commit, then run the tests on the real workspace without foil.
    rc, out, _ = sh(f"{FOIL} commit {gold_b}", cwd=ws, env=foil_env, check=False)
    rec["commit_ok"] = rc == 0
    _, out, _ = sh(tests, cwd=ws, timeout=TEST_TIMEOUT, check=False)
    open(f"{patches}/committed.log", "w").write(out)
    rec["committed"] = outcome(task, out)
    rec["committed_matches_baseline"] = same(rec["committed"], rec["baseline"])
    sh(f"git -C {repo_path} worktree remove --force {ws}", check=False)
    return rec


def same(a, b):
    return a["f2p"] == b["f2p"] and a["p2p"] == b["p2p"]


# ---------------------------------------------------------------- main

def pick(tasks, per_version, sympy_per_version):
    chosen = []
    seen = collections.Counter()
    for t in tasks:
        cap = sympy_per_version if t["repo"] == "sympy/sympy" else per_version
        if t["repo"] in REPOS and seen[(t["repo"], t["version"])] < cap:
            seen[(t["repo"], t["version"])] += 1
            chosen.append(t)
    return chosen


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("lite")
    ap.add_argument("work")
    ap.add_argument("--per-version", type=int, default=3)
    ap.add_argument("--only", nargs="*")
    ap.add_argument("--workers", type=int, default=2)
    ap.add_argument("--sympy-per-version", type=int, default=1)
    args = ap.parse_args()
    tasks = [json.loads(line) for line in open(args.lite)]
    tasks = [t for t in tasks if t["instance_id"] in args.only] if args.only else pick(tasks, args.per_version, args.sympy_per_version)
    args.work = os.path.abspath(args.work)
    os.makedirs(args.work, exist_ok=True)
    done = set()
    results_path = f"{args.work}/results.jsonl"
    if os.path.exists(results_path):
        done = {json.loads(line)["id"] for line in open(results_path)}
    print(f"{len(tasks)} tasks, {len(done)} already done", flush=True)
    todo = [t for t in tasks if t["instance_id"] not in done]
    # Clone and build environments up front, one at a time.
    for t in todo:
        ensure_repo(args.work, t["repo"])
        ensure_env(args.work, t)
    lock = threading.Lock()

    def one(task):
        try:
            rec = run_task(task, args.work)
        except Exception as err:  # keep going; record the harness failure
            rec = {"id": task["instance_id"], "repo": task["repo"], "version": task["version"],
                   "harness_error": str(err)[-2000:]}
        with lock:
            with open(results_path, "a") as f:
                f.write(json.dumps(rec) + "\n")
            report(rec, len(tasks))

    with concurrent.futures.ThreadPoolExecutor(args.workers) as pool:
        list(pool.map(one, todo))


def report(rec, total):
        flags = {k: v for k, v in rec.items() if isinstance(v, bool)}
        print(f"[{total}] {rec['id']}: "
              f"base={rec.get('baseline', {}).get('resolved')} foil={rec.get('foil', {}).get('resolved')} "
              f"{' '.join(k for k, v in flags.items() if not v) or 'all checks ok'}"
              f"{' ERROR ' + rec['harness_error'][:200] if 'harness_error' in rec else ''}"
              f"{' SKIPPED ' + rec['skipped'] if 'skipped' in rec else ''}", flush=True)


if __name__ == "__main__":
    sys.exit(main())
