import { defineConfig } from "@playwright/test";

/**
 * How long to allow an assertion that is slower than the default on purpose.
 *
 * `expect.timeout` below covers everything ordinary, so a call site should only
 * name one of these when it is genuinely waiting on something slower — a second
 * agent start, a session resume — and the name should say which. Anything that
 * needs longer than `SLOW` is stuck rather than slow.
 */
export const SLOW = 8_000;

/** The same, for a wait that has to outlast a server-side grace period. */
export const PAST_GRACE = 15_000;

/**
 * The agent plumbing's real-time waits, shrunk for the suite.
 *
 * These are the *server's* waits — how long it holds off before pasting, how
 * long it gives a dialog to paint — so they belong to the server rather than to
 * whatever agent it spawns. The one wait that is the agent's own travels in
 * `agent_env`.
 *
 * Exported because a spec that boots a server of its own has to pass them too,
 * or it pays the production delays on its own agent starts.
 */
export const AGENT_TIMINGS = {
  // The stand-in's own knob, handed to it through the server's `agent_env`
  // rather than left to leak in by process inheritance.
  LEDECKY_AGENT_ENV: '{FAKE_AGENT_BOOT_MS="250"}',
  LEDECKY_READY_DELAY: "150",
  LEDECKY_PASTE_POLL: "25",
  LEDECKY_RESUME_TIMEOUT: "2000",
  // Left near its default: nothing asserts the misconfigured state, it sleeps
  // on a background thread so it costs no wall clock, and shrinking it would
  // invent a new flake where a slow first hook trips it mid-assertion.
  LEDECKY_HOOK_GRACE: "15000",
  // Not shrunk as far as the rest: this one is how long the watcher waits for a
  // dialog to *paint* before concluding none is coming. Cut too fine, the
  // server gives up on a dialog that was merely slow to be read off the pty,
  // and answering it then resumes nothing.
  LEDECKY_DIALOG_GRACE: "2500",
  // How long a burst of worktree writes settles for before the diff is
  // announced as moved. Shrunk so a spec that edits a worktree is not waiting
  // out a debounce meant for an agent writing a whole tree.
  LEDECKY_WATCH_DEBOUNCE: "25",
};

export default defineConfig({
  testDir: "tests",
  // `screenshots.spec` is documentation-by-screenshot rather than assertions, so
  // it is not part of the ordinary loop. `pnpm e2e:shots` runs it.
  grepInvert: process.env.E2E_SHOTS ? undefined : /@shots/,
  outputDir: "tests/.artifacts",
  // Per *file*, not per test: every spec keeps its `mode: "serial"` ordering and
  // its `beforeAll` + module-level card id, while separate files run at once.
  // Each worker has a server, a database and a scratch repo of its own
  // (`tests/support/fixtures.mjs`), so there is nothing left to serialise.
  fullyParallel: false,
  workers: 4,
  retries: 0,
  // Sized for a suite whose server no longer sleeps in real time; a test that
  // needs longer than this is stuck, not slow.
  timeout: 15_000,
  expect: { timeout: 5_000 },
  // The fast loop is the common case; `pnpm e2e:trace` is the one that wants an
  // artifact to open afterwards.
  reporter: process.env.E2E_DEBUG
    ? [["list"], ["html", { open: "never", outputFolder: "tests/.report" }]]
    : [["list"]],

  use: {
    // baseURL comes from the worker's own server, via the fixture.
    // NB: not `retain-on-failure`, which records every test and throws the
    // traces away on a green run. With `retries: 0` this records nothing until
    // `pnpm e2e:trace` asks for a retry.
    trace: "on-first-retry",
    screenshot: "only-on-failure",
    viewport: { width: 1440, height: 900 },
  },

  projects: [{ name: "chromium", use: { browserName: "chromium" } }],

  // `global-setup` makes the run's test root and builds the binary once; the
  // servers themselves are per-worker fixtures, and teardown takes the root
  // away again unless `E2E_DEBUG` wants it kept.
  globalSetup: "./tests/global-setup.mjs",
  globalTeardown: "./tests/global-teardown.mjs",
});
