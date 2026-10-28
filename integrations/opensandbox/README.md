# foil and OpenSandbox

[OpenSandbox](https://github.com/alibaba/OpenSandbox) (Alibaba, Apache-2.0)
is a sandbox platform for AI agents. Its SDKs (Python, Java/Kotlin,
TypeScript, C#, Go), its CLI and its MCP server create a sandbox through a
lifecycle API, then run commands and move files through **execd**, a small
daemon inside each sandbox. Sandboxes are Docker containers, Kubernetes pods
or microVMs.

foil works with it in three ways. Each one is built and tested.

| | What | Use it when | Status |
|---|---|---|---|
| **1. foil inside a sandbox** | an image with foil; agents branch the project *inside* the sandbox | you already run OpenSandbox and want best-of-N attempts in one sandbox | [`inside/`](inside/): tested in a real Docker sandbox, unchanged OpenSandbox 1.2.0 |
| **2. foil as a runtime** | `runtime.type = "foil"` in OpenSandbox's server | you want OpenSandbox's server (auth, tooling) with sandboxes that start in ms, no Docker, on Linux or a Mac | [`runtime/`](runtime/): a patch to OpenSandbox + a proposal (OSEP-0025, draft, not yet sent) |
| **3. `foil serve`** | foil speaks OpenSandbox's APIs itself | you want the SDKs against your own repo, no server, no Docker | in foil (`foil serve`); the unchanged Python SDK passes [`sdk_e2e.py`](sdk_e2e.py) |

Levels 2 and 3 are the same machinery: OpenSandbox's server with the foil
runtime forwards to `foil serve`.

## 3. `foil serve`: the SDKs, against your repository

```sh
cd ~/src/app
OPEN_SANDBOX_API_KEY=$(openssl rand -hex 16) foil serve --strict --memory 4G &
```

```python
from opensandbox import SandboxSync          # pip install opensandbox
from opensandbox.config import ConnectionConfigSync

config = ConnectionConfigSync(domain="127.0.0.1:8080", api_key=KEY)
sandbox = SandboxSync.create("foil", connection_config=config)    # ~25 ms on Linux
sandbox.commands.run("python -m pytest -x")                        # runs in a branch of ~/src/app
sandbox.files.write_file("/home/me/src/app/notes.md", "...")       # lands in the branch, not the folder
```

Then review and apply, with foil's two extra endpoints (or `foil diff` /
`foil commit` on the branch named in the sandbox's `extensions`):

```sh
curl -H "OPEN-SANDBOX-API-KEY: $KEY" 127.0.0.1:8080/v1/sandboxes/$ID/diff             # {"changes":[{"kind":"M","path":"app.py"}],...}
curl -H "OPEN-SANDBOX-API-KEY: $KEY" "127.0.0.1:8080/v1/sandboxes/$ID/diff?patch=true"
curl -H "OPEN-SANDBOX-API-KEY: $KEY" -X POST -d '{}' 127.0.0.1:8080/v1/sandboxes/$ID/commit   # {"applied":3}
```

Commit refuses (409 `CONFLICT`) when a file it would write changed in the
folder since the sandbox started, and leaves the sandbox running;
`{"force": true}` overwrites. Deleting a sandbox discards its changes.

**How it works.** Each sandbox is a foil branch of the folder with one
long-lived `foil run` in it (with the options `foil serve` was given:
`--strict`, `--allow-host`, `--memory`, ...), whose command is `foil
__execd`: foil's own implementation of execd's API, listening on a Unix
socket. Everything it starts is inside the branch and the sandbox.
`foil serve` answers the lifecycle API and relays
`/v1/sandboxes/<id>/proxy/44772/...` to that socket, checking the
credentials outside the sandbox. Sandboxes outlive the server: a restarted
`foil serve` adopts runs still going and restarts the ones that stopped.

```
SDK ──► foil serve ──(Unix socket)──► foil __execd ──► your commands
          │                            └ inside: foil run -b osb-<id> --strict ...
          └ lifecycle: create = branch + run, delete = stop + discard, commit = stop + apply
```

**What is implemented**

| API | |
|---|---|
| Lifecycle | create (`env`, `metadata`, `timeout`, `entrypoint`, `networkPolicy`, `extensions."foil.workspace"`), get, list (state, metadata, pages), delete, pause, resume, renew, patch metadata, endpoints; plus `GET .../diff`, `POST .../commit`, `GET .../logs` |
| execd | `/ping`; `/command` (foreground, streamed; background, with status, logs and interrupt; `timeout`, `envs`, `cwd`, `argv`); bash sessions; `/files` info, upload, download (ranges, lines), delete, move, permissions, search, replace; `/directories` list, create, delete; `/metrics` |
| Not (yet) | Jupyter `/code`, `/pty`, `/v1/isolated/*` (a foil sandbox is already one), `volumes`, snapshots, the credential vault, other ports of strict sandboxes |

**Choices worth knowing**

- `image` is recorded, not run: a sandbox is a view of the folder. Several
  folders: `foil serve -w A -w B`, and `extensions: {"foil.workspace": B}`.
- `resourceLimits` from requests are ignored; limits come from `foil serve
  --memory/--cpus/--pids` (the SDKs always send defaults).
- `networkPolicy` with `defaultAction: deny` becomes `--strict` plus
  `--allow-host` per allowed target; what foil can't enforce (CIDR ranges,
  deny rules under allow-by-default) is refused with 400, never ignored.
- `uid`/`gid` other than yours are refused: foil is unprivileged.
- On macOS a branch is a folder of its own: file APIs map the workspace
  path to it; commands start in it (`pwd` shows the branch path).
- `foil serve` listens on `127.0.0.1:8080` by default (the SDKs' default).
  It makes a random key if `OPEN_SANDBOX_API_KEY` is unset, and prints it.

**Speed** (`bench.py`, median of 10, from create until execd answers):

| | Linux (VM, kernel 7.0) | macOS (Intel) |
|---|---|---|
| create, folder of 10 000 files | 25 ms (35 ms `--strict`) | 0.76 s (two APFS clones) |
| create, small folder | 25 ms | 27 ms (30 ms `--strict`) |
| first command | 10 ms | 13-18 ms |
| delete | 22 ms | 27 ms |

**Tests.** `tests/serve.rs` (lifecycle, relay, auth, conflicts, expiry,
restart, strict isolation between sandboxes); [`run-e2e.sh`](run-e2e.sh)
runs [`sdk_e2e.py`](sdk_e2e.py), 29 checks with the unchanged OpenSandbox
Python SDK, plain and `--strict`, in CI on Linux and macOS.

## 2. foil as an OpenSandbox runtime

[`runtime/opensandbox-foil-runtime.patch`](runtime/) adds `runtime.type =
"foil"` to OpenSandbox's server: `FoilSandboxService` forwards every
lifecycle call to a `foil serve`, so the server keeps its API keys,
endpoints, server-side proxy and tooling while sandboxes are foil branches.

```sh
git clone https://github.com/alibaba/OpenSandbox && cd OpenSandbox
git checkout $(cat ../foil/integrations/opensandbox/runtime/BASE)
git apply ../foil/integrations/opensandbox/runtime/opensandbox-foil-runtime.patch
```

```toml
[runtime]
type = "foil"
execd_image = "unused-by-foil"

[foil]
url = "http://127.0.0.1:8090"          # foil serve --listen 127.0.0.1:8090 -w ~/src/app --strict
api_key_env = "FOIL_SERVE_API_KEY"
```

Verified: the SDK end-to-end suite through OpenSandbox's server with this
runtime (direct and `use_server_proxy`), the patch's own tests, and the
server's full test suite (2,626 tests) still passing. The patch includes a
proposal for OpenSandbox's maintainers,
[OSEP-0025](runtime/opensandbox-foil-runtime.patch) (in `oseps/`), not yet
submitted upstream.

## 1. foil inside an OpenSandbox sandbox

See [`inside/`](inside/): an image with foil, and a demo where `foil
fanout` runs three attempts at a fix at once in one Docker sandbox, checks
each in its own branch and commits the one that passes. The sandbox needs
`extensions: {"bootstrap.execd.isolation": "enable"}`, which gives it what
foil needs (namespaces, mounts, a tmpfs for branches).
