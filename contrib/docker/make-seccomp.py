"""Build foil-seccomp.json: Docker's default seccomp profile, plus the
system calls foil needs to create namespaces and mounts.

    curl -sSL https://raw.githubusercontent.com/moby/profiles/main/seccomp/default.json -o default.json
    python3 make-seccomp.py default.json > foil-seccomp.json
"""
import json
import sys

FOIL = [
    "unshare", "clone", "setns", "mount", "umount2", "pivot_root",
    "open_tree", "move_mount", "mount_setattr", "fsopen", "fsconfig", "fsmount", "fspick",
    "close_range",
]

profile = json.load(open(sys.argv[1]))
profile["syscalls"].append({
    "names": FOIL,
    "action": "SCMP_ACT_ALLOW",
    "comment": "foil: user, mount, PID, IPC and network namespaces; overlay and view mounts",
})
json.dump(profile, sys.stdout, indent=2)
print()
