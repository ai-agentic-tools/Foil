"""OpenSandbox's published create benchmark, run against any OpenSandbox server.

Adapted from OpenSandbox's scripts/fast-sandbox-env/create-bench.py
(Apache-2.0, https://github.com/opensandbox-group/OpenSandbox), the method
behind docs/architecture/fast-sandbox/performance.md: WARMUP untimed
creates, then N timed creates in batches of CONCURRENCY with BATCH_PAUSE
seconds between batches; a sample runs from the SDK call until the SDK's own
readiness check (execd /ping) passes, polled every PING_INTERVAL_MS; each
sandbox is killed after readiness, outside the timing; nearest-rank
percentiles over successful attempts, failures counted separately.

The one change: Sandbox.create(IMAGE) instead of create_from_template(),
since templates are a fast-sandbox (Firecracker) feature. With `foil serve`
the image is recorded, not run; with OpenSandbox's docker runtime it is
pulled once (warmup) and started per sandbox.

    DOMAIN=127.0.0.1:18200 API_KEY=... IMAGE=ubuntu:24.04 N=100 CONCURRENCY=10 \
      REPORT_PATH=docker-c10.json python create_bench.py
"""

import asyncio
import json
import math
import os
import statistics
import sys
import time
from datetime import timedelta
from pathlib import Path

from opensandbox.config import ConnectionConfig
from opensandbox.manager import SandboxManager
from opensandbox.sandbox import Sandbox

DOMAIN = os.environ.get("DOMAIN", "127.0.0.1:8080")
API_KEY = os.environ["API_KEY"]
IMAGE = os.environ.get("IMAGE", "ubuntu:24.04")
LABEL = os.environ.get("LABEL", DOMAIN)
N = int(os.environ.get("N", "100"))
CONCURRENCY = int(os.environ.get("CONCURRENCY", "1"))
BATCH_PAUSE = float(os.environ.get("BATCH_PAUSE", "5" if CONCURRENCY > 1 else "0"))
WARMUP = int(os.environ.get("WARMUP", "2"))
READY_TIMEOUT = int(os.environ.get("READY_TIMEOUT", "180"))
PING_INTERVAL_MS = int(os.environ.get("PING_INTERVAL_MS", "10"))
REQUEST_TIMEOUT = float(os.environ.get("REQUEST_TIMEOUT", "10"))
REPORT_PATH = os.environ.get("REPORT_PATH", "")

CONFIG = ConnectionConfig(
    domain=DOMAIN,
    api_key=API_KEY,
    request_timeout=timedelta(seconds=REQUEST_TIMEOUT),
    disable_metrics=True,
)


async def one_create():
    start = time.monotonic()
    sandbox = await Sandbox.create(
        IMAGE,
        timeout=timedelta(hours=1),
        ready_timeout=timedelta(seconds=READY_TIMEOUT),
        connection_config=CONFIG,
        health_check_polling_interval=timedelta(milliseconds=PING_INTERVAL_MS),
    )
    latency = time.monotonic() - start
    await sandbox.close()
    return latency, sandbox.id


async def main():
    manager = await SandboxManager.create(CONFIG)
    for i in range(WARMUP):
        latency, sandbox_id = await one_create()
        print(f"==> warmup {i + 1}/{WARMUP}: {latency:.3f}s")
        await manager.kill_sandbox(sandbox_id)

    samples, attempts = [], []

    async def run(i):
        try:
            latency, sandbox_id = await one_create()
        except Exception as err:
            attempts.append({"index": i, "success": False, "error_type": type(err).__name__, "error": str(err)[:200]})
            print(f"[{i}/{N}] FAILED: {type(err).__name__}: {str(err)[:120]}", flush=True)
            return
        samples.append(latency)
        attempts.append({"index": i, "success": True, "ready_seconds": latency})
        await manager.kill_sandbox(sandbox_id)

    started = time.monotonic()
    for batch_start in range(1, N + 1, CONCURRENCY):
        batch = range(batch_start, min(batch_start + CONCURRENCY, N + 1))
        await asyncio.gather(*(run(i) for i in batch))
        if batch.stop < N + 1 and BATCH_PAUSE:
            await asyncio.sleep(BATCH_PAUSE)
    wall = time.monotonic() - started
    await manager.close()

    def pct(p):
        ordered = sorted(samples)
        return ordered[max(0, math.ceil(p / 100 * len(ordered)) - 1)]

    summary = {
        "mean": statistics.mean(samples), "p50": pct(50), "p90": pct(90), "p99": pct(99),
        "min": min(samples), "max": max(samples),
    } if samples else {}
    failures = N - len(samples)
    report = {
        "label": LABEL,
        "measurement": "SDK create through execd /ping readiness",
        "settings": {
            "domain": DOMAIN, "image": IMAGE, "n": N, "concurrency": CONCURRENCY, "warmup": WARMUP,
            "batch_pause_seconds": BATCH_PAUSE, "ping_interval_ms": PING_INTERVAL_MS,
            "request_timeout_seconds": REQUEST_TIMEOUT,
        },
        "successes": len(samples),
        "failures": failures,
        "wall_seconds_including_pauses": wall,
        "percentile_method": "nearest_rank",
        "successful_latency_seconds": summary,
        "attempts": sorted(attempts, key=lambda a: a["index"]),
    }
    if REPORT_PATH:
        Path(REPORT_PATH).write_text(json.dumps(report, indent=2) + "\n")
    if summary:
        print(f"{LABEL} c={CONCURRENCY}: {len(samples)}/{N} ok  p50 {summary['p50'] * 1000:.1f} ms  "
              f"p90 {summary['p90'] * 1000:.1f} ms  p99 {summary['p99'] * 1000:.1f} ms  mean {summary['mean'] * 1000:.1f} ms")
    else:
        print(f"{LABEL} c={CONCURRENCY}: 0/{N} ok")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    asyncio.run(main())
