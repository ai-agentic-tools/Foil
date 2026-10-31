"""foil inside an OpenSandbox sandbox: three attempts at a fix, in one sandbox.

    docker build -t foil-opensandbox integrations/opensandbox/inside
    OPEN_SANDBOX_DOMAIN=localhost:8080 OPEN_SANDBOX_API_KEY=... python demo.py

The sandbox gets a small project with a failing test. `foil fanout` runs
three attempts at once, each in its own copy-on-write branch of the project:
one wrong, one right, one that "passes" by deleting the test. The check
runs in each branch; the right attempt is committed, the rest discarded.
Without foil this takes three sandboxes, or three copies of the project.
"""

import os
import sys
from datetime import timedelta

from opensandbox import SandboxSync
from opensandbox.config import ConnectionConfigSync

IMAGE = os.environ.get("IMAGE", "foil-opensandbox")
config = ConnectionConfigSync(request_timeout=timedelta(seconds=120))

CALC = '''def average(xs):
    return sum(xs) / len(xs) + 1
'''
TEST = '''import calc
assert calc.average([2, 4]) == 3, calc.average([2, 4])
print("ok")
'''
# Each attempt sees FOIL_ATTEMPT=1..3.
ATTEMPT = r'''case $FOIL_ATTEMPT in
1) sed -i 's/+ 1/- 1/' calc.py ;;
2) sed -i 's/ + 1//' calc.py ;;
3) printf 'print("ok")\n' > test_calc.py ;;
esac'''


def run(sandbox, command):
    execution = sandbox.commands.run(command)
    # Some execd versions keep line endings in each event, some drop them.
    texts = [m.text for m in execution.logs.stdout + execution.logs.stderr]
    out = "".join(t if t.endswith(("\n", "\r")) else t + "\n" for t in texts)
    print(f"$ {command}\n{out}", end="" if out.endswith("\n") else "\n")
    if execution.exit_code not in (0, None):
        sys.exit(f"exit {execution.exit_code}")
    return out


sandbox = SandboxSync.create(
    IMAGE,
    connection_config=config,
    # Lets the container create the namespaces and mounts foil uses, with a
    # tmpfs for its branches (FOIL_HOME in the image points there).
    extensions={"bootstrap.execd.isolation": "enable"},
)
try:
    run(sandbox, "foil doctor")
    sandbox.files.write_file("/workspace/app/calc.py", CALC)
    sandbox.files.write_file("/workspace/app/test_calc.py", TEST)
    sandbox.files.write_file("/workspace/app/attempt.sh", ATTEMPT)
    sandbox.files.write_file("/workspace/app/.gitignore", "__pycache__/\n")
    run(sandbox, "cd /workspace/app && git init -q && git add -A && git -c user.email=d@x -c user.name=demo commit -qm base && echo base committed")
    # The check is the original test, kept outside the project where no attempt can edit it.
    run(sandbox, "cp /workspace/app/test_calc.py /workspace/check_calc.py")
    run(
        sandbox,
        "cd /workspace/app && foil fanout -n 3 --name fix --pick "
        "--check 'PYTHONPATH=. python3 -B /workspace/check_calc.py' -- sh attempt.sh",
    )
    run(sandbox, "cd /workspace/app && git status --short && git diff && python3 -B test_calc.py")
    print("committed the attempt that passed; the other two were discarded")
finally:
    sandbox.kill()
