# kanban2

A local kanban board for Claude Code agents. Each card that reaches **In
Progress** gets its own detached git worktree and a live `claude` process; the
terminal streams to the browser over a WebSocket, and a GitHub-style diff pane
beside it turns review comments back into prompts.

```
To Do  ──drag──▶  In Progress  ──agent idles──▶  In Review  ──merge──▶  Done
                  worktree +                      diff +                 agent lands
                  live agent                      comments               commits on base
```

## Running it

```sh
direnv allow          # or: nix develop
pnpm install
pnpm build            # populates static/ — required before cargo run
cargo run             # http://127.0.0.1:8770
```

The flake supplies node, pnpm, and esbuild. Rust comes from your system
toolchain on purpose, so `cargo` stays whatever you already use.

`pnpm watch` rebuilds assets on change. Templates reload without a restart.

## How it works

**Worktrees.** Entering In Progress runs `git worktree add --detach` under
`$XDG_DATA_HOME/kanban2/worktrees/<card>` and records the starting commit as
`refs/kanban2/<card>/base`.

**Turns.** Claude Code HTTP hooks, passed per-session via `--settings`, report
each `Stop`. The server then commits the *working tree* as
`refs/kanban2/<card>/turn-<n>` — the agent does not have to commit for a turn to
be captured. Staging happens in a scratch `GIT_INDEX_FILE`, so the worktree's
own index is never disturbed.

This is what makes the diff scopes work: `base..turn-N` for everything,
`turn-(N-1)..turn-N` for one round, `turn-N..latest` for everything since.

**Review.** Click any diff line to comment. *Submit review* formats the drafts
into one message and pastes it into the agent's terminal.

**Merge.** Available in In Review. The server asks the agent to land its commits
on the base branch and never rewrites branches itself. On the next turn it
checks that the branch moved *and* that its tree matches the latest snapshot
before marking the card Done and pruning the worktree. Turn refs are kept.

## Configuration

`APP_SLUG` in `src/config.rs` names both the data directory and the git ref
namespace. The port is fixed in `Rocket.toml` because hook URLs have to be
stable.

Per-card permission mode and model are set on the card form. `bypassPermissions`
shows a one-time consent dialog in the terminal — answer it there; the card
reports `needs permission` until you do.

## Cleaning up

```sh
rm -rf ~/.local/share/kanban2
git -C <project> worktree prune
git -C <project> for-each-ref --format='%(refname)' 'refs/kanban2/**' |
  xargs -n1 git -C <project> update-ref -d
```
