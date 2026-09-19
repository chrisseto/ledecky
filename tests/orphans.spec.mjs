import { execFileSync } from "node:child_process";
import { existsSync } from "node:fs";
import { join } from "node:path";
import { SLOW } from "../playwright.config.mjs";

import { expect, test } from "@playwright/test";

import { ROOT as WORKER_ROOT } from "./support/paths.mjs";
import { PROJECT, boot as bootServer, provision } from "./support/server.mjs";

// This spec runs a server of its own so it can kill it outright, which would
// take a shared one down with it. Under this worker's root rather than a fixed
// path, so parallel workers cannot wipe each other's copy of it.
const ROOT = join(WORKER_ROOT, "orphans");

const alive = (pid) => {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
};

/**
 * The pid the server recorded for a card's agent.
 *
 * NB: read back out of the server's own database rather than hunted down with
 * `pgrep` and `/proc`. That worked, but it asked the kernel a question this
 * server already knows the answer to — it matched every agent the user was
 * running and leaned on a cwd check to tell them apart, and it only worked on
 * Linux. `agent_pid` is what `sweep_orphans` uses to clean up after a crash, so
 * reading it here tests the column the recovery path depends on.
 */
function agentPid() {
  const db = join(ROOT, "data/ledecky/ledecky.db");
  if (!existsSync(db)) return undefined;

  // Read-only, so this cannot be what unblocks a stuck writer.
  const out = execFileSync(
    "sqlite3",
    [`file:${db}?mode=ro`, "SELECT agent_pid FROM cards WHERE id = 1"],
    { encoding: "utf8" },
  ).trim();

  return out ? Number(out) : undefined;
}

/** This spec's server, on its own root. */
const boot = () => bootServer({ root: ROOT, agentBin: join(PROJECT, "tests/fake-agent.mjs") });

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
  provision(ROOT);
});

/**
 * Closing the pty master hangs up the session, so an agent normally dies with
 * the server even when it is killed outright. This locks that in: turning the
 * controlling terminal off, or detaching the child, would leave agents running.
 */
test("SIGKILLing the server takes its agents with it", async ({ request }) => {
  test.slow();

  let url;
  ({ server, url } = await boot());
  await startCard(request, url);

  await expect.poll(agentPid, { timeout: SLOW }).toBeTruthy();
  const pid = agentPid();

  process.kill(server.pid, "SIGKILL");

  await expect.poll(() => alive(pid)).toBe(false);
});

