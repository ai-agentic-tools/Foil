"""Make SandboxFusion's `lite` isolation run on a current Linux host.

    python lite_on_cgroup_v2.py path/to/SandboxFusion

Unpatched, every `lite` run failed on our Ubuntu 26.04 host (cgroup v2,
systemd-resolved), and each failure leaves its overlay mounts and cgroups
behind:
- it copies /etc/resolv.conf into the sandbox's root, where it is a symlink
  into /run that doesn't exist there: `cp: not writing through dangling
  symlink`. Fix: cp --remove-destination.
- it sets cgroup v1 limits (memory.limit_in_bytes, cpu.cfs_quota_us); cgroup
  v2 has memory.max and cpu.max. Fix: the v2 names, same values.
- the code to run is written under /tmp and found again inside the overlay
  of /; where /tmp is its own tmpfs (as on that host, and Fedora), the
  overlay doesn't contain it: `cd: /tmp/tmpXXXX: No such file or directory`.
  Fix: SANDBOX_TMP_DIR picks the folder (the benchmark uses /var/tmp/sf).
The benchmark runs `lite` with these fixes, so it can be compared at all.
"""

import sys
from pathlib import Path

path = Path(sys.argv[1]) / "sandbox/runners/isolation.py"
source = path.read_text()
fixes = [
    ("['cp', '/etc/resolv.conf',", "['cp', '--remove-destination', '/etc/resolv.conf',"),
    ("f'memory.limit_in_bytes={mem_limit}'", "f'memory.max={mem_limit}'"),
    ("        await execute_command(['sudo', 'cgset', '-r', f'cpu.cfs_quota_us={int(100000 * cpu_limit)}', cpu_group_name])\n"
     "        await execute_command(['sudo', 'cgset', '-r', f'cpu.cfs_period_us=100000', cpu_group_name])\n",
     "        await execute_command(['sudo', 'cgset', '-r', f'cpu.max={int(100000 * cpu_limit)} 100000', cpu_group_name])\n"),
]
for old, new in fixes:
    if new in source:
        continue
    if old not in source:
        sys.exit(f"not found (SandboxFusion changed?): {old.strip()[:60]}")
    source = source.replace(old, new, 1)
path.write_text(source)

execution = Path(sys.argv[1]) / "sandbox/utils/execution.py"
text = execution.read_text()
old, new = "    TMP_DIR = '/tmp'\n", "    TMP_DIR = os.environ.get('SANDBOX_TMP_DIR', '/tmp')\n"
if new not in text:
    if old not in text:
        sys.exit("TMP_DIR not found (SandboxFusion changed?)")
    execution.write_text(text.replace(old, new, 1))
print(f"lite patched for current hosts in {path.parent.parent}")
