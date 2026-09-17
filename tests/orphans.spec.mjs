import { execFileSync, spawn } from "node:child_process";
import { mkdirSync, readlinkSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { expect, test } from "@playwright/test";

// This spec runs its own server so it can kill it outright, which would take the
// shared one down with it.
const PROJECT = join(dirname(fileURLToPath(import.meta.url)), "..");
const ROOT = "/tmp/kanban2-orphans";
const PORT = 8781;
const BASE = `http://127.0.0.1:${PORT}`;

const git = (cwd, ...args) =>
  execFileSync("git", ["-C", cwd, ...args], { encoding: "utf8" }).trim();

const alive = (pid) => {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
};

/** The pid of the agent working in a given card's worktree. */
function agentPid(worktreesDir) {
  const pids = execFileSync("pgrep", ["-u", String(process.getuid()), "-f", "fake-agent"], {
    encoding: "utf8",
  })
    .split("\n")
    .filter(Boolean);

  return pids.find((pid) => {
    try {
      return readlinkSync(`/proc/${pid}/cwd`).startsWith(worktreesDir);
    } catch {
      return false;
    }
  });
}

/**
 * The server binary, built in place.
 *
 * NB: this spec spawns it directly rather than through `cargo run`, because it
 * kills the pid it is handed. Cargo does not pass a SIGKILL on to the binary it
 * launched, so killing it would leave the server — and the agent this test is
 * about — running, and that orphan would go on to hold the port against the
 * next run.
 */
function build() {
  execFileSync("cargo", ["build", "--quiet"], { cwd: PROJECT, stdio: "inherit" });
  const target = process.env.CARGO_TARGET_DIR ?? join(PROJECT, "target");
  return join(target, "debug", "kanban2");
}

async function boot({ agentBin }) {
  const server = spawn(build(), {
    cwd: PROJECT,
    env: {
      ...process.env,
      ROCKET_PORT: String(PORT),
      ROCKET_LOG_LEVEL: "critical",
      XDG_DATA_HOME: join(ROOT, "data"),
      KANBAN2_AGENT_BIN: agentBin,
    },
    stdio: "ignore",
  });

  for (let i = 0; i < 120; i++) {
    // A server that died did not come up. Polling on would find whatever else
    // is on the port and test that instead.
    if (server.exitCode !== null) {
      throw new Error(`the server exited with ${server.exitCode}; is ${PORT} taken?`);
    }
    try {
      await fetch(`${BASE}/`);
      return server;
    } catch {
      await new Promise((r) => setTimeout(r, 500));
    }
  }
  throw new Error("server did not come up");
}

async function startCard(request) {
  await request.post(`${BASE}/projects`, { form: { path: join(ROOT, "repo") } });
  await request.post(`${BASE}/projects/1/cards`, {
    form: {
      task: "orphan probe\n\nSit still.",
      base_branch: "main",
      permission_mode: "acceptEdits",
      model: "",
    },
  });
  await request.post(`${BASE}/cards/1/move`, { form: { lane: "in_progress", index: 0 } });
}

/** Held across the test so a failure before the kill still tears it down. */
let server;

test.afterEach(() => {
  // On the handle rather than the pid: the test kills the server itself and the
  // exit is not seen synchronously, so this usually has nothing left to do.
  server?.kill("SIGKILL");
  server = undefined;
});

test.beforeEach(() => {
  rmSync(ROOT, { recursive: true, force: true });
  mkdirSync(join(ROOT, "repo"), { recursive: true });

  const repo = join(ROOT, "repo");
  git(repo, "init", "-q", "-b", "main");
  git(repo, "config", "user.email", "e2e@kanban2.test");
  git(repo, "config", "user.name", "kanban2 e2e");
  writeFileSync(join(repo, "main.rs"), "fn main() {}\n");
  git(repo, "add", "-A");
  git(repo, "commit", "-qm", "init");
});

/**
 * Closing the pty master hangs up the session, so an agent normally dies with
 * the server even when it is killed outright. This locks that in: turning the
 * controlling terminal off, or detaching the child, would leave agents running.
 */
test("SIGKILLing the server takes its agents with it", async ({ request }) => {
  test.slow();

  server = await boot({ agentBin: join(PROJECT, "tests/fake-agent.mjs") });
  await startCard(request);

  const worktrees = join(ROOT, "data/kanban2/worktrees");
  await expect.poll(() => agentPid(worktrees), { timeout: 20_000 }).toBeTruthy();
  const pid = Number(agentPid(worktrees));

  process.kill(server.pid, "SIGKILL");

  await expect.poll(() => alive(pid), { timeout: 15_000 }).toBe(false);
});

