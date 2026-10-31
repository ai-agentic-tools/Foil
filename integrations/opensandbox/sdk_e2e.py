"""End-to-end check of `foil serve` with the unmodified OpenSandbox Python SDK.

    foil serve -w WORKSPACE [run options] &
    OPEN_SANDBOX_DOMAIN=127.0.0.1:8080 OPEN_SANDBOX_API_KEY=... python sdk_e2e.py WORKSPACE

or through OpenSandbox's own server with runtime.type = "foil": point
OPEN_SANDBOX_* at that server and FOIL_SERVE_DOMAIN/FOIL_SERVE_API_KEY at
the `foil serve` behind it.

Exercises what the SDK does on its own (create, readiness, endpoints), then
commands, sessions, files, metrics, pause/resume, renew, and foil's own
diff and commit, and checks that the real workspace only changes on commit.
"""

import os
import sys
import time
from datetime import timedelta
from pathlib import Path

import httpx
from opensandbox import SandboxManagerSync, SandboxSync
from opensandbox.config import ConnectionConfigSync
from opensandbox.models.execd import RunCommandOpts
from opensandbox.models.sandboxes import SandboxFilter
from opensandbox.models.filesystem import (
    ContentReplaceEntry,
    DirectoryListEntry,
    MoveEntry,
    SearchEntry,
    SetPermissionEntry,
    WriteEntry,
)

workspace = Path(sys.argv[1]).resolve()
domain = os.environ.get("OPEN_SANDBOX_DOMAIN", "127.0.0.1:8080")
key = os.environ["OPEN_SANDBOX_API_KEY"]
config = ConnectionConfigSync(domain=domain, api_key=key, request_timeout=timedelta(seconds=60))
# foil's own calls (diff, commit) go to `foil serve`; through OpenSandbox's
# server (runtime.type = "foil") that is another address and key.
foil_domain = os.environ.get("FOIL_SERVE_DOMAIN", domain)
foil_key = os.environ.get("FOIL_SERVE_API_KEY", key)
api = httpx.Client(base_url=f"http://{foil_domain}/v1", headers={"OPEN-SANDBOX-API-KEY": foil_key}, timeout=60)


def check(cond, what):
    if not cond:
        raise SystemExit(f"FAIL: {what}")
    print(f"ok   {what}")


def stdout(execution):
    return "".join(m.text for m in execution.logs.stdout)


(workspace / "hello.txt").write_text("hello from the real workspace\n")
start = time.monotonic()
sandbox = SandboxSync.create("foil", connection_config=config, metadata={"suite": "e2e"}, timeout=timedelta(minutes=5))
check(time.monotonic() - start < 10, f"sandbox {sandbox.id} created in {time.monotonic() - start:.2f}s")
try:
    info = sandbox.get_info()
    check(info.status.state == "Running", "it is Running")

    r = sandbox.commands.run("cat hello.txt && echo changed > hello.txt && echo new > added.txt && echo oops >&2")
    check(stdout(r) == "hello from the real workspace\n", "a command sees the workspace")
    check("".join(m.text for m in r.logs.stderr) == "oops\n", "stderr is streamed apart")
    check(r.exit_code == 0 and r.error is None, "exit code 0")
    check((workspace / "hello.txt").read_text() == "hello from the real workspace\n", "the real workspace is untouched")

    r = sandbox.commands.run("exit 7")
    check(r.exit_code == 7, f"a non-zero exit code is reported ({r.exit_code})")

    r = sandbox.commands.run("echo bg-start; sleep 0.3; echo bg-end", opts=RunCommandOpts(background=True))
    time.sleep(1)
    status = sandbox.commands.get_command_status(r.id)
    check(status.running is False and status.exit_code == 0, "a background command finishes")
    logs = sandbox.commands.get_background_command_logs(r.id)
    check(logs.content == "bg-start\nbg-end\n", "its logs are kept")

    r = sandbox.commands.run("sleep 10", opts=RunCommandOpts(timeout=timedelta(milliseconds=300)))
    check(r.exit_code != 0, "a timeout stops a command")

    sid = sandbox.commands.create_session()
    sandbox.commands.run_in_session(sid, "cd sub 2>/dev/null || mkdir sub && cd sub; export X=42")
    r = sandbox.commands.run_in_session(sid, "echo $X $(basename $PWD)")
    check(stdout(r) == "42 sub\n", "a bash session keeps its state")
    sandbox.commands.delete_session(sid)

    sandbox.files.write_file(str(workspace / "notes" / "a.md"), "# notes\nfoo bar foo\n")
    check(sandbox.files.read_file(str(workspace / "notes" / "a.md")) == "# notes\nfoo bar foo\n", "write then read a file")
    sandbox.files.write_files([WriteEntry(path=str(workspace / "bin.dat"), data=bytes(range(256)))])
    check(sandbox.files.read_bytes(str(workspace / "bin.dat")) == bytes(range(256)), "binary files round-trip")
    infos = sandbox.files.get_file_info([str(workspace / "notes" / "a.md")])
    check(list(infos.values())[0].size == 20, "file info")
    sandbox.files.replace_contents([ContentReplaceEntry(path=str(workspace / "notes" / "a.md"), old_content="foo", new_content="baz")])
    check(sandbox.files.read_file(str(workspace / "notes" / "a.md")) == "# notes\nbaz bar baz\n", "replace contents")
    found = sandbox.files.search(SearchEntry(path=str(workspace), pattern="*.md"))
    check([Path(f.path).name for f in found] == ["a.md"], "search by pattern")
    sandbox.files.move_files([MoveEntry(src=str(workspace / "notes" / "a.md"), dest=str(workspace / "notes" / "b.md"))])
    listing = sandbox.files.list_directory(DirectoryListEntry(path=str(workspace / "notes")))
    check([Path(e.path).name for e in listing] == ["b.md"], "move and list")
    sandbox.files.set_permissions([SetPermissionEntry(path=str(workspace / "notes" / "b.md"), mode=600)])
    check(sandbox.files.get_file_info([str(workspace / "notes" / "b.md")])[str(workspace / "notes" / "b.md")].mode == 600, "set permissions")
    sandbox.files.create_directories([WriteEntry(path=str(workspace / "d1" / "d2"), mode=755)])
    sandbox.files.delete_directories([str(workspace / "d1")])
    sandbox.files.delete_files([str(workspace / "bin.dat")])
    check(not (workspace / "notes").exists(), "files written in the sandbox stay out of the workspace")

    m = sandbox.get_metrics()
    check(m.cpu_count >= 1 and m.memory_total_in_mib > 0, "metrics")

    sandbox.pause()
    check(sandbox.get_info().status.state == "Paused", "pause")
    SandboxSync.resume(sandbox.id, connection_config=config)
    check(sandbox.get_info().status.state == "Running", "resume")
    check(stdout(sandbox.commands.run("echo alive")) == "alive\n", "commands run after resume")

    sandbox.renew(timedelta(minutes=30))
    manager = SandboxManagerSync.create(connection_config=config)
    listed = manager.list_sandbox_infos(SandboxFilter(states=["Running"], metadata={"suite": "e2e"}))
    check([s.id for s in listed.sandbox_infos] == [sandbox.id], "the manager lists it, filtered by state and metadata")

    diff = api.get(f"/sandboxes/{sandbox.id}/diff").json()["changes"]
    kinds = {c["path"]: c["kind"] for c in diff}
    check(kinds.get("hello.txt") == "M" and kinds.get("added.txt") == "A", f"diff shows the changes {kinds}")
    applied = api.post(f"/sandboxes/{sandbox.id}/commit", json={}).json()
    check(applied.get("applied", 0) >= 2, f"commit applied {applied}")
    check((workspace / "hello.txt").read_text() == "changed\n" and (workspace / "added.txt").exists(), "the workspace has the changes now")
    check(api.get(f"/sandboxes/{sandbox.id}").status_code == 404, "a committed sandbox is gone")
    sandbox = None

    # Delete discards: nothing reaches the workspace.
    other = SandboxSync.create("foil", connection_config=config)
    other.commands.run("echo discarded > discarded.txt")
    other.kill()
    check(not (workspace / "discarded.txt").exists(), "kill discards the sandbox's changes")
    print("all checks passed")
finally:
    if sandbox is not None:
        try:
            sandbox.kill()
        except Exception as e:  # noqa: BLE001
            print(f"cleanup: {e}")
