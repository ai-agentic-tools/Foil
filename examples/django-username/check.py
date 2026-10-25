"""Does Django reject usernames with a trailing newline (ticket #30257)?

Run from a Django checkout. Checks the cases the upstream fix added to
Django's tests, plus the usernames those tests already covered, then runs
Django's own validator tests. Exit status 0 = fixed and nothing broken.
The agents never see this file: foil runs it in each branch afterwards.
"""
import os
import subprocess
import sys

sys.path.insert(0, os.getcwd())
from django.conf import settings  # noqa: E402

settings.configure()
import django  # noqa: E402

django.setup()
from django.contrib.auth import validators  # noqa: E402
from django.core.exceptions import ValidationError  # noqa: E402

CASES = [
    (validators.ASCIIUsernameValidator(),
        ["glenn", "GLEnN", "jean-marc"],
        ["o'connell", "Éric", "jean marc", "أحمد", "trailingnewline\n"]),
    (validators.UnicodeUsernameValidator(),
        ["joe", "René", "ᴮᴵᴳᴮᴵᴿᴰ", "أحمد"],
        ["o'connell", "عبد ال", "zerowidth​space", "nonbreaking space", "en–dash", "trailingnewline\n"]),
]

failed = []
for validator, valid, invalid in CASES:
    name = type(validator).__name__
    for username in valid:
        try:
            validator(username)
        except ValidationError:
            failed.append(f"{name} rejects valid {username!r}")
    for username in invalid:
        try:
            validator(username)
            failed.append(f"{name} accepts invalid {username!r}")
        except ValidationError:
            pass

for line in failed:
    print("FAIL", line, flush=True)
if failed:
    sys.exit(1)
print("ok: both validators reject a trailing newline", flush=True)

# Nothing else broken: Django's own tests for these validators.
tests = subprocess.run(
    [sys.executable, "-B", "runtests.py", "--settings=test_sqlite", "--parallel", "1", "-v", "0", "auth_tests.test_validators"],
    cwd="tests",
    env={**os.environ, "PYTHONPATH": ".."},
)
sys.exit(tests.returncode)
