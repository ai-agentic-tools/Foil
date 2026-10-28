# The foil runtime for OpenSandbox's server

`opensandbox-foil-runtime.patch` applies to OpenSandbox at the commit in
`BASE` and adds:

- `runtime.type = "foil"` and a `[foil]` config block (`url`, `api_key` or
  `api_key_env`, `workspace`, `endpoint_host`, `timeout_seconds`);
- `opensandbox_server/services/foil/`: `FoilSandboxService`, which forwards
  the lifecycle calls to `foil serve` and returns its relay as the execd
  endpoint (with the sandbox's `X-EXECD-ACCESS-TOKEN`), plus `get_diff` and
  `commit`;
- the no-op snapshot runtime for `foil`;
- `server/tests/test_foil_service.py`: against a mocked `foil serve`, and
  a real one when `FOIL_BIN` names a foil binary;
- docs in `server/configuration.md`, and **OSEP-0025** (`oseps/`), the
  proposal OpenSandbox asks for before a change like this. It is a draft:
  not yet sent to OpenSandbox's maintainers.

```sh
git clone https://github.com/alibaba/OpenSandbox && cd OpenSandbox
git checkout "$(cat /path/to/foil/integrations/opensandbox/runtime/BASE)"
git apply /path/to/foil/integrations/opensandbox/runtime/opensandbox-foil-runtime.patch
cd server && uv sync && FOIL_BIN=$(command -v foil) uv run pytest tests/test_foil_service.py

FOIL_SERVE_API_KEY=k OPEN_SANDBOX_API_KEY=k foil serve --listen 127.0.0.1:8090 -w ~/src/app --strict &
FOIL_SERVE_API_KEY=k uv run opensandbox-server --config foil.toml
```

with `foil.toml`:

```toml
[server]
host = "127.0.0.1"
port = 8080
api_key = "your-server-key"

[runtime]
type = "foil"
execd_image = "unused-by-foil"

[foil]
url = "http://127.0.0.1:8090"
```

To refresh the patch after changing a clone: `git add -A && git diff
--cached > opensandbox-foil-runtime.patch`, and put the base commit in
`BASE`. CI applies it and runs its tests on every push.
