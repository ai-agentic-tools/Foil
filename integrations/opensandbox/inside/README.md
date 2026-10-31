# foil inside an OpenSandbox sandbox

One sandbox, several attempts: foil branches the project inside the
sandbox, so an agent (or `foil fanout`) can try N changes at once, check
each in its own copy-on-write branch, and commit only the one that works.
No extra sandboxes, no copies of the project.

```sh
docker build -t foil-opensandbox integrations/opensandbox/inside
OPEN_SANDBOX_DOMAIN=localhost:8080 OPEN_SANDBOX_API_KEY=... python integrations/opensandbox/inside/demo.py
```

The image is Ubuntu 24.04 with git, Python and the foil release binary
(SHA-256 checked); `FOIL_VERSION=v0.4.0` as a build argument pins a release.

**Creating the sandbox.** foil needs user namespaces and mounts, which
Docker's default profiles block. OpenSandbox's Docker runtime grants exactly
that per sandbox when you ask for isolated execution:

```python
SandboxSync.create("foil-opensandbox", extensions={"bootstrap.execd.isolation": "enable"}, ...)
```

That sets AppArmor and seccomp to unconfined, unmasks `/proc` (needed for
`--strict`) and mounts a tmpfs at `/var/lib/execd/isolation`, where the image
keeps branches (`FOIL_HOME`): the container's own filesystem is overlayfs,
which a branch can't stack on. It also adds `CAP_SYS_ADMIN` to the
container. As with OpenSandbox's own isolated execution, that widens what
code in the sandbox can ask of the host kernel; for hostile code use a
microVM runtime (Kata, Firecracker) and run foil inside it.

**What the demo shows** (OpenSandbox 1.2.0, Docker runtime, Linux 7.0, foil 0.3.0):

```
$ foil doctor
ok  branches      copy-on-write branches work (stored in /var/lib/execd/isolation/foil)
ok  --protect     read-only view of everything outside the workspace works
ok  --strict      deny-by-default view, PID/IPC isolation, no privilege gain work
ok  --no-network  network isolation works
NO  limits        systemd-run not found: ... use the container's own limits
$ foil fanout -n 3 --name fix --pick --check 'PYTHONPATH=. python3 -B /workspace/check_calc.py' -- sh attempt.sh
foil: 3 attempts of fix in 0.1 s
  attempt  status   check  changes  time
  fix-1   0        fail   1        0.0 s
  fix-2   0        pass   1        0.0 s
  fix-3   0        fail   1        0.0 s
foil: picked fix-2: applied 1 change; discarded 2 other attempts
```

Attempt 3 "fixed" the test by rewriting it; the check is a copy kept
outside the project, so it fails. Limits inside the sandbox come from the
sandbox's own `resourceLimits`.
