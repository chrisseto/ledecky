import { execFileSync, spawn } from "node:child_process";
import { mkdirSync, readlinkSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { expect, test } from "@playwright/test";

// This spec runs its own server so it can kill it outright, which would take the
// shared one down with it.
const PROJECT = join(dirname(fileURLToPath(import.meta.url)), "..");
const ROOT = "/tmp/ledecky-orphans";

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
 * about — running, and this spec would pass with the orphan it is meant to
 * catch still alive.
 */
function build() {
  execFileSync("cargo", ["build", "--quiet"], { cwd: PROJECT, stdio: "inherit" });
  const target = process.env.CARGO_TARGET_DIR ?? join(PROJECT, "target");
  return join(target, "debug", "ledecky");
}

/** Starts a server on a port of its own choosing and reports where it landed. */
async function boot({ agentBin }) {
  const server = spawn(build(), {
    cwd: PROJECT,
    env: {
      ...process.env,
      ROCKET_PORT: "0",
      ROCKET_LOG_LEVEL: "critical",
      XDG_DATA_HOME: join(ROOT, "data"),
      LEDECKY_AGENT_BIN: agentBin,
    },
    stdio: ["ignore", "pipe", "inherit"],
  });

  const base = await new Promise((resolve, reject) => {
    let out = "";
    server.stdout.setEncoding("utf8");
    server.stdout.on("data", (chunk) => {
      out += chunk;
      const url = out.match(/listening on (\S+)/)?.[1];
      if (url) resolve(url);
    });
    // Nothing else says where the server is, so a death here is terminal.
    server.on("exit", (code) => reject(new Error(`the server exited with ${code}`)));
  });

  return { server, base };
}

async function startCard(request, base) {
  await request.post(`${base}/projects`, { form: { path: join(ROOT, "repo") } });
  await request.post(`${base}/projects/1/cards`, {
    form: {
      task: "orphan probe\n\nSit still.",
      base_branch: "main",
      permission_mode: "acceptEdits",
      model: "",
    },
  });
  await request.post(`${base}/cards/1/move`, { form: { lane: "in_progress", index: 0 } });
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
  git(repo, "config", "user.email", "e2e@ledecky.test");
  git(repo, "config", "user.name", "ledecky e2e");
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

  let base;
  ({ server, base } = await boot({ agentBin: join(PROJECT, "tests/fake-agent.mjs") }));
  await startCard(request, base);

  const worktrees = join(ROOT, "data/ledecky/worktrees");
  await expect.poll(() => agentPid(worktrees), { timeout: 20_000 }).toBeTruthy();
  const pid = Number(agentPid(worktrees));

  process.kill(server.pid, "SIGKILL");

  await expect.poll(() => alive(pid), { timeout: 15_000 }).toBe(false);
});

