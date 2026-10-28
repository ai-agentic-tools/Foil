# Security

## Reporting a vulnerability

Please report security problems privately, through GitHub's
[private vulnerability reporting](https://github.com/ai-agentic-tools/foil/security/advisories/new),
not in a public issue. Include the foil version (`foil --version`), the
kernel and distribution, the exact command line, and what the command
inside the run could do that it should not. You'll get an answer within a
week. Fixes are released as soon as they are ready and credited to the
reporter unless they prefer otherwise.

## What foil promises

foil is a guard rail for agents and scripts that are buggy or misled (a
poisoned issue, a prompt injection, a careless `rm -rf`), running as you,
on your machine. It is **not** a boundary against an attacker with a
kernel exploit: everything runs on the host kernel, as in a rootless
container. For hostile code, run foil inside a VM.

Inside `foil run --strict` (what `foil agent` and the demos use), a command
must not be able to:

| Guarantee | Mechanism | Test (`tests/strict.rs`) |
|---|---|---|
| Write outside its branch and allowed paths | private mount namespace; tmpfs root; read-only binds | `cannot_write_outside_the_branch` |
| Read your files (`~/.ssh`, other repos, tokens) | deny-by-default view: only system folders, the workspace, `--allow-read` | `cannot_read_secrets_or_other_folders` |
| See your environment variables | environment allowlist (`--allow-env`) | `cannot_see_environment_secrets_unless_allowed` |
| Reach the network, except allowed hosts | empty network namespace; allowlist proxy outside it | `cannot_reach_the_network_or_resolve_names`, `cannot_reach_hosts_outside_the_allowlist`, `the_proxy_refuses_smuggled_and_disguised_requests` |
| See the API key it uses | key added by the proxy, outside the sandbox | `cannot_see_the_injected_api_key`, `cannot_find_the_key_anywhere_in_proc` |
| See or signal your other processes | PID and IPC namespaces | `cannot_see_or_signal_other_processes` |
| Signal foil or its caller's process group (`kill 0`) | a session of its own when stdio is not a terminal | `cannot_signal_foils_process_group` |
| Use descriptors foil inherited (an open folder or socket) | close-on-exec on everything but stdio before the command starts | `cannot_use_descriptors_foil_inherited` |
| Reach this machine's services through an allowed name (`*`, DNS rebinding, `127.1`) | the proxy resolves once, refuses internal addresses unless the host is named exactly, and connects to the address it checked | `cannot_reach_this_machine_through_a_wildcard` |
| Gain privileges (setuid) | `no_new_privs` | `cannot_gain_privileges` |
| Reach local services or host sockets | network namespace; abstract sockets are per namespace | `cannot_reach_local_services_or_host_tmp`, `cannot_reach_host_abstract_unix_sockets` |
| Trick a commit into writing outside the workspace | commit through open folder handles, never following symlinks | `commit_does_not_follow_a_symlink_the_branch_replaced` |
| Outlive the run | cgroup box + subreaper + PID namespace; `--timeout` | `leftovers_inside_a_strict_run_end_with_it` |

Without `--strict`, `--protect` only stops *writes* outside the branch;
reads are not restricted.

## Known limits

- **Same kernel.** A kernel bug reachable from an unprivileged user
  namespace breaks every guarantee above. Keep the kernel updated, or use
  a VM.
- **Allowed hosts are a channel.** Data can leave through any host you
  `--allow-host`, including the model API: `foil agent` allows tunnels to
  `api.anthropic.com` / `api.openai.com`, so a process inside could use
  that API with a key of its own. Keep the list as narrow as the task.
- **Interactive runs share your terminal.** When stdio is a terminal (an
  agent's TUI), the command stays in your terminal's session: it can
  signal foil's process group, and on kernels before 6.2 (or with
  `dev.tty.legacy_tiocsti=1`) push keystrokes into the terminal with
  `TIOCSTI`. Non-interactive runs (scripts, CI, `foil fanout`) get their
  own session and are not affected. A pseudo-terminal relay that closes
  this for interactive runs too is planned.
- **Agent logins.** Without `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` in
  foil's environment, the agent uses the login saved in its state folder
  (e.g. `~/.claude/.credentials.json`), which commands it runs can read;
  `foil agent` warns about this.
- **What you allow, it gets.** `--allow-read`, `--allow-write` and
  `--allow-env` are passed through as asked; `foil agent` allows the
  agent's own state folder (e.g. `~/.claude`) so it can work.
- **One layer.** Each guarantee rests on one kernel mechanism today. A
  second layer (Landlock, seccomp) is planned before 1.0, as is an
  external audit.
- **Resource exhaustion** is bounded only with `--memory`, `--cpus`,
  `--pids` and `--timeout`; there is no disk quota yet beyond the
  filesystem's own.
