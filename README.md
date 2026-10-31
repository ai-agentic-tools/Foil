# Foil: a local sandbox for AI coding agents

[![CI](https://github.com/ai-agentic-tools/foil/actions/workflows/ci.yml/badge.svg)](https://github.com/ai-agentic-tools/foil/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/ai-agentic-tools/foil)](https://github.com/ai-agentic-tools/foil/releases/latest)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)

**Let AI agents loose on your repo without letting them loose on your machine.**

Run Claude Code, OpenAI Codex or any other agent or script in an instant
copy-on-write branch of your project, then review the diff and commit it, or
throw it away. No Docker, no root, no VM.

`foil` gives a coding agent an instant, private copy-on-write *branch* of your
working folder. The agent edits, builds and tests freely; your real folder
does not change until you review the diff and `foil commit`. Add `--protect`
and nothing outside the branch is writable; add `--no-network` and nothing
leaves the box; add `--memory`/`--cpus`/`--pids` and a runaway process can't
take the machine down with it. When the run ends, every process it started
is gone, even daemons that double-forked away.

```sh
foil agent claude                  # Claude Code, in a strict branch of this folder
# ... work with the agent; when it exits, foil shows what it changed and asks:
# Apply to ~/src/app? [c]ommit / [k]eep for later / [a]bort
```

`foil agent` runs the agent with `--strict`: it sees only system folders,
the branch and its own state folder, can't read `~/.ssh` or your tokens,
can't see the API key it uses (foil's proxy adds it to requests on the way
out), and can reach only the hosts its profile allows. Any command works the
same way with `foil run`:

```sh
foil run -b fix-login --strict --allow-host pypi.org --memory 8G -- ./my-agent
foil diff --patch fix-login        # review exactly what changed
foil commit fix-login              # apply it (or: foil abort fix-login)
```

No root, no daemon, no container image, no copy: one static 2 MB binary on
plain Linux.

## Demo: three agents fix a real Django bug

[Django ticket #30257](https://code.djangoproject.com/ticket/30257): the
username validators accepted `"alice\n"`. [`examples/django-username`](examples/django-username)
rewinds Django to that bug and lets three Claude Code sessions fix it at once,
each in its own branch of the 6 000-file checkout. foil checks every branch
and commits the best passing fix. A real run:

```
foil: 3 attempts of username in 18.8 s
  attempt      status   check  changes  time
  username-1   0        pass   1        17.6 s
  username-2   0        pass   1        13.3 s
  username-3   0        pass   1        15.6 s

== What each agent could reach (the same sandbox, probed)
API key it sees:  foil-injects-the-real-key
~/.ssh:           ls: cannot access '/home/me/.ssh': No such file or directory
processes:        5 visible
example.com:      urllib.error.URLError: <urlopen error Tunnel connection failed: 403 Forbidden>

== foil pick username-1
applied 1 change; discarded username-2, username-3
```

The same in your repo:

```sh
foil fanout -n 3 --agent claude --timeout 15m --check "make test" -- -p "fix issue #123"
```

No API key? [`examples/best-of-n/demo.sh`](examples/best-of-n) runs in five
seconds with scripted agents, one of which goes rogue (reads `~/.ssh`, plants
a backdoor, phones home, hangs) and gets nothing. [`demo.sh`](demo.sh) shows a
single misbehaving agent contained by `--protect`, `--no-network` and
`--memory`.

## Why

Agents are most useful when they can run things: install packages, run the
test suite, try a refactor, throw it away, try again. Today that means
either watching every command, or trusting the agent with your whole home
directory. Copying the repo (or a git worktree per agent) is slow for big
repos, misses untracked files and build state, and protects nothing outside
the copy.

| On a 1 GB, 10 000-file workspace | `cp -a` | `foil run -b` |
|---|---|---|
| Time to a usable private copy | 1.9 s | **~25 ms** (any size) |
| Extra disk | 977 MB | **only what changes** (3 branches: 80 KB) |
| Diff / commit a one-file change | — | 5 ms / 6 ms |

*Measured on Ubuntu 26.04, Linux 7.0, 4 vCPU VM. The branch cost does not
grow with the workspace: nothing is copied until it is written.*

## How foil compares

| | git worktree | Docker / dev container | Cloud sandbox (E2B, Daytona, OpenSandbox) | Agent's own sandbox mode | **foil** |
|---|---|---|---|---|---|
| Ready in | seconds (checkout) | seconds to minutes (image) | under a second to seconds, plus upload | instant | **~25 ms, any repo size** |
| Has untracked files and build state | no | if mounted or copied | if uploaded | yes | **yes** |
| Agent writes reach your folder | in the worktree; you merge | directly, if mounted | after you download them | directly | **only when you `foil commit`** |
| Review the whole change first | via git | no | no | no | **`foil diff`, then commit or abort** |
| Hides `~/.ssh`, tokens, other repos | no | yes | yes | partly | **yes (`--strict`)** |
| Network allow-list, key hidden from agent | no | add a proxy | varies | partly | **yes (`--allow-host`, `--inject-key`)** |
| Needs | git | a daemon and an image | an account, a network | the agent | **one 2 MB binary** |
| Kernel isolation | none | shared kernel | microVM or VM | shared kernel | shared kernel ([use a VM for hostile code](#what-foil-is-not)) |

Use a cloud microVM for code you don't trust at all. Use foil for the agents
you run every day on your own projects, where the risk is a wrong or misled
agent, not an attacker with a kernel exploit.

## What you get

- **Branches.** `foil run -b NAME` runs the command on a copy-on-write view of
  the folder. Run many branches of the same folder in parallel, one per
  agent. A branch persists across runs until you commit or abort it.
- **Review, then commit.** `foil diff` lists changes (`A`/`M`/`D`/`R`),
  `--patch` shows a unified diff, `--json` feeds your tooling.
  `foil commit` applies the branch. It **refuses** if any file it changes was
  also changed in the real folder since the branch was made (say, by
  another agent's commit), naming each one; `--force` overrides. Commits
  into one folder take turns, so two agents can never both win.
- **Crash-safe commit.** A commit records what it will do before it starts
  and syncs to disk before it deletes the branch. If foil is killed, the
  machine loses power or the disk fills half-way, run `foil commit NAME`
  again and it finishes the same commit. (`foil abort` then refuses, since
  the folder already has part of the change; `abort --force` keeps it as it
  is.)
- **Many attempts at once.** `foil fanout -n 3 --check "make test" -- ...`
  runs three attempts in parallel branches, checks each, and `foil pick`
  commits the best (`--json` for harnesses).
- **`--timeout 10m`**: a run that hangs is stopped (SIGTERM, then SIGKILL)
  and exits 124, like `timeout(1)`.
- **`--protect`**: the whole filesystem is read-only to the command except
  the branch, `/tmp`, `/var/tmp`, `/dev/shm` and any `--allow-write PATH`.
  An agent cannot touch `~/.ssh`, your dotfiles, other repos, or the
  branch store.
- **`--no-network`**: the command gets a private network with only loopback.
- **`--strict`** (what `foil agent` uses): deny by default. The command sees
  a private root with the system folders (read-only), the workspace, a
  private `/tmp`, an empty home folder and only the paths you
  `--allow-read` / `--allow-write`; its own PID and IPC namespaces (it can't
  see or signal your other processes); `no_new_privs` (no setuid
  escalation); a clean environment (`--allow-env` to pass a variable); and
  no network except `--allow-host` hosts through foil's proxy, which can
  also add an API key the command never sees (`--inject-key anthropic`).
  `tests/strict.rs` is the escape suite: each guarantee has a test that
  tries to break it.
- **Limits**: `--memory 4G`, `--cpus 2`, `--pids 512`. Exceeding memory kills
  the command, and foil says so instead of leaving you with a bare exit 137.
- **Nothing left behind.** Every run gets its own cgroup; when the command
  exits, stragglers get SIGTERM, a grace period (`FOIL_GRACE_MS`, default
  1000), then SIGKILL. Even if foil itself is `kill -9`ed, the next foil to
  touch that branch stops the leftovers first, so a commit never races a
  writer. (This needs the cleanup box, which systemd provides on normal
  desktops and servers; `foil doctor` tells you.)
- **Transparent.** Same user, same paths, same terminal, same environment;
  signals are forwarded; foil exits exactly as the command did.

## Install

Linux, kernel 5.12 or newer (Ubuntu 22.04+, Debian 12+, Fedora 36+).
Tested on x86_64; other architectures build from source but are untested.

```sh
curl -fsSL https://raw.githubusercontent.com/ai-agentic-tools/foil/main/install.sh | sh
foil doctor           # what works on this machine, and how to fix what doesn't
```

The script downloads the latest release binary, checks its SHA-256, puts it
in `/usr/local/bin` and runs `foil setup` where needed. To build from
source instead: `git clone https://github.com/ai-agentic-tools/foil && cd foil &&
./install.sh` (needs Rust; `musl-tools` for a static binary).

**Ubuntu 23.10 and later** only let approved programs create user
namespaces. `sudo foil setup` installs a small AppArmor profile approving the
foil binary at its current path (as browsers and Podman do). Run it again if
you move the binary.

`foil doctor` on a healthy machine:

```
ok  kernel        Linux 7.0.0-34-generic (branches need >= 5.11, --protect >= 5.12)
ok  cleanup box   branch, isolated and limited runs get one through a systemd scope (~12 ms); ...
ok  branches      copy-on-write branches work (stored in /home/me/.local/share/foil)
ok  --protect     read-only view of everything outside the workspace works
ok  --no-network  network isolation works
ok  limits        --memory/--cpus/--pids via systemd (controllers: cpuset cpu io memory pids)
```

## Recipes

**One agent, one task, reviewed before it lands**

```sh
cd ~/src/app
foil run -b task-42 --protect --allow-write ~/.claude --memory 8G -- claude
foil diff --patch task-42 | less
foil commit task-42
```

The agent itself needs the network (its API) and somewhere to keep its own
state: allow its state folder, not your whole home directory.
`--allow-write` also takes single files, but programs that save a file by
writing a temporary file next to it and renaming it need the folder.

**Several agents on the same repo at once**

```sh
for n in 1 2 3; do
  foil run -b attempt-$n --protect --memory 4G -- my-agent "make the build 2x faster" &
done; wait
foil diff attempt-1; foil diff attempt-2; foil diff attempt-3
foil commit attempt-2 && foil abort attempt-1 && foil abort attempt-3
```

**Untrusted code: someone else's PR, an install script, a test suite**

```sh
foil run -b pr-1234 --protect --no-network --memory 2G --pids 256 -- make test
foil abort pr-1234
```

**An orchestrator that starts many runs at once**

```sh
foil session -- ./my-agent-harness      # every foil run inside starts fast
```

Without a cgroup it may manage, each run asks systemd for one, and systemd
handles those requests one at a time (50 runs at once: ~1.5 s each).
Inside one `foil session` they don't need to (50 runs: ~65 ms in total).
`foil fanout` does this by itself.

**In CI or scripts**

```sh
foil run -b ci-$RUN_ID --protect -- ./generate-code.sh
foil diff --json ci-$RUN_ID          # [{"kind":"M","path":"src/api.rs"}, ...]
```

## Commands

```
foil run [options] [--] <command> [args...]
  -b, --branch NAME     run in branch NAME of the workspace (created if new)
  -w, --workspace DIR   the folder a new branch branches (default: .)
  --protect             everything except the branch and temp dirs is read-only
  --allow-write PATH    with --protect or --strict: PATH writable (repeatable)
  --no-network          no network (loopback only)
  --strict              deny by default (see above)
  --allow-read PATH     with --strict: show PATH read-only (repeatable)
  --allow-host HOST     with --strict: reachable through the proxy (host,
                        host:port, *.domain; repeatable)
  --inject-key API      with --strict: the proxy adds API's key (anthropic,
                        openai) from foil's environment
  --allow-env NAME      with --strict: pass environment variable NAME
  --setenv NAME=VALUE   with --strict: set NAME inside the run
  --memory SIZE         memory limit, e.g. 2G
  --cpus N              CPU limit, e.g. 1.5
  --pids N              process and thread limit
  --timeout TIME        stop the command after TIME (90s, 10m, 2h); exit 124
foil agent NAME [run options] [-- agent args]    NAME: claude, codex
foil diff [--patch | --json] [--all] NAME
foil fanout -n N [--check CMD] [--agent NAME] [--name BASE] [--pick] [--json] [run options] [--] ...
foil pick NAME
foil session [--] <command> [args...]
foil commit [--force] [--all] NAME
foil abort [--force] NAME
foil list [--json]
foil doctor
foil setup [--print]
```

Exit status: the command's own, or 124 if `--timeout` stopped it, 125 if
foil could not set the run up, 126 if the command could not be executed,
127 if it was not found.

Environment: `FOIL_HOME` (branch store, default `~/.local/share/foil`; must
be outside the workspace), `FOIL_GRACE_MS`, `FOIL_NO_SCOPE=1` (never create
a systemd scope), `FOIL_NO_CGROUP=1` (no cleanup box).

## What foil is not

foil is a guard rail, not a security boundary against a determined attacker.

- **Without `--strict`, reads are not restricted.** `--protect` stops writes;
  the command can still *read* whatever you can. `--strict` hides everything
  not explicitly shown. Even then, data can leave through hosts you allow, so
  keep `--allow-host` lists as narrow as the task.
- **Same kernel.** Isolation uses Linux namespaces, like rootless containers;
  it does not protect against kernel exploits. Use a VM for hostile code.
- **Interactive strict runs share your terminal** (an agent's TUI needs
  it); see [SECURITY.md](SECURITY.md) for what that allows on older
  kernels. Scripts, CI and `foil fanout` are not affected.
- **`/tmp` is shared** with the rest of the machine under `--protect` (it is
  private under `--strict`).
- **Commit is per file atomic, not all at once.** Readers of the folder can
  see a commit half-done while it runs; if it stops half-way, `foil commit`
  finishes it.
- **Conflicts are detected per path, not merged.** A conflicting commit is
  refused; you choose to force it or abort. Detection uses change times: a
  change made in the same instant just before the branch can be flagged
  (the safe mistake); one made after is never missed.
- **Renaming a folder inside a branch is a copy.** overlayfs answers a folder
  rename with "cross-device", so tools like `mv` copy and delete instead:
  slower for big folders, and it fails half-way on a folder whose owner
  removed their own search (`x`) permission. Files rename normally.

## How it works

`foil run` forks the command into a fresh cgroup and, before it starts, into
a new user namespace that maps your own uid to itself (so no root), a new
mount namespace in which an overlayfs is mounted right over the workspace
path (reads fall through to the real folder; writes land in the branch's
upper layer), optionally a read-only remount of everything else, and
optionally an empty network namespace. When the command and everything it
started have exited, the namespaces vanish and the branch's changes sit in
`$FOIL_HOME/branches/NAME/upper`, ready for `diff` and `commit`.

Limits use a transient systemd scope (`systemd-run --user --scope`), which
foil creates by re-running itself inside it; the same scope gives the run a
cleanup cgroup when your login shell has none.

## FAQ

**How do I run Claude Code in a sandbox?** `foil agent claude` in your
project. Claude Code runs in a strict branch: it can edit, build and test,
can reach only Anthropic's API and the hosts you allow, never sees your API
key, and changes nothing until you commit. `foil agent codex` does the same
for OpenAI's Codex CLI.

**Can several agents work on the same repo at once?** Yes. Each gets its own
branch (`foil run -b NAME`), and they don't see each other's changes.
`foil fanout -n 3 --check "make test"` runs N attempts in parallel and
commits the best passing one. A commit is refused if another commit already
changed the same file.

**What about other agents (Aider, Gemini CLI, OpenHands, my own)?**
`foil run --strict --allow-host HOST -- COMMAND` works with any command;
`foil agent` adds ready-made profiles for Claude Code and Codex.

**How is this different from git worktrees?** A worktree is a second
checkout: tracked files only, no build state, slower for big repos, and it
protects nothing outside itself. A foil branch is the whole folder as it is,
in about 25 ms, with the rest of the machine out of reach.

**Do I need Docker, root or a daemon?** No. foil uses Linux user namespaces
and overlayfs as your own user. On Ubuntu 23.10 or newer, `sudo foil setup`
once allows user namespaces for the foil binary.

**Is it a security boundary?** Against a buggy, careless or prompt-injected
agent, yes: that is what it is built and tested for
([`tests/strict.rs`](tests/strict.rs) tries each escape). Against a kernel
exploit, no: run hostile code in a VM. See [SECURITY.md](SECURITY.md).

**Which systems are supported?** Linux 5.12 or newer (Ubuntu 22.04+,
Debian 12+, Fedora 36+). Windows: in a Linux VM.

## Development

```sh
cargo test                                     # in a Linux VM or box
systemd-run --user --scope cargo test          # with a delegated cgroup
FOIL_REQUIRE_BRANCH=1 FOIL_REQUIRE_LIMITS=1 ... # make skips into failures
```

Tests that need user namespaces, a delegated cgroup or systemd skip
themselves where those are missing; CI runs them all, required.

See [CONTRIBUTING.md](CONTRIBUTING.md) for the full test setup, and
[SECURITY.md](SECURITY.md) for the threat model and how to report a
vulnerability.

## License

[Apache License 2.0](LICENSE). Free for any use, commercial included, with a
patent grant from every contributor. The one condition is credit: keep the
copyright notice and the [NOTICE](NOTICE) file in copies and derived works.

Contributions are accepted under the [Contributor License Agreement](CLA.md),
which a bot asks for on your first pull request (one comment, once).
Releases up to 0.3.0 remain available under MIT or Apache-2.0, as published.
