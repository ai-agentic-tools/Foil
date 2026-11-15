# Foil: TODO plan

Last updated 2026-11-14, after 0.5.1. Order is priority. Each item lists why,
what, rough effort and how we will know it is done.

## 0. Launch (blocking everything else)

Traction on 2026-11-11: 0 stars, 5 unique visitors in 14 days, 24 crates.io
downloads. No public posts yet.

- [ ] Show HN, linking `docs/blog/agent-guardrails.html`. Hook: "Claude Code
      in a sandbox it can't escape, same SWE-bench score (42.0 vs 42.7 / 50)".
- [ ] r/ClaudeAI and r/LocalLLaMA with a short demo clip: `.env` refused,
      pypi blocked and logged.
- [ ] X / LinkedIn post with the benchmark table.
- [ ] Claude Code community and Discord channels.
- [ ] Recommend Foil to hesreallyhim/awesome-claude-code. It must be done by
      a person through the web issue form; the description is in the session
      notes.
- [ ] PRs to active lists. Both are drafted, but forking needs the user's OK:
  - [ ] ottosulin/awesome-ai-security, in "Agent Runtime Security & Sandboxing".
  - [ ] scadastrangelove/awesome-ai-security-tools, in `WATCHLIST.md`.
- [ ] Nudge the open, unanswered PRs with one comment each, pointing to 0.5.1
      and the benchmark:
  - [ ] restyler/awesome-sandbox #47.
  - [ ] bytedance/SandboxFusion #83. This is also the way into verl (see 3).
- [ ] Five-developer pilot from the strategy doc; collect quotes for the site.
- [ ] Housekeeping:
  - [ ] Dependabot #9 (rustls) needs `@dependabot rebase`, then a merge.
  - [ ] Lima VM: chrony returns on reboot. Stop it before any benchmark.

## 1. Always-on daemon: remove the CLI step (next build)

Why: typing `foil agent claude`, then `foil diff`, then `foil commit` is a
new habit, and that friction blocks adoption. Goal: install once and keep
typing `claude`.

Hooks alone are not enough. Claude Code runs Read, Edit and Write inside its
own process, so a hook can block a call but cannot sandbox it. Hooks also
miss MCP servers, plugins and new tools, and they live in a file the agent
can edit. So process isolation stays the boundary, and hooks add a second
layer that sees what the agent intends.

- [ ] **`foild` daemon** (about 1–2 weeks), built on `foil serve`'s
      lifecycle code. It starts at login and keeps, per repo:
  - [ ] A warm branch, ready before launch.
  - [ ] An always-up proxy with allowlist, keys and logins held, and an
        audit stream.
  - [ ] The repo's policy, from `.foil/policy`.
- [ ] **Shims** (days): `foil install claude|codex|cursor-agent` puts a small
      launcher first on PATH, so the agent starts inside foil through
      `foild`. Isolation is the same as today.
- [ ] **Hook integration** (about 1 week): foil writes a guarded `PreToolUse`
      hook, and every tool call goes to `foild`.
  - [ ] Each tool call recorded in the audit log, which fills the "doesn't
        record each command" limit in SECURITY.md.
  - [ ] Per-call policy.
  - [ ] Approval prompts for risky actions.
  - [ ] Claude Code first. Codex and Cursor have less hook support.
- [ ] **Review surface** (about 1–2 weeks): `foil review`, plus a menu-bar app
      or VS Code extension showing "Agent finished: N files changed, M
      guarded. Review / commit / abort".
- [ ] **Managed mode** (later): security pushes org policy to `foild`, which
      streams audit to their log collector.
- Caveat to document: running the real binary by full path skips the shim.
  In managed mode, use MDM or file permissions to enforce it.
- Done when: `brew install foil && foil install claude`, then a plain
  `claude` session, is sandboxed, every tool call is in the audit log, and a
  notification offers review.
- Pitch: "On by default, invisible until it matters."

## 2. Security boundary hardening (before 1.0)

Each item removes a line from SECURITY.md "Known limits". Each needs a test
that tries to break it, like the existing guarantees.

- [ ] **Terminal relay** (about 1 week). Interactive runs share the user's
      terminal: the agent can signal foil, and on older kernels it can push
      keystrokes with `TIOCSTI`. Give the agent a pty of its own and have
      foil relay the bytes.
- [ ] **Landlock + seccomp** (about 1–2 weeks). Today each guarantee rests on
      one kernel mechanism.
  - [ ] Landlock file rules as a second layer under the mounts.
  - [ ] seccomp: deny `ptrace`, `keyctl`, `bpf`, `userfaultfd`,
        `perf_event_open` and the new mount APIs. This cuts off kernel
        surface that exploits use.
- [ ] **Proxy holds agent logins** (about 1 week). OAuth credentials such as
      `~/.claude/.credentials.json` stay outside the sandbox, and the agent
      sees a placeholder.
- [ ] **Proxy key check** (days). Refuse API requests that carry a key other
      than foil's, and optionally cap egress volume. This closes the "own key
      through an allowed host" channel.
- [ ] **Sealed audit log.** Hash-chain the entries, and optionally stream them
      to syslog or OpenTelemetry as they are written.
- [ ] **Fuzzing in CI** of the proxy's HTTP/CONNECT handling and the policy
      parser, plus a set of known container-escape techniques run against
      every release.
- [ ] **Signed releases** with cosign, plus an SBOM.
- [ ] **Content-based secret scan**, so key-shaped strings in fixtures or
      backups get warned about and hidden. `--hide` covers paths only.
- [ ] **Flag more files that run later** in `foil diff`: `Makefile`,
      `package.json` scripts, CI workflows.
- [ ] **Disk quota** per branch.
- [ ] **macOS**: tighten Seatbelt (process listing, Mach services) and steer
      untrusted code to `foil vm`.
- [ ] **External security audit.**
- [ ] **Optional VM isolation** (about 3–4 weeks): `--isolation vm` runs the
      same branch, policy, proxy and audit inside Firecracker (Linux) or the
      existing Lima path (macOS). This removes the shared-kernel limit for
      hostile code.
- Order: terminal relay, Landlock + seccomp, proxy-held logins, then the
  audit and the sealed log before 1.0. VM mode when users ask.
- Release story: "0.6: two independent layers, no shared terminal, the agent
  never sees a credential."

## 3. RL environment for coding agents (second audience)

Why: RL rollouts need a fast reset, many parallel copies of one prepared
repo, no leftovers, rootless clusters, and protection against reward
hacking. Foil already has all of these:

- 25 ms branches.
- OpenSandbox create: 37 ms vs 1,143 ms.
- SandboxFusion: 14.4 vs 3.7 programs/s, with nothing left behind.
- `--hide`, `--no-network`, an allowlist and an audit log.

- [ ] **`foil-env` Python package** (about 1–2 weeks), gym-style:
      `reset()` makes a new branch, `step(cmd)` runs a command, `grade(tests)`
      applies the hidden tests in a separate branch the agent never sees, and
      `close()` aborts.
- [ ] **Reward-hacking benchmark**, the marketing centrepiece. About 20 tasks,
      each with a planted shortcut:
  - Readable hidden tests.
  - A writable grader.
  - Network access to the upstream fix.
  - The fix in a future git commit.

  Run a real agent in a plain container and under foil, and count cheats and
  blocks, with the audit log as evidence. Estimated $5–10 in API.
- [ ] **Throughput benchmark**: rollouts per hour on a 64-core machine, foil
      vs Docker vs a microVM, on the same SWE-bench task.
- [ ] **Framework adapters**, each a small upstream PR:
  - [ ] verl, via SandboxFusion #83.
  - [ ] SkyRL.
  - [ ] OpenRLHF.
  - [ ] R2E-Gym / SWE-Gym.
- [ ] **`foil pool`**: pre-warmed branches. Also a JSON trajectory export from
      the audit log.
- Pitch: "Rollouts in 25 ms that can't cheat: a rootless RL environment for
  coding agents."
- Audience: RL infra engineers (verl, SkyRL, Prime Intellect, Nous, Allen AI)
  and the SWE-Gym / R2E-Gym authors.
- Limits to state up front: host kernel; Linux at scale; no process-memory
  checkpoints; GPUs out of scope.

## 4. Checkpoints and the DeltaBox link

DeltaBox (arXiv 2605.22781) checkpoints files and process memory in
milliseconds inside Firecracker, for tree search and RL. No public code was
found as of 2026-11-14. It complements foil rather than competing: it covers
search speed, and foil covers safety on real repos.

- [ ] **`foil checkpoint NAME` / `foil rollback NAME`**, files only. Freeze
      the upper layer into a read-only layer and start a fresh one (overlayfs
      lowerdir stacking, APFS clones on macOS).
  - Easy: between runs.
  - Hard: mid-run.
  - Process memory needs CRIU, which mostly needs root. Leave that to
    DeltaBox.
- [ ] **If DeltaBox releases code**, build an adapter:
  - The foil branch is DeltaBox's workspace.
  - DeltaBox's network proxy daemon goes through foil's proxy.
  - The winning changes are written back into the branch, then `foil diff`
    and `foil commit`.
- [ ] **Email the DeltaBox authors** proposing the joint integration ("DeltaBox
      for search, foil for safety on real repos").

## Suggested sequence

1. Launch (section 0): this week.
2. `foild` + shims, then hook integration (section 1).
3. Terminal relay, Landlock + seccomp, proxy-held logins (section 2) → 0.6.
4. `foil-env` + reward-hacking benchmark (section 3), as a second launch.
5. File checkpoints (section 4) if users ask, plus the audit and VM mode
   toward 1.0.
