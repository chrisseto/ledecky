import { defineConfig } from "@playwright/test";

import { DATA_HOME } from "./tests/support/paths.mjs";

export const POLL_INTERVAL = 250;

export default defineConfig({
  testDir: "tests",
  outputDir: "tests/.artifacts",
  fullyParallel: false, // one server, one sqlite file, real worktrees on disk
  workers: 1,
  retries: 0,
  timeout: 30_000,
  expect: { timeout: 10_000 },
  reporter: process.env.CI ? "list" : [["list"], ["html", { open: "never", outputFolder: "tests/.report" }]],

  use: {
    // The server binds a free port and prints where it landed; `webServer.wait`
    // captures that into the environment before any worker loads this file.
    baseURL: process.env.LEDECKY_URL,
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
    viewport: { width: 1440, height: 900 },
  },

  projects: [{ name: "chromium", use: { browserName: "chromium" } }],

  // `global-setup` prepares a throwaway data dir; the server is started here so
  // Playwright owns its lifetime, and building it builds the assets.
  globalSetup: "./tests/global-setup.mjs",
  webServer: {
    command: "cargo run --quiet",
    wait: { stdout: /listening on (?<ledecky_url>\S+)/ },
    timeout: 180_000,
    stdout: "pipe",
    stderr: "pipe",
    env: {
      // A free port, so a run never fights the dev server or a leftover of its
      // own for a fixed one.
      ROCKET_PORT: "0",
      ROCKET_LOG_LEVEL: "critical",
      // Isolation: the app derives every path it writes from XDG_DATA_HOME, so
      // a test run never touches a real board. This has to be the same value
      // global-setup wipes, or a stale database survives into the next run.
      XDG_DATA_HOME: DATA_HOME,
      LEDECKY_AGENT_BIN: process.env.LEDECKY_AGENT_BIN ?? new URL("tests/fake-agent.mjs", import.meta.url).pathname,
      // Polls are conditional, so a fast interval costs a 304 and nothing else.
      // Tests can then observe several ticks without waiting in real time.
      LEDECKY_POLL_INTERVAL: String(POLL_INTERVAL),
    },
  },
});
