# kanban2

A local kanban board for Claude Code agents. Each card that reaches **In
Progress** gets its own detached git worktree and a live `claude` process; the
terminal streams to the browser over a WebSocket, and a GitHub-style review pane
beside it turns review comments back into prompts.

The board is the whole interface. Opening a card, switching project, and both
forms are drawers and modals over it, each with its own URL — so a reload lands
back where you were and the back button closes what is open.

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

**Review.** The diff is rendered by piping `git diff` through [delta][] at full
context. Full context is what makes highlighting correct: a block comment or
string opened above the visible window would otherwise leave everything after it
mis-coloured. Delta also marks the part of a line that actually changed, so a
one-word edit reads as one word rather than a replaced line.

Delta is run with its backgrounds pinned to sentinel colours, so its output is a
vocabulary we control; `src/review/ansi.rs` maps those and the syntax palette to
CSS classes, keeping the actual colours in `app.css`.

The parse is cached per resolved commit pair and holds every line of the file, so
selecting a file, opening a hunk, or widening to the whole file is a re-slice
rather than another run.

One file is shown at a time, picked from the tree beside it. The diff opens with
three lines of context; *Expand N lines above/below* takes a bite out of a gap
and *Expand whole file* opens all of them. What is open lives in the pane's
query string, so nothing about it is server state. Ticking *Viewed* folds a file
away, and that much is remembered per card.

Click any diff line to comment; clicking away saves it as a draft. *Send N to
agent* formats the batch into one message and pastes it into the agent's
terminal.

**Talking to the terminal.** Every message the server sends the agent — the
opening task, a review — goes in as a bracketed paste, because the TUI reads a
bare newline as *submit*. Nothing is sent until the input box is actually on
screen, and the submit key is not sent until the paste is visibly in it: a
client still starting up *queues* what it is handed, somewhere the box is not
and Enter cannot reach, and a dialog — the workspace-trust prompt, the
`bypassPermissions` consent — swallows it, where a blind Enter would answer the
dialog instead. Delivery is confirmed by watching the box let go of the text,
so "sent" means sent.

[delta]: https://github.com/dandavison/delta

**Merge.** Available in In Review. The server asks the agent to land its commits
on the base branch and never rewrites branches itself. On the next turn it
checks that the branch moved *and* that its tree matches the latest snapshot
before marking the card Done and pruning the worktree. Turn refs are kept.

## Configuration

Settings live in `Rocket.toml` beside Rocket's own and are read from the same
figment, so any of them can be overridden per-run with a `KANBAN2_` environment
variable:

| Key | Default | What it does |
| --- | --- | --- |
| `app_slug` | `kanban2` | Names the data directory and the `refs/<slug>/` namespace |
| `data_dir` | `/<slug>` | Database, worktrees, per-card scratch |
| `agent_bin` | `claude` | The executable spawned for an agent |

The port is fixed in `Rocket.toml` because hook URLs have to be stable.

Per-card permission mode and model are set on the new-card form, whose one Task
field doubles as the card's title: the first line names the card, the whole
thing is what the agent is told. `bypassPermissions` shows a one-time consent
dialog in the terminal — answer it there; the card reports `needs permission`
until you do.

## Layout

Code is grouped by domain rather than by kind, so a change usually lands in one
folder:

```
src/project/   project.rs card.rs board.rs   models, their SQL, their routes
src/agent/     agent.rs session.rs terminal.rs webhooks.rs
src/review/    turn.rs comment.rs scope.rs expand.rs viewed.rs
               diff.rs ansi.rs cache.rs routes.rs
src/           config.rs db.rs git.rs hooks.rs tmpl.rs
```

`board.rs` owns the shell every page is: `Shell::render` draws the board and
whatever overlay a route asked for, so there is one template for the whole app
and one place that decides what is on screen.

Each entity owns its own queries — `Card::find`, `Turn::latest`,
`Comment::drafts` — rather than a shared query module.

## Cleaning up

```sh
rm -rf ~/.local/share/kanban2
git -C <project> worktree prune
git -C <project> for-each-ref --format='%(refname)' 'refs/kanban2/**' |
  xargs -n1 git -C <project> update-ref -d
```

## Tests

```sh
cargo test    # diff parsing, paste-needle selection, scope round-tripping
pnpm e2e      # end-to-end, in a real browser
pnpm e2e:ui   # the same, in Playwright's interactive runner
```

The end-to-end suite drives Chromium from the nix store — `PLAYWRIGHT_BROWSERS_PATH`
comes from the flake, because Playwright's own browser download produces binaries
that will not run on NixOS. The npm `@playwright/test` version must match
`playwright-driver` in nixpkgs; `$PLAYWRIGHT_VERSION` in the dev shell tells you
which that is.

Each run wipes `/tmp/kanban2-e2e`, builds a scratch repository there, and points
the server at it via `XDG_DATA_HOME` — nothing touches a real board.

### The fake agent

`tests/fake-agent.mjs` stands in for `claude`, selected through
`KANBAN2_AGENT_BIN`. It imitates only what the app couples to: an input box at
the bottom of the screen, a startup window with no box at all where anything
sent is queued out of its reach, bracketed-paste handling that collapses long
pastes, a modal that swallows pastes and reads a bare Enter as "exit", and the
HTTP hooks named in its own `--settings`. That makes worktrees, turn snapshots,
lane transitions, review submission and merge deterministic and free to run.

`tests/modals.spec.mjs` is the regression guard worth knowing about: injection
must verify its own paste landed before sending Enter, because a modal would
otherwise be answered by it. Both it and the JS-boot coverage were checked by
reintroducing the original bugs and confirming the suite goes red.

`pnpm e2e tests/screenshots.spec.mjs` writes `tests/.shots/` for eyeballing the
UI. The dev shell supplies fonts so that rendering is representative; without
them the container has no monospace face at all.
