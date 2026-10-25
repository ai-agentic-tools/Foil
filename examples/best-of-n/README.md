# Best of N, safely

Agents aren't deterministic: ask four times and you get four different fixes,
some wrong, one maybe malicious if the agent read a poisoned issue or web page.
`foil fanout` runs every attempt at once, each in its own copy-on-write branch
of your repo (about 25 ms each, whatever the repo's size), runs your tests in
each branch, and keeps the smallest passing change. Your folder doesn't change
until you pick.

```sh
./demo.sh        # scripted agents, no API key, ~10 s
./claude.sh      # the same with three real Claude Code sessions
```

What `demo.sh` shows, on a repo whose `slugify()` fails 3 of 4 tests:

```
foil: 4 attempts of fix in 5.2 s
  attempt  status   check  changes  time
  fix-1   0        fail   1        0.0 s     wrong fix
  fix-2   0        pass   3        0.0 s     right, but rewrites three files
  fix-3   0        pass   1        0.0 s     right and minimal  <- best
  fix-4   timeout  fail   0        5.0 s     rogue
```

Attempt 4 tries to read `~/.ssh/id_ed25519` and a secret next to the repo,
append a backdoor to a shell profile, reach `example.com`, and then spins
forever. Under `--strict` none of those paths exist for it, there is no
network, it sees only its own six processes, and `--timeout` stops it. Then:

```sh
foil diff --patch fix-3     # review the winner
foil pick fix-3             # commit it; fix-1, fix-2 and fix-4 are discarded
```

The same thing in your own repo:

```sh
foil fanout -n 4 --agent claude --timeout 15m --check "make test" -- -p "fix issue #123"
```
