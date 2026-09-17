import { defineConfig } from "@playwright/test";

import { DATA_HOME } from "./tests/support/paths.mjs";

const PORT = Number(process.env.KANBAN2_TEST_PORT ?? 8771);

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
    baseURL: `http://127.0.0.1:${PORT}`,
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
    url: `http://127.0.0.1:${PORT}/`,
    reuseExistingServer: false,
    timeout: 180_000,
    stdout: "pipe",
    stderr: "pipe",
    env: {
      ROCKET_PORT: String(PORT),
      ROCKET_LOG_LEVEL: "critical",
      // Isolation: the app derives every path it writes from XDG_DATA_HOME, so
      // a test run never touches a real board. This has to be the same value
      // global-setup wipes, or a stale database survives into the next run.
      XDG_DATA_HOME: DATA_HOME,
      KANBAN2_AGENT_BIN: process.env.KANBAN2_AGENT_BIN ?? new URL("tests/fake-agent.mjs", import.meta.url).pathname,
    },
  },
});
