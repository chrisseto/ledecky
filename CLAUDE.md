# CLAUDE.md

`README.md` covers the architecture, the nix/Playwright version pinning, the
`XDG_DATA_HOME` isolation story and the fake agent's contract. Read it first.
What follows is only what is easy to get wrong.

## Fresh worktrees

A new worktree has no `node_modules` or `static/`: the server won't boot and
`pnpm e2e` can't resolve Playwright. Run `pnpm install && pnpm build`. Don't
symlink `node_modules` from another checkout; its lockfile may not match.

## Running tests

```sh
cargo test                        # unit
pnpm e2e                          # end-to-end
pnpm e2e tests/lifecycle.spec.mjs # one file, while iterating on it
pnpm e2e -g "rewritten"           # one test
pnpm e2e:trace                    # retries once with a trace + HTML report
pnpm e2e:shots                    # screenshots.spec, which is documentation
pnpm e2e:perf                     # perf.spec, which counts what a big diff costs
```

Those are three Playwright *projects*, not three ways of running one file:
`chromium` is the ordinary loop and excludes the other two by tag, and each
script is just `--project=<name>`. Running `playwright test` with no project
runs all three, so reach for the scripts.

The whole suite is ~20s on four workers, so run it. Narrow to a file while
iterating on that file, not to save wall clock.

**Don't pipe a run into `head` or `tail`.** Closing the pipe early kills the
reporter mid-run, which prints as `N did not run` with no failure above it and
exits non-zero — a red run that is nothing of the sort. Redirect to a file and
read that instead.

Don't reach for `--only-changed`: it selects test files that changed, or that
import something changed, and the specs do not import Rust — so **editing
`src/` selects nothing** and the run comes back green having executed nothing.
It cannot pay for that footgun at this suite's size.

## Writing end-to-end tests

- **Never add `page.waitForTimeout`.** Wait on something observable: an
  assertion, `expect.poll`, or `pollsOfPath()` from `tests/support/board.mjs`
  when the point is that *nothing* happened. Nothing polls any more, so the
  clock to count against is *changes*: make one, wait for it to land, and assert
  the fragment fetched itself exactly once more. That proves an update arrives
  only when something asked for it; a sleep only proves the number was big
  enough.
- **Server timings are settings, not consts.** A new `thread::sleep` in
  `src/agent/` without a matching `Settings` field is a review failure — see
  `Timings` and `watch_debounce` in `src/config.rs`, and `AGENT_TIMINGS` in
  `playwright.config.mjs` for what the suite sets them to. If you are tempted to
  sleep a test out past a server delay, add the knob instead.
- **Don't assert on the agent's banner line.** It is the first thing the fake
  agent prints and so the first thing any scroll takes away. `worktrees/<id>` in
  the terminal says the same thing and says it about *this* card.
- **The drawer opens on the review tab** as soon as a card has something to
  review. Anything touching the terminal needs `showAgent(page)` first, or it
  measures a hidden element.
- **Don't pass a `timeout` that matches the default.** `expect.timeout` in
  `playwright.config.mjs` covers everything ordinary. Name `SLOW` or
  `PAST_GRACE` from there when a wait is genuinely slower on purpose, and say
  which in a comment. A literal at a call site is a number nobody can revise.
- **Selectors**, in order: `getByRole`/`getByLabel` → an existing `data-*` hook
  (`data-lane`, `data-card-id`, `data-path`, `data-terminal`) → a named helper
  in `tests/support/dom.mjs`. Don't introduce `data-testid`; the classes here
  are hand-written component names and are already stable. Never inline a
  vendor class like `.xterm-rows` at a new call site — it belongs in `dom.mjs`.
- **The suite is parallel and order-free.** Each worker owns a data dir, a
  scratch repo and a server (`tests/support/fixtures.mjs`). Never assume global
  state, and in particular never assume an empty board.
- **Any number of runs can go at once.** Each gets its own `mkdtemp` root, made
  by `global-setup` and passed to the workers in `LEDECKY_TEST_ROOT`. Teardown
  removes it, so set `E2E_DEBUG=1` when you want to read what a run left.

## Writing comments

**Write comments in ASD-STE100 (Simplified Technical English).** Short
sentences, one idea each. Active voice, present tense. No metaphor, no idiom,
no figurative language. One meaning per word — prefer `delete` to `withdraw`,
`show` to `say`, `complete` to `land`. Keep code identifiers as they are.

**Comment on the code as it is, not on how it changed.** A comment that only
makes sense if you remember the previous version — "this used to redraw the
pane", "the button came back dead" — is noise to the next reader. Say what the
code does and why it has to, and leave the history to `git log`.

## Writing queries

**A query returns `rusqlite::Result`, never a default.** `unwrap_or_default()`
on a `SELECT` turns a broken statement into an empty board, an empty diff or a
card with no comments — the page renders, nothing says anything is wrong, and
the bug surfaces as "my data disappeared". Propagate, and let the route decide:
routes returning `Result<_, Status>` log and answer `500`. There is older code
that swallows; don't copy it.

Keep the SQL plain. No `CASE` — branch in Rust and hand each arm its own
statement. No comments inside the string; the reason belongs in the doc comment
or an `NB:` above the call. An `ORDER BY` needs a reason a caller can feel: a
thread reading in the order it was written, a batch reaching the agent that way.
Don't add one to a query whose caller only counts.

## htmx 4 attributes

Two parsing rules that fail silently rather than loudly:

- **Quote any trigger modifier value that contains a space.** HCON ends a bare
  value at the first space, so `from:find textarea` becomes `from: "find"`,
  htmx binds the trigger to no element, and nothing happens at all. Write
  `from:'find textarea'`.
- **An out-of-band selector cannot contain a space,** and resolves against the
  document. Use `hx-swap-oob` when the fragment owns a fixed id, so no sender
  repeats a selector. Use `hx-partial` when the target has no id or is relative
  to the requesting element: a partial resolves `hx-target` against the
  request's source element, so `find`/`closest` work there.

Read `node_modules/htmx.org/dist/htmx.esm.js` rather than htmx 1/2 docs; the
attribute set and the swap pipeline both differ.

## Iterating on Rust only

`LEDECKY_SKIP_ASSETS=1` skips the `pnpm build` in `build.rs` and embeds
`static/` as it stands.
