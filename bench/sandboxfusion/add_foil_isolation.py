"""Add `isolation: foil` to a SandboxFusion checkout (bytedance/SandboxFusion).

    python add_foil_isolation.py path/to/SandboxFusion

SandboxFusion's `lite` isolation builds a sandbox from parts, as root: an
overlay of / on a tmpfs, cgroups (cgcreate/cgset), a network namespace from a
script, unshare --pid and chroot. `foil` mode runs each command under
`foil run --strict` as an ordinary user: a private view (system folders
read-only, the code's folder writable, a private /tmp and home), its own PID
and IPC namespaces, no network, a clean environment, and lite's limits (4G
of memory, 1 CPU). FOIL_ALLOW_READ (colon-separated) shows extra folders,
such as the Python the code runs with.
"""

import sys
from pathlib import Path

ADD = '''    elif config.sandbox.isolation == 'foil':
        import shlex
        foil = os.environ.get('FOIL', 'foil')
        reads = [p for p in os.environ.get('FOIL_ALLOW_READ', '').split(':') if p]
        path = (extra_env or {}).get('PATH', os.environ.get('PATH', ''))

        def wrap(command: str) -> str:
            flags = ['--strict', '--memory', '4G', '--cpus', '1', '--allow-write', cwd, '--setenv', f'PATH={path}']
            for p in reads:
                flags += ['--allow-read', p]
            return (f'cd {shlex.quote(cwd)} && exec {shlex.quote(foil)} run ' + ' '.join(shlex.quote(f) for f in flags) +
                    ' -- bash -c ' + shlex.quote(command))

        if compile_command is not None:
            compile_res = await run_command_bare(wrap(compile_command), args.compile_timeout, None, cwd, extra_env)
        if compile_res is None or (compile_res.status == CommandRunStatus.Finished and compile_res.return_code == 0):
            run_res = await run_command_bare(wrap(run_command), args.run_timeout, args.stdin, cwd, extra_env)
        for filename in args.fetch_files:
            fp = os.path.abspath(os.path.join(cwd, filename))
            if os.path.isfile(fp):
                with open(fp, 'rb') as f:
                    files[filename] = base64.b64encode(f.read()).decode('utf-8')
        return CodeRunResult(compile_result=compile_res, run_result=run_res, files=files)

'''

root = Path(sys.argv[1])
config = root / "sandbox/configs/run_config.py"
text = config.read_text()
if "Literal['none', 'lite', 'foil']" not in text:
    if "isolation: Literal['none', 'lite']" not in text:
        sys.exit("isolation setting not found; SandboxFusion changed")
    config.write_text(text.replace("isolation: Literal['none', 'lite']", "isolation: Literal['none', 'lite', 'foil']", 1))

path = root / "sandbox/runners/base.py"
source = path.read_text()
anchor = "    elif config.sandbox.isolation == 'lite':\n"
if "isolation == 'foil'" in source:
    sys.exit("already there")
if anchor not in source:
    sys.exit("lite branch not found; SandboxFusion changed")
path.write_text(source.replace(anchor, ADD + anchor, 1))
print(f"added isolation: foil to {path}")
