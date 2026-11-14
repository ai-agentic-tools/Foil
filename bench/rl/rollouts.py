"""Part A: what an RL rollout costs, by environment, with no model involved.

A rollout = reset to a fresh copy of a prepared task, apply a candidate
patch, run the task's tests, score the reward, tear down. The candidate is
the task's merged fix (correct reward: 1) or nothing (correct reward: 0), so
every reward can be checked. Three environments, same tests, same Python
environment (mounted read-only at the same path):

  foil    `foil run -b NAME --strict` over the prepared tree, no network;
          `foil abort NAME` afterwards (a copy-on-write branch).
  docker  `docker run --rm --network none` from an image with the prepared
          tree baked in (the usual RL sandbox: one image per task, a fresh
          container per rollout).
  copy    `cp -a` of the prepared tree, run without isolation, `rm -rf`.

Measured: overhead of an empty rollout (command `true`), wall time of real
rollouts at 1 and 4 at once, rollouts per hour, whether every reward is
right, and what is left on the machine after normal runs and after the
rollout driver is SIGKILLed mid-run.

Usage: rollouts.py prepare WORK      (once: task trees, images)
       rollouts.py run WORK [--arms foil,docker,copy] [--reps N]
Needs the stage-1 work folder of bench/swebench (repos, envs, verified.jsonl,
results.jsonl) in WORK/../verified, docker, and FOIL=path/to/foil.
"""

import argparse
import concurrent.futures
import json
import os
import re
import shutil
import signal
import statistics
import subprocess
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "swebench"))
from run import ensure_env, outcome, sh, test_command  # noqa: E402

FOIL = os.environ.get("FOIL", "foil")
UV_PY = os.path.expanduser("~/.local/share/uv/python")
PER_REPO = 5  # tasks per repository: the ones with the fastest tests


def load(work):
    src = os.path.join(os.path.dirname(work), "verified")
    tasks = {t["instance_id"]: t for t in map(json.loads, open(f"{src}/verified.jsonl"))}
    stage1 = [r for r in map(json.loads, open(f"{src}/results.jsonl")) if r.get("baseline", {}).get("resolved")]
    return src, tasks, stage1


def pick(tasks, stage1):
    chosen = []
    for repo in ("django/django", "sympy/sympy"):
        rows = sorted((r for r in stage1 if r["repo"] == repo), key=lambda r: r["baseline_s"])
        chosen += [r["id"] for r in rows[:PER_REPO]]
    return chosen


def image(tid):
    return "rl-" + tid.lower().replace("__", "-")


def prepare(work):
    src, tasks, stage1 = load(work)
    ids = pick(tasks, stage1)
    os.makedirs(f"{work}/base", exist_ok=True)
    os.makedirs(f"{work}/patches", exist_ok=True)
    rc, out, _ = sh("docker image inspect rl-base", check=False)
    if rc != 0:
        os.makedirs(f"{work}/rl-base", exist_ok=True)
        open(f"{work}/rl-base/Dockerfile", "w").write(
            "FROM ubuntu:24.04\nRUN apt-get update -qq && apt-get install -qq -y git >/dev/null && rm -rf /var/lib/apt/lists/*\n")
        sh(f"docker build -q -t rl-base {work}/rl-base", timeout=1800)
    for tid in ids:
        t = tasks[tid]
        base = f"{work}/base/{tid}"
        if not os.path.isdir(base):
            os.makedirs(base)
            repo = f"{src}/repos/{t['repo'].split('/')[1]}"
            sh(f"git -C {repo} archive {t['base_commit']} | tar -x -C {base}")
        p = f"{work}/patches/{tid}"
        os.makedirs(p, exist_ok=True)
        open(f"{p}/test.patch", "w").write(t["test_patch"])
        open(f"{p}/gold.patch", "w").write(t["patch"])
        if sh(f"docker image inspect {image(tid)}", check=False)[0] != 0:
            open(f"{work}/base/{tid}.Dockerfile", "w").write("FROM rl-base\nCOPY . /work\nWORKDIR /work\n")
            sh(f"docker build -q -t {image(tid)} -f {work}/base/{tid}.Dockerfile {base}", timeout=1800)
        ensure_env(src, t)
    json.dump(ids, open(f"{work}/tasks.json", "w"))
    print(f"prepared {len(ids)} tasks: {', '.join(ids)}")


def script(task, env_dir, patches, gold):
    steps = [f"git apply {patches}/test.patch"]
    if gold:
        steps.append(f"git apply {patches}/gold.patch")
    return " && ".join(steps) + f" && {{ {test_command(task, env_dir)}; }} 2>&1"


def command(arm, work, src, task, n, body):
    """argv, cwd and the teardown argv for one rollout of `body` in `arm`."""
    tid = task["instance_id"]
    base = f"{work}/base/{tid}"
    env_dir = ensure_env(src, task)
    name = f"r{n}-{arm}"
    if arm == "foil":
        argv = [FOIL, "run", "-b", name, "-w", base, "--strict", "--allow-read", env_dir, "--allow-read", UV_PY,
                "--allow-read", f"{work}/patches/{tid}", "--", "bash", "-c", body]
        return argv, base, [FOIL, "abort", name]
    if arm == "docker":
        argv = ["docker", "run", "--rm", "--network", "none", "-v", f"{env_dir}:{env_dir}:ro", "-v", f"{UV_PY}:{UV_PY}:ro",
                "-v", f"{work}/patches/{tid}:{work}/patches/{tid}:ro", image(tid), "bash", "-c", body]
        return argv, work, None
    dest = f"{work}/copies/{name}"
    argv = ["bash", "-c", f"rm -rf {dest} && mkdir -p {work}/copies && cp -a {base} {dest} && cd {dest} && {body}"]
    return argv, work, ["rm", "-rf", dest]


def rollout(arm, work, src, task, n, gold, body=None):
    tid = task["instance_id"]
    body = body or script(task, ensure_env(src, task), f"{work}/patches/{tid}", gold)
    argv, cwd, teardown = command(arm, work, src, task, n, body)
    env = dict(os.environ, FOIL_HOME=f"{work}/foil-home")
    start = time.monotonic()
    p = subprocess.run(argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                       stderr=subprocess.STDOUT, text=True, errors="replace", timeout=900)
    if teardown:
        subprocess.run(teardown, cwd=cwd, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    took = time.monotonic() - start
    reward = outcome(task, p.stdout)["resolved"] if body is not None and "git apply" in body else None
    return {"arm": arm, "id": tid, "gold": gold, "s": round(took, 3), "rc": p.returncode, "reward": reward,
            "right": reward is None or reward == gold, "tail": p.stdout[-400:] if p.returncode not in (0, 1) else ""}


def leftovers(work):
    """What is still on the machine that a rollout made."""
    c = lambda cmd: int(sh(cmd, check=False)[1].strip() or 0)
    # The [x] in each pattern keeps grep/pgrep from matching their own command line.
    return {
        "containers": c("docker ps -aq | xargs -r docker inspect -f '{{.Config.Image}}' | grep -c '^r[l]-' ; true"),
        "foil_branches": c(f"FOIL_HOME={work}/foil-home {FOIL} list --json 2>/dev/null | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))' || echo 0"),
        "overlay_mounts": c("grep -c ' - overlay ' /proc/self/mountinfo; true"),
        "copies": c(f"ls {work}/copies 2>/dev/null | wc -l"),
        "processes": c("pgrep -fc 'runtest[s].py|bin/tes[t] |slee[p] 35'; true"),
    }


def batch(arm, work, src, tasks, ids, reps, workers):
    jobs = [(tasks[t], i * len(ids) * 2 + k * 2 + g, bool(g)) for i in range(reps) for k, t in enumerate(ids) for g in (1, 0)]
    start = time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(workers) as pool:
        rows = list(pool.map(lambda j: rollout(arm, work, src, j[0], j[1] + 100000 * workers, j[2]), jobs))
    wall = time.monotonic() - start
    return rows, wall


def kill_test(arm, work, src, tasks, ids):
    """Start 4 rollouts that sleep inside, SIGKILL each one's top process (foil, the docker
    client, bash) after 3 s, as when the RL worker crashes; look 2 s and 45 s later."""
    before = leftovers(work)
    procs = []
    for k, tid in enumerate(ids[:4]):
        argv, cwd, _ = command(arm, work, src, tasks[tid], 900000 + k, "sleep 35")
        env = dict(os.environ, FOIL_HOME=f"{work}/foil-home")
        procs.append(subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                      stderr=subprocess.DEVNULL, start_new_session=True))
    time.sleep(3)
    during = leftovers(work)
    for p in procs:
        os.kill(p.pid, signal.SIGKILL)
        p.wait()
    time.sleep(2)
    soon = leftovers(work)
    time.sleep(43)
    later = leftovers(work)
    return {"before": before, "running": during, "after_2s": soon, "after_45s": later}


def cleanup(work):
    sh("docker ps -aq | xargs -r docker inspect -f '{{.Id}} {{.Config.Image}}' | awk '$2 ~ /^rl-/ {print $1}' | xargs -r docker rm -f",
       check=False)
    sh(f"FOIL_HOME={work}/foil-home {FOIL} list --json 2>/dev/null | python3 -c 'import json,sys; [print(b[\"name\"]) for b in json.load(sys.stdin)]' "
       f"| xargs -r -n1 env FOIL_HOME={work}/foil-home {FOIL} abort --force", check=False)
    shutil.rmtree(f"{work}/copies", ignore_errors=True)
    sh("pkill -f 'sleep 35'", check=False)


def run(work, arms, reps):
    src, tasks, _ = load(work)
    ids = json.load(open(f"{work}/tasks.json"))
    out = open(f"{work}/rollouts.jsonl", "a")
    for arm in arms:
        cleanup(work)
        # Warm-up: one rollout per task, not counted (page cache, image layers).
        for k, tid in enumerate(ids):
            rollout(arm, work, src, tasks[tid], 800000 + k, True)
        empty = [rollout(arm, work, src, tasks[ids[i % len(ids)]], 700000 + i, False, body="true")["s"] for i in range(30)]
        rec = {"arm": arm, "empty_rollout_s": {"median": statistics.median(empty), "p90": sorted(empty)[26],
                                                 "min": min(empty), "max": max(empty)}}
        for workers in (1, 4):
            rows, wall = batch(arm, work, src, tasks, ids, reps, workers)
            for r in rows:
                out.write(json.dumps(dict(r, workers=workers)) + "\n")
            rec[f"w{workers}"] = {"rollouts": len(rows), "wall_s": round(wall, 1),
                                  "per_hour": round(len(rows) / wall * 3600),
                                  "median_s": statistics.median(r["s"] for r in rows),
                                  "rewards_right": sum(r["right"] for r in rows),
                                  "errors": sum(r["rc"] not in (0, 1) for r in rows)}
            rec[f"leftovers_after_w{workers}"] = leftovers(work)
        rec["kill_9"] = kill_test(arm, work, src, tasks, ids)
        cleanup(work)
        print(json.dumps(rec), flush=True)
        with open(f"{work}/summary.jsonl", "a") as f:
            f.write(json.dumps(rec) + "\n")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("cmd", choices=["prepare", "run"])
    ap.add_argument("work")
    ap.add_argument("--arms", default="foil,docker,copy")
    ap.add_argument("--reps", type=int, default=2)
    a = ap.parse_args()
    work = os.path.abspath(a.work)
    prepare(work) if a.cmd == "prepare" else run(work, a.arms.split(","), a.reps)


if __name__ == "__main__":
    main()
