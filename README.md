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

`build.rs` runs `pnpm build` into `static/`, which the binary then embeds, so
the bundle a server serves cannot be older than the server itself. It rebuilds
when `web/` or the package files change, and when `static/` has gone missing —
git ignores it, so nothing else would put it back. `LEDECKY_SKIP_ASSETS=1`
leaves it alone, for a build with no node toolchain to hand.

`pnpm watch` rebuilds assets on change, but a running server holds the copy it
was built with: templates reload without a restart, assets need one.

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

The base anchor reads "what this card is based on" rather than where it started,
and the distinction is the one above: an agent that rebases re-roots its
worktree, the base follows it, and `base..worktree` stays the card's own work
instead of the card's work plus everything upstream did meanwhile.

Anything live ends at the worktree rather than at the last turn, and that is the
point: work shows up while the agent is still doing it, committed or not, rather
than only once a `Stop` has captured it. The worktree is staged into a scratch
index and written out with `git write-tree`, so it has an object id the diff can
use like any other revision — a *tree*, deliberately not a commit, because a
commit would carry a timestamp and so change on every read, defeating both the
diff cache and the pane's ETag. `refs/ledecky/<card>/working` holds it so `gc`
cannot prune it mid-read, and it goes away with the worktree.

A range that ends at the worktree keeps up on its own: the watcher notices the
write and the pane redraws by morphing, so a comment being typed keeps its text
and its place rather than having to hold the update off.

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
upstream mid-update cannot slide it onto a different one. Ticking *Viewed* folds a
file away, and that much is remembered per card.

Click any diff line to comment; which line is open lives in the pane's query
string like everything else, so an update redraws the box where it already was.
Clicking away saves it as a draft. *Send N to
agent* formats the batch into one message and pastes it into the agent's
terminal.

**Talking to the agent.** Nothing the server sends is typed at the terminal.
The opening task is a command-line argument, so it is the user's own prompt and
the session names itself from it; the client holds it behind the workspace-trust
dialog and submits it once that is answered. Anything
later — a review batch, a merge request — goes to the session's inbox socket,
whose path it reports through a `SessionStart` hook. That event is the only
place `CLAUDE_CODE_MESSAGING_SOCKET` is exported, and only to a `command`
handler, so the hook runs `ledecky hook session-start <url>` — this same binary,
re-executed, posting the socket back to `/inbox` and exiting. Running ourselves
rather than `curl` keeps the hook free of anything that has to be installed
where the agent runs. It needs claude 2.1.224 or newer.

A message that arrives this way is framed to the agent as coming from another
session rather than from the user — right for relayed review comments, which is
why the opening task does not take the same road. The session reads its inbox
between tool calls, so a dialog holding the keyboard no longer blocks a review
from landing.

The screen still answers one question the hooks cannot. `Notification` says a
dialog is coming — a tool permission, a question, a plan to approve, an MCP server
asking for input — which is why the card says `needs you` rather than naming one
of them. It is matched down to those types: `idle_prompt` is the same event and
would light up every idle card a minute after it went quiet. `PermissionRequest`
would cover less and answer from inside the permission flow, where a slow reply
stalls the turn.

Nothing at all reports the answer, though. So the pty reader watches for the
dialog leaving the screen, and that is what puts the card back to work; a hook
that never paints one is given up on after five seconds.

[delta]: https://github.com/dandavison/delta

**Updates.** Nothing polls. A board holds one `EventSource` open on `/events`,
and every fragment that used to poll — the board, the agent-state chip, the
agent pane, the review pane — listens for a named event on it and fetches itself
when one lands. The events are signals only, so rendering stays in the ordinary
routes and a connected client costs no template work.

Three things produce them: the writes that change a card, the agent-state
changes the hooks report, and a `notify` watch on each live worktree — that last
one being the only change the server has no other way to hear about, since an
agent editing files runs no route and fires no hook. What git ignores is not
watched at all: the watch is a descriptor per directory, so a recursive one over
a worktree an agent has built in spends hundreds against a repo's dozen — enough
for one `node_modules` to exhaust `fs.inotify.max_user_watches` where that is
the common 8192. The set is walked instead, pruned at every ignored directory,
and kept up as directories come and go. A burst that git ignores in its entirety
is dropped rather than restaged anyway, since rules can change under a live
watch — the difference between one `git add -A` and one per reader for as long
as the build runs. Every connection opens with a resync, so a page rendered just
before the stream came up, or one whose tab was backgrounded, cannot be left
showing something stale.

Updates are applied by morphing, so an element keeps its identity: a card that
did not change keeps its live node, each lane keeps its scroll position, and a
comment half-typed into the review pane keeps both its text and its place. The
terminal carries `hx-morph-skip` because xterm builds that subtree on the client
and the server knows nothing about it. Responses still carry an `ETag` over
their own bytes, which now saves the bytes rather than the redraw.

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
| `agent_bin` | `claude` | The executable spawned for an agent. 2.1.224 or newer, for the inbox socket |
| `watch_debounce` | `250` | How long a burst of worktree writes settles before the diff is announced, in ms |
| `head_ttl` | `30000` | How long a staged worktree head stands without the watcher, in ms |
| `gzip` | release builds | Whether to gzip responses |

`Rocket.toml` carries the agent plumbing's real-time waits beside these; the
end-to-end suite shrinks every one of them rather than waiting them out.

The port is fixed in `Rocket.toml` because hook URLs have to be stable.

Per-card permission mode and model are set on the new-card form, whose one Task
field doubles as the card's title: the first line names the card, the whole
thing is what the agent is told. The mode only seeds a card's first session; a
restarted one resumes in whatever mode it was last in, as the client recorded it.

## Running it as a user service

The flake exposes the board as a package and as a home-manager module:

```nix
{
  inputs.ledecky.url = "github:chrisseto/ledecky";

  # …in the home-manager configuration:
  imports = [ inputs.ledecky.homeManagerModules.default ];

  services.ledecky.enable = true;
}
```

## Layout

Code is grouped by domain rather than by kind, so a change usually lands in one
folder:

```
src/project/   project.rs card.rs board.rs lifecycle.rs
src/agent/     agent.rs manager.rs messaging.rs terminal.rs webhooks.rs
src/review/    turn.rs comment.rs scope.rs expand.rs viewed.rs
               diff.rs ansi.rs cache.rs routes.rs
src/           config.rs db.rs git.rs hooks.rs tmpl.rs
```

Within `agent/` the split is mechanism from ownership. `agent.rs` knows about a
pty, a screen and a process, and imports neither the database nor the event bus:
the pump reports what it sees — a dialog leaving the screen, the pty closing —
through a `Watcher` it holds a `Weak` to, rather than calling anything. The
manager on the other end owns the registry of live agents and is the only writer
of a card's `agent_state`.

`project/lifecycle.rs` is the card's arc — giving it a worktree and an agent,
and retiring it once its work lands. Those cross git, the diff cache and the
worktree watcher at once, which is why they sit above the manager rather than
inside it; `teardown`'s other caller deletes a card and involves no agent at all.

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

The garbage button in the Done lane does this for the cards it collects: their
worktrees, scratch directories and `refs/<slug>/<card>/**` go, while the rows
stay behind as the record. To clear a whole board by hand:

```sh
rm -rf ~/.local/share/ledecky
git -C <project> worktree prune
git -C <project> for-each-ref --format='%(refname)' 'refs/ledecky/**' |
  xargs -n1 git -C <project> update-ref -d
```

## Tests

```sh
cargo test     # diff parsing, dialog detection, scope round-tripping
pnpm e2e       # end-to-end, in a real browser
pnpm e2e:ui    # the same, in Playwright's interactive runner
pnpm e2e:trace # the same, retried once with a trace and an HTML report
pnpm e2e:shots # the screenshot pass, which is documentation rather than assertions
```

The suite runs four workers, each with a server, a database and a scratch
repository of its own, so no spec depends on another's state or on file order.
`tests/support/fixtures.mjs` owns that; `tests/support/server.mjs` builds and
starts the binary. The waits the agent plumbing makes in real time are settings
(`startup_timeout`, `dialog_grace` and friends in `Rocket.toml`), which is what
keeps a run in seconds rather than minutes — see `AGENT_TIMINGS` in
`playwright.config.mjs`.

The end-to-end suite drives Chromium from the nix store — `PLAYWRIGHT_BROWSERS_PATH`
comes from the flake, because Playwright's own browser download produces binaries
that will not run on NixOS. The npm `@playwright/test` version must match
`playwright-driver` in nixpkgs; `$PLAYWRIGHT_VERSION` in the dev shell tells you
which that is.

Each run gets a `mkdtemp` directory of its own, builds a scratch repository
under a per-worker subdirectory of it, and points that worker's server there via
`XDG_DATA_HOME` — nothing touches a real board, and any number of runs can go at
once. Teardown removes it again; `E2E_DEBUG=1` keeps it, and `LEDECKY_TEST_ROOT`
puts it somewhere you choose (and leaves it to you to delete).

### The fake agent

`tests/fake-agent.mjs` stands in for `claude`, selected through
`LEDECKY_AGENT_BIN`. It imitates only what the app couples to: an opening task
taken as a positional argument and held while a modal is up, an inbox socket
whose path it reports by running the `SessionStart` command hook out of its own
`--settings` — whatever that command is, which is how the real handshake gets
covered without the stand-in knowing anything about it — a startup window where it has drawn nothing and reported nothing,
an input box at the bottom of the screen, a modal that owns the keyboard and
reads a bare Enter as "exit", and the HTTP hooks named in that same
`--settings`. That makes worktrees, turn snapshots, lane transitions, review
submission and merge deterministic and free to run.

The startup dialogs are covered by unit tests instead, drawn verbatim from the
real client: they do **not** number their options. Numbering them is what hid a
bug where the workspace-trust prompt read as an input box and the card sat in
`starting` with nobody told there was anything to answer.

Two regression guards are worth knowing about.
`a_startup_dialog_leaves_the_card_waiting_on_the_user` in `src/agent/manager.rs`:
a card behind a startup dialog has to report `needs you` and stay untouched.
`tests/death.spec.mjs`: an agent killed without a `SessionEnd` — the stand-in
dies on `k` for this — still has to stop its card, because the pty reaching EOF
is the only notice anyone gets. Both were checked by reintroducing the bug and
confirming the suite goes red.

`pnpm e2e tests/screenshots.spec.mjs` writes `tests/.shots/` for eyeballing the
UI. The dev shell supplies fonts so that rendering is representative; without
them the container has no monospace face at all.
