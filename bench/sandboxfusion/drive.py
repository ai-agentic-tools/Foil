"""Send HumanEval's 164 canonical solutions, plus a few probes, to a
SandboxFusion server's /run_code, and report speed, results and isolation.

    python drive.py http://127.0.0.1:8080 HumanEval.jsonl CONCURRENCY LABEL [REPORT.json]

Every program is the problem's prompt + canonical solution + its tests +
check(entry_point), so all 164 must pass in every mode; a failure is the
sandbox's. The probes are ordinary programs that try what code under test
shouldn't: read the server user's home, reach the network, and leave a
process running after it exits.
"""

import asyncio
import json
import statistics
import sys
import time

import httpx

url, data, concurrency, label = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]
report_path = sys.argv[5] if len(sys.argv) > 5 else ""
problems = [json.loads(line) for line in open(data)]

PROBES = {
    # Prints what it finds; "blocked" lines are the sandbox doing its job.
    "home": "import os\nh = os.path.expanduser('~')\ntry:\n    print('home:', sorted(os.listdir(h))[:8])\nexcept Exception as e:\n    print('home: blocked', type(e).__name__)\n",
    "network": "import urllib.request\ntry:\n    urllib.request.urlopen('http://example.com', timeout=3)\n    print('network: reached example.com')\nexcept Exception as e:\n    print('network: blocked', type(e).__name__)\n",
    # A child in its own session that outlives the program (`sleep 314.159`,
    # found afterwards by its arguments).
    "leftover": "import subprocess\nsubprocess.Popen(['sleep', '314.159'], start_new_session=True)\nprint('leftover: started')\n",
}


async def run(client, sem, code):
    async with sem:
        t = time.perf_counter()
        r = await client.post(f"{url}/run_code", json={"code": code, "language": "python", "run_timeout": 20})
        elapsed = time.perf_counter() - t
        body = r.json()
        return elapsed, body


async def main():
    sem = asyncio.Semaphore(concurrency)
    async with httpx.AsyncClient(timeout=120) as client:
        await run(client, sem, "print(1)")  # warm up
        programs = [p["prompt"] + p["canonical_solution"] + "\n" + p["test"] + f"\ncheck({p['entry_point']})\n" for p in problems]
        started = time.perf_counter()
        results = await asyncio.gather(*(run(client, sem, code) for code in programs))
        wall = time.perf_counter() - started
        probes = {}
        for name, code in PROBES.items():
            _, body = await run(client, sem, code)
            out = ((body.get("run_result") or {}).get("stdout") or "").strip()
            err = ((body.get("run_result") or {}).get("stderr") or "").strip()
            probes[name] = out or f"(no output; {body.get('status')}; {err[-160:]})"
    latencies = sorted(e for e, _ in results)
    passed = sum(1 for _, b in results if b.get("status") == "Success")
    failed = [(p["task_id"], b.get("status"), ((b.get("run_result") or {}).get("stderr") or b.get("message") or "")[-200:])
              for p, (_, b) in zip(problems, results) if b.get("status") != "Success"]
    pct = lambda q: latencies[max(0, int(q / 100 * len(latencies) + 0.999999) - 1)]
    report = {
        "label": label, "concurrency": concurrency, "programs": len(problems), "passed": passed,
        "wall_seconds": wall, "programs_per_second": len(problems) / wall,
        "latency_ms": {"p50": pct(50) * 1000, "p90": pct(90) * 1000, "p99": pct(99) * 1000,
                       "mean": statistics.mean(latencies) * 1000},
        "failures": failed[:10], "probes": probes,
    }
    print(f"{label} c={concurrency}: {passed}/{len(problems)} passed, {report['programs_per_second']:.1f}/s, "
          f"p50 {report['latency_ms']['p50']:.0f} ms, p99 {report['latency_ms']['p99']:.0f} ms")
    for name, out in probes.items():
        print(f"  probe {name}: {out[:150]}")
    for f in failed[:3]:
        print("  FAIL", f)
    if report_path:
        open(report_path, "w").write(json.dumps(report, indent=2) + "\n")


asyncio.run(main())
