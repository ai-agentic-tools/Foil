"""Time sandbox creation, a first command, and deletion against `foil serve`.

    OPEN_SANDBOX_DOMAIN=127.0.0.1:8080 OPEN_SANDBOX_API_KEY=... python bench.py [N]
"""

import os
import statistics
import sys
import time

import httpx

n = int(sys.argv[1]) if len(sys.argv) > 1 else 10
domain = os.environ.get("OPEN_SANDBOX_DOMAIN", "127.0.0.1:8080")
api = httpx.Client(base_url=f"http://{domain}/v1", headers={"OPEN-SANDBOX-API-KEY": os.environ["OPEN_SANDBOX_API_KEY"]}, timeout=60)
create, first, delete = [], [], []
for _ in range(n):
    t = time.perf_counter()
    sandbox = api.post("/sandboxes", json={"image": {"uri": "foil"}}).json()
    create.append(time.perf_counter() - t)
    endpoint = api.get(f"/sandboxes/{sandbox['id']}/endpoints/44772").json()
    t = time.perf_counter()
    httpx.post(f"http://{endpoint['endpoint']}/command", headers=endpoint["headers"], json={"command": "true"}, timeout=60).raise_for_status()
    first.append(time.perf_counter() - t)
    t = time.perf_counter()
    api.delete(f"/sandboxes/{sandbox['id']}")
    delete.append(time.perf_counter() - t)
for name, xs in (("create (until execd answers)", create), ("first command", first), ("delete", delete)):
    print(f"{name:30} median {statistics.median(xs) * 1000:6.1f} ms   max {max(xs) * 1000:6.1f} ms")
