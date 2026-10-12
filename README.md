# foil

Project Foil: instant, throwaway copy-on-write branches of a workspace for
AI agents, plus a supervisor that leaves no stray processes behind. Linux only.

```sh
foil run -- <command> [args...]                  # supervise a command
foil run --branch try-1 -- <command> [args...]   # run it in a private branch of the current folder
foil diff try-1                                  # A added, M modified, D deleted, R replaced
foil commit try-1                                # apply the branch to the folder
foil abort try-1                                 # throw the branch away
foil list
```

Branches need Linux >= 5.11 with unprivileged user namespaces. They are
stored in `$FOIL_HOME` (default `~/.local/share/foil`), which must be outside
the workspace. Leftover processes get SIGTERM and `FOIL_GRACE_MS` (default
1000) to exit before SIGKILL.
