# ledecky

A local kanban board for Claude Code agents. Each card that reaches **In
Progress** gets its own detached git worktree and a live `claude` process; the
terminal streams to the browser over a WebSocket, and a GitHub-style review pane
beside it turns review comments back into prompts.

The board is the whole interface. Opening a card, switching project, and both
forms are drawers and modals over it, each with its own URL — so a reload lands
back where you were and the back button closes what is open.

```
To Do  ──drag──▶  In Progress  ──agent idles──▶  In Review  ──merge──▶  Done
                  worktree +     ◀──agent works──  diff +                 agent lands
                  live agent                       comments               commits on base
```

In Progress means an agent is working. Anything else — a finished turn, a
question, a permission prompt — is In Review, which is where you are.

## Running it

```sh
direnv allow          # or: nix develop
pnpm install
cargo run             # http://127.0.0.1:8770
```

The flake supplies node, pnpm, and esbuild. Rust comes from your system
toolchain on purpose, so `cargo` stays whatever you already use.

`build.rs` runs `pnpm build` into `static/`, so the bundle the server reads off
disk cannot be older than the server itself. It rebuilds when `web/` or the
package files change, and when `static/` has gone missing — git ignores it, so
nothing else would put it back. `LEDECKY_SKIP_ASSETS=1` leaves it alone, for a
build with no node toolchain to hand.

`pnpm watch` rebuilds assets on change. Templates reload without a restart.

## How it works

**Worktrees.** Entering In Progress runs `git worktree add --detach` under
`$XDG_DATA_HOME/ledecky/worktrees/<card>` and records the starting commit as
`refs/ledecky/<card>/base`.

**Turns.** Claude Code HTTP hooks, passed per-session via `--settings`, report
each `Stop`. The server then commits the *working tree* as
`refs/ledecky/<card>/turn-<n>` — the agent does not have to commit for a turn to
be captured. Staging happens in a scratch `GIT_INDEX_FILE`, so the worktree's
own index is never disturbed.

**Ranges.** The diff is always measured from some *anchor* — the live worktree,
one of the agent's own commits, a turn, or where the card started — and a toggle
beside the picker says whether to read *just* that point or everything *since*
it. So `base..worktree` is everything the card has done, `turn-(N-1)..turn-N` is
one round, `sha^..sha` is one commit. The picker lists the anchors newest first,
turns and commits interleaved by time, each tagged with its own colour.

Anything live ends at the worktree rather than at the last turn, and that is the
point: work shows up while the agent is still doing it, committed or not, rather
than only once a `Stop` has captured it. The worktree is staged into a scratch
index and written out with `git write-tree`, so it has an object id the diff can
use like any other revision — a *tree*, deliberately not a commit, because a
commit would carry a timestamp and so change on every read, defeating both the
diff cache and the pane's ETag. `refs/ledecky/<card>/working` holds it so `gc`
cannot prune it mid-read, and it goes away with the worktree.

The pane polls, so a range that ends at the worktree keeps up on its own. It
holds still while a comment is open or the picker is down, and an unchanged diff
is answered `304` and never swapped.

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

Every file in the range is stacked on the page, with the tree beside it jumping
to one. The diff opens with three lines of context; *Expand N lines above/below*
takes a bite out of a gap and *Expand whole file* opens all of them. What is
open lives in the pane's query string, so nothing about it is server state — and
it is keyed by path rather than by position in the diff, so a file appearing
upstream mid-poll cannot slide it onto a different one. Ticking *Viewed* folds a
file away, and that much is remembered per card.

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

The same screen answers a question the hooks cannot. `Notification` says a dialog
is coming — a tool permission, a question, a plan to approve, an MCP server
asking for input — which is why the card says `needs you` rather than naming one
of them. It is matched down to those types: `idle_prompt` is the same event and
would light up every idle card a minute after it went quiet. `PermissionRequest`
would cover less and answer from inside the permission flow, where a slow reply
stalls the turn.

Nothing at all reports the answer, though. So the pty reader watches for the
dialog leaving the screen, and that is what puts the card back to work; a hook
that never paints one is given up on after five seconds.

[delta]: https://github.com/dandavison/delta

**Polling.** The board and the agent-state chip poll themselves through unpoly.
Every template response carries an `ETag` over its own rendered bytes, so a poll
that finds nothing new is answered `304` and unpoly skips the update entirely —
the board is not re-rendered, and hover, selection and per-lane scroll survive.
The board's etag is rendered into `up-etag` so even the first poll is
conditional. When something has changed, `up-keep` on each card means only the
cards that actually differ are replaced.

**Merge.** Available in In Review. The server asks the agent to land its commits
on the base branch and never rewrites branches itself. On the next turn it
checks that the branch moved *and* that its tree matches the latest snapshot
before marking the card Done and pruning the worktree. Turn refs are kept; the
`working` ref goes with the worktree it described.

## Configuration

Settings live in `Rocket.toml` beside Rocket's own and are read from the same
figment, so any of them can be overridden per-run with a `LEDECKY_` environment
variable:

| Key | Default | What it does |
| --- | --- | --- |
| `app_slug` | `ledecky` | Names the data directory and the `refs/<slug>/` namespace |
| `data_dir` | `/<slug>` | Database, worktrees, per-card scratch |
| `agent_bin` | `claude` | The executable spawned for an agent |
| `poll_interval` | `4000` | How often a polled fragment re-checks the server, in ms |

The port is fixed in `Rocket.toml` because hook URLs have to be stable.

Per-card permission mode and model are set on the new-card form, whose one Task
field doubles as the card's title: the first line names the card, the whole
thing is what the agent is told. `bypassPermissions` shows a one-time consent
dialog in the terminal — answer it there; the card reports `needs you` and waits
in In Review until you do.

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

## Migrating an existing board

The slug names the data directory, the database file, and the ref namespace,
and the database stores absolute worktree paths and literal ref names — so a
board created under an older slug needs more than a rename of the directory.
`scripts/migrate-data-dir.sh` moves the directory, rewrites those rows, renames
`refs/<old>/**` in every registered project, and repairs the worktrees:

```sh
DRY_RUN=1 scripts/migrate-data-dir.sh   # print the plan
scripts/migrate-data-dir.sh             # then, with the server stopped
```

## Cleaning up

```sh
rm -rf ~/.local/share/ledecky
git -C <project> worktree prune
git -C <project> for-each-ref --format='%(refname)' 'refs/ledecky/**' |
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

Each run wipes `/tmp/ledecky-e2e`, builds a scratch repository there, and points
the server at it via `XDG_DATA_HOME` — nothing touches a real board.

### The fake agent

`tests/fake-agent.mjs` stands in for `claude`, selected through
`LEDECKY_AGENT_BIN`. It imitates only what the app couples to: an input box at
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
