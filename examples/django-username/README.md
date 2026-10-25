# Three agents fix a real Django bug, safely

[Django ticket #30257](https://code.djangoproject.com/ticket/30257): Django's
username validators accepted `"alice\n"`. Their pattern ended in `$`, which in
Python also matches just before a trailing newline. Django fixed it in
[cbf7e71](https://github.com/django/django/commit/cbf7e71558c94ce1c327b683768a18a25d82d197)
by using `\A…\Z` (it is SWE-bench task `django__django-11099`).

This example rewinds Django to the commit before that fix and lets three
Claude Code sessions fix it at the same time, each in its own `foil` branch of
the same 6 000-file checkout. foil runs a check in every branch (the cases
Django's own fix added to its tests, plus Django's validator tests), shows you
the best passing fix, and commits it. The agents never see the check.

```sh
./setup.sh                              # once: Django at the bug, Python 3.9 via uv (~10 s)
ANTHROPIC_API_KEY=sk-ant-... ./run.sh   # ~20 s, a few cents with Haiku (MODEL=sonnet, N=5 to change)
```

Needs: Linux with foil installed, [`uv`](https://docs.astral.sh/uv/),
[Claude Code](https://docs.claude.com/en/docs/claude-code) on PATH, and a
workspace API key.

## What you see

A real run (Ubuntu 26.04, 4 vCPUs, Claude Haiku 4.5):

```
foil: 3 attempts of username in 18.8 s
  attempt      status   check  changes  time
  username-1   0        pass   1        17.6 s
  username-2   0        pass   1        13.3 s
  username-3   0        pass   1        15.6 s

== What each agent said it did
attempt-1: Both username validators in `django/contrib/auth/validators.py` now use
           `r'\A[\w.@+-]+\Z'`, so usernames ending in a newline are rejected;
           the auth_tests suite passes (494 tests, 7 skipped).
...

== What each agent could reach (the same sandbox, probed)
API key it sees:  foil-injects-the-real-key
home folder:      .cargo .local foil-django-demo (workspace, Python, tools on PATH)
~/.ssh:           ls: cannot access '/home/me/.ssh': No such file or directory
processes:        5 visible
example.com:      urllib.error.URLError: <urlopen error Tunnel connection failed: 403 Forbidden>

== The best attempt, username-1, before it touches the checkout
(checkout clean)
-    regex = r'^[\w.@+-]+$'
+    regex = r'\A[\w.@+-]+\Z'
   (both validators)

== foil pick username-1
applied 1 change; discarded username-2, username-3

== Django, fixed (check.py outside foil)
ok: both validators reject a trailing newline
Ran 22 tests ... OK
```

Each agent worked with full permissions inside its branch: it edited files,
ran Django's test suite (494 tests), and tried its own fix. Your checkout
didn't change until `foil pick`. The agents could reach only
`api.anthropic.com`, through foil's proxy, which adds the real API key on the
way out; inside, the key is a placeholder.

## The same in your own repo

```sh
foil fanout -n 3 --agent claude --timeout 15m --check "make test" -- -p "fix issue #123"
foil diff --patch <best>     # review
foil pick <best>             # commit it, discard the rest
```
