# CLAUDE.md

`README.md` covers the architecture, the nix/Playwright version pinning, the
`XDG_DATA_HOME` isolation story and the fake agent's contract. Read it first.
What follows is only what is easy to get wrong.

## Running tests

```sh
cargo test                        # unit
pnpm e2e                          # end-to-end
pnpm e2e tests/lifecycle.spec.mjs # one file, while iterating on it
pnpm e2e -g "rewritten"           # one test
pnpm e2e:trace                    # retries once with a trace + HTML report
pnpm e2e:shots                    # screenshots.spec, which is documentation
```

The whole suite is ~20s on four workers, so run it. Narrow to a file while
iterating on that file, not to save wall clock.

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

## Iterating on Rust only

`LEDECKY_SKIP_ASSETS=1` skips the `pnpm build` in `build.rs` and embeds
`static/` as it stands.
