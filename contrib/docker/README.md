# foil inside containers

Container runtimes switch off, by default, the kernel features foil uses.
These recipes switch on exactly what foil needs. Tested on 2026-10-14 on
Ubuntu 26.04 (Linux 7.0) with Docker 29.1, Podman 5.7 and Sysbox 0.7.1.
Run `foil doctor` inside the container: it names each blocker it finds and
the setting that removes it.

## Results

| Runtime and settings | branches | --protect | --strict | --no-network | cleanup box |
| --- | --- | --- | --- | --- | --- |
| Docker, defaults | no | no | no | no | no |
| Docker, recipe below | yes | yes | yes | yes | no (strict runs still clean up) |
| Podman, defaults | no | no | no | no | – |
| Podman, recipe below | yes | yes | yes | yes | yes |
| Sysbox (`--runtime=sysbox-runc`), no flags, FOIL_HOME on tmpfs | yes | no | no | yes | no |

The full test suite passes inside Docker with the recipe:
`contrib/docker/test-in-container.sh` (CI runs it).

## Docker

```sh
docker run \
  --security-opt seccomp=contrib/docker/foil-seccomp.json \
  --security-opt apparmor=unconfined \
  --security-opt systempaths=unconfined \
  --tmpfs /foil:exec,mode=1777 -e FOIL_HOME=/foil \
  IMAGE ...
```

What each setting is for:

- `seccomp=foil-seccomp.json`: Docker's default seccomp profile plus the
  calls foil makes (new namespaces, mount, pivot_root, open_tree,
  move_mount, mount_setattr). Narrower than `seccomp=unconfined`.
  Rebuild it from Docker's current profile with `make-seccomp.py`.
- `apparmor=unconfined`: Docker's `docker-default` profile refuses mounts.
- `systempaths=unconfined`: Docker masks parts of /proc, and the kernel
  then refuses the fresh /proc that `--strict` mounts (also with
  `subset=pid`). Needed for `--strict` only.
- `FOIL_HOME` on a tmpfs or a volume: the container's own filesystem is
  overlayfs, which a branch's overlay can't stack on.

The trade-off: code in this container may create user namespaces, which
gives it more of the kernel to attack. That is why Docker blocks it by
default. For code you don't trust, prefer a microVM per tenant (Kata,
Firecracker) and run foil inside it.

## Podman

```sh
podman run \
  --security-opt seccomp=contrib/docker/foil-seccomp.json \
  --security-opt apparmor=unconfined \
  --security-opt unmask=ALL \
  --tmpfs /foil:exec,mode=1777 -e FOIL_HOME=/foil \
  IMAGE ...
```

`unmask=ALL` is Podman's equivalent of Docker's `systempaths=unconfined`.
Podman also gives the container a writable cgroup, so foil's cleanup box
works too.

## Ubuntu hosts (23.10 and later)

The host's user-namespace restriction applies inside containers too: the
host must trust the foil binary at the path it has *inside* the container.
Install foil on the host at the same path (e.g. `/usr/local/bin/foil`) and
run `sudo foil setup` there once. foil re-executes itself once at start-up
if the runtime started it without that profile (a container's first
process is started "unconfined"), so `docker run IMAGE foil ...` works.

## Sysbox

Sysbox containers can create namespaces with no extra flags, so branches
(with FOIL_HOME on a tmpfs) and `--no-network` work. `--protect` and
`--strict` don't: Sysbox refuses `open_tree`, and it masks /proc. On
Ubuntu 24.04 and later, Sysbox 0.7.1 also needs the host's `fusermount3`
AppArmor profile in complain mode to start containers at all.

## Dev containers, CI, Kubernetes (not tested yet)

- Dev containers (`devcontainer.json`): put the Docker settings in
  `runArgs`, e.g. `"runArgs": ["--security-opt", "apparmor=unconfined", ...]`.
- GitHub Actions jobs with `container:`: the same settings under
  `options:`. Jobs without `container:` run on a VM, where foil needs nothing.
- Kubernetes: a `Localhost` seccomp profile built from foil-seccomp.json,
  `appArmorProfile: {type: Unconfined}`, `procMount: Unmasked` (requires
  `hostUsers: false`), and an `emptyDir` volume for FOIL_HOME.
