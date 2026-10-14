"""Experiments on one Django SWE-bench task, beyond per-task correctness.

  parallel   6 branches of one workspace at once (3 with the fix, 3 without):
             each must get its own result, and see only its own changes.
  conflict   commit one fixed branch, then another that touches the same
             files: the second must be refused, naming the files.
  kill       SIGKILL foil while the test suite runs with 4 worker processes:
             nothing may survive `foil abort`, and the workspace is unchanged.
  timing     a private copy by cp -a, git worktree add, and foil branch;
             and the same test run with and without foil, interleaved.

Usage: extra.py LITE_JSONL WORKDIR TASK_ID
"""

import json
import os
import signal
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(__file__))
from run import FOIL, FOIL_FLAGS, checkout, ensure_env, ensure_repo, fingerprint, outcome, sh, test_command  # noqa: E402


def main():
    lite, work, tid = sys.argv[1], os.path.abspath(sys.argv[2]), sys.argv[3]
    task = next(json.loads(line) for line in open(lite) if json.loads(line)["instance_id"] == tid)
    repo = ensure_repo(work, task["repo"])
    env_dir = ensure_env(work, task)
    patches = f"{work}/patches/{tid}"
    os.makedirs(patches, exist_ok=True)
    open(f"{patches}/test.patch", "w").write(task["test_patch"])
    open(f"{patches}/gold.patch", "w").write(task["patch"])
    tests = test_command(task, env_dir)
    test_only = f"git apply {patches}/test.patch && {tests}"
    with_fix = f"git apply {patches}/test.patch && git apply {patches}/gold.patch && {tests}"
    ws = f"{work}/ws-extra/{tid}"
    checkout(repo, ws, task["base_commit"])
    env = dict(os.environ, FOIL_HOME=f"{work}/foil-home-extra")
    before = fingerprint(ws)
    report = {"task": tid}

    def foil_run(branch, script):
        return subprocess.Popen([FOIL, "run", "-b", branch, *FOIL_FLAGS, "--", "sh", "-c", script], cwd=ws, env=env,
                                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)

    # ---- parallel
    names = [f"fix-{i}" for i in range(3)] + [f"nofix-{i}" for i in range(3)]
    start = time.monotonic()
    procs = {n: foil_run(n, with_fix if n.startswith("fix") else test_only) for n in names}
    outs = {n: p.communicate()[0] for n, p in procs.items()}
    report["parallel_wall_s"] = round(time.monotonic() - start, 2)
    res = {n: outcome(task, outs[n]) for n in names}
    report["parallel_fix_f2p_pass"] = all(all(res[n]["f2p"].values()) for n in names if n.startswith("fix"))
    report["parallel_nofix_f2p_fail"] = all(not all(res[n]["f2p"].values()) for n in names if n.startswith("nofix"))
    gold_files = set(l[len("diff --git a/"):].split()[0] for l in task["patch"].splitlines() if l.startswith("diff --git"))
    leaks = []
    for n in names:
        _, d, _ = sh(f"{FOIL} diff {n}", cwd=ws, env=env)
        changed = {line[2:] for line in d.splitlines()}
        if n.startswith("nofix") and changed & gold_files:
            leaks.append(n)
    report["parallel_no_leaks"] = not leaks
    report["parallel_ws_unchanged"] = fingerprint(ws) == before

    # ---- conflict
    rc1, out1, _ = sh(f"{FOIL} commit fix-0", cwd=ws, env=env, check=False)
    rc2, out2, _ = sh(f"{FOIL} commit fix-1", cwd=ws, env=env, check=False)
    report["conflict_first_commit_ok"] = rc1 == 0
    report["conflict_second_refused"] = rc2 != 0 and "changed in" in out2
    report["conflict_message"] = out2.strip().splitlines()[:6]
    for n in names[1:]:
        sh(f"{FOIL} abort {n}", cwd=ws, env=env, check=False)
    sh("git checkout -q -- . && git clean -qfd", cwd=ws)
    before = fingerprint(ws)

    # ---- kill: the whole Django suite on 4 worker processes, killed mid-run
    long_tests = f"cd tests && PYTHONPATH=.. {env_dir}/bin/python runtests.py --settings=test_sqlite --parallel 4"
    count = "ps -eo args | grep -c '[r]untests.py'"
    p = foil_run("long", long_tests)
    deadline = time.monotonic() + 120
    running = 0
    while time.monotonic() < deadline:
        running = int(sh(count, check=False)[1].strip() or 0)
        if running >= 4:
            break
        time.sleep(0.5)
    time.sleep(3)
    running = int(sh(count, check=False)[1].strip() or 0)
    os.kill(p.pid, signal.SIGKILL)
    p.wait()
    time.sleep(1)
    ps_mid = sh(count, check=False)[1]
    rc, out, _ = sh(f"{FOIL} abort long", cwd=ws, env=env, check=False)
    time.sleep(0.5)
    _, ps_after, _ = sh(count, check=False)
    report["kill_test_processes_running"] = running
    report["kill_left_after_sigkill"] = int(ps_mid.strip() or 0)
    report["kill_left_after_abort"] = int(ps_after.strip() or 0)
    report["kill_abort_ok"] = rc == 0
    report["kill_ws_unchanged"] = fingerprint(ws) == before

    # ---- timing
    _, size, _ = sh(f"du -sh --exclude=.git {ws} | cut -f1; git ls-files | wc -l", cwd=ws)
    report["workspace"] = " ".join(size.split())

    def best(cmd, n=5, cwd=ws, setup=None):
        times = []
        for i in range(n):
            if setup:
                sh(setup.format(i=i), cwd=cwd, env=env, check=False)
            _, _, t = sh(cmd.format(i=i), cwd=cwd, env=env, check=False)
            times.append(t)
        return round(min(times) * 1000, 1), round(sorted(times)[len(times) // 2] * 1000, 1)

    report["cp_a_ms(min,median)"] = best(f"cp -a {ws} {work}/copy-{{i}}", setup=f"rm -rf {work}/copy-{{i}}")
    sh(f"rm -rf {work}/copy-*")
    report["git_worktree_add_ms"] = best(f"git -C {repo} worktree add -q --detach {work}/wt-{{i}} {task['base_commit']}",
                                         setup=f"git -C {repo} worktree remove --force {work}/wt-{{i}}")
    for i in range(5):
        sh(f"git -C {repo} worktree remove --force {work}/wt-{i}", check=False)
    report["foil_branch_run_true_ms"] = best(f"{FOIL} run -b t{{i}} -- true", setup=f"{FOIL} abort t{{i}}")
    report["foil_branch_run_true_noscope_ms"] = best(f"FOIL_NO_SCOPE=1 {FOIL} run -b n{{i}} -- true",
                                                     setup=f"{FOIL} abort n{{i}}")
    # Same test run, with and without foil, interleaved so drift hits both.
    sh(f"git apply {patches}/test.patch && git apply {patches}/gold.patch", cwd=ws)
    plain, foiled = [], []
    for i in range(5):
        _, _, t = sh(tests, cwd=ws, check=False)
        plain.append(t)
        _, _, t = sh(f"{FOIL} run -b perf{i} {' '.join(FOIL_FLAGS)} -- sh -c '{tests}'", cwd=ws, env=env, check=False)
        foiled.append(t)
        sh(f"{FOIL} abort perf{i}", cwd=ws, env=env, check=False)
    report["tests_plain_s(median)"] = round(sorted(plain)[2], 2)
    report["tests_in_foil_s(median)"] = round(sorted(foiled)[2], 2)
    print(json.dumps(report, indent=1))
    json.dump(report, open(f"{work}/extra-{tid}.json", "w"), indent=1)


if __name__ == "__main__":
    main()
