import { execFileSync, spawn } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { AGENT_TIMINGS } from "../../playwright.config.mjs";

export const PROJECT = join(dirname(fileURLToPath(import.meta.url)), "..", "..");

const git = (cwd, ...args) =>
  execFileSync("git", ["-C", cwd, ...args], { encoding: "utf8" }).trim();

/**
 * The server binary.
 *
 * NB: spawned directly rather than through `cargo run`. Cargo does not pass a
 * signal on to the binary it launched, so a killed `cargo run` leaves the
 * server — and its agents — behind; `orphans.spec` depends on that not
 * happening, and every worker depends on its server actually dying at the end.
 * `globalSetup` builds it once, so this only ever names it.
 */
export const binary = () =>
  join(process.env.CARGO_TARGET_DIR ?? join(PROJECT, "target"), "debug", "ledecky");

/** Wipes `root` and lays down the scratch repository the board points at. */
export function provision(root) {
  const repo = join(root, "repo");

  rmSync(root, { recursive: true, force: true });
  mkdirSync(join(root, "data"), { recursive: true });
  mkdirSync(repo, { recursive: true });

  git(repo, "init", "-q", "-b", "main");
  git(repo, "config", "user.email", "e2e@ledecky.test");
  git(repo, "config", "user.name", "ledecky e2e");
  // The fake agent edits this file; keeping it small keeps diff assertions
  // legible. Long enough that a 3-line context window does not already show the
  // whole file, so widening it is observable.
  const filler = Array.from({ length: 24 }, (_, i) => `fn spare_${i}() -> u32 { ${i} }`);
  writeFileSync(
    join(repo, "main.rs"),
    `${filler.join("\n")}\n\nfn main() {\n    println!("hi");\n}\n`,
  );
  writeFileSync(join(repo, "README.md"), "# scratch\n");
  git(repo, "add", "-A");
  git(repo, "commit", "-qm", "init");

  // A second branch so the base-branch picker has something to choose between.
  git(repo, "branch", "release");
}

/** Starts a server on a port of its own choosing and reports where it landed. */
export async function boot({ root, agentBin } = {}) {
  const server = spawn(binary(), {
    cwd: PROJECT,
    env: {
      ...process.env,
      // A free port, so a run never fights the dev server, another worker, or a
      // leftover of its own for a fixed one.
      ROCKET_PORT: "0",
      ROCKET_LOG_LEVEL: "critical",
      // Isolation: the app derives every path it writes from this, so a test
      // run never touches a real board and workers never touch each other's.
      XDG_DATA_HOME: join(root, "data"),
      LEDECKY_AGENT_BIN:
        agentBin ?? process.env.LEDECKY_AGENT_BIN ?? join(PROJECT, "tests/fake-agent.mjs"),
      ...AGENT_TIMINGS,
    },
    stdio: ["ignore", "pipe", "inherit"],
  });

  const url = await new Promise((resolve, reject) => {
    let out = "";
    server.stdout.setEncoding("utf8");
    server.stdout.on("data", (chunk) => {
      out += chunk;
      const found = out.match(/listening on (\S+)/)?.[1];
      if (found) resolve(found);
    });
    // Nothing else says where the server is, so a death here is terminal.
    server.on("exit", (code) => reject(new Error(`the server exited with ${code}`)));
  });

  return { server, url };
}

/**
 * Stops a server started by `boot`.
 *
 * SIGINT rather than SIGTERM: Rocket's `shutdown.signals` defaults to `ctrl_c`,
 * so a TERM skips the fairing that kills the card's agents on the way out.
 */
export async function shutdown({ server }) {
  if (!server || server.exitCode !== null) return;

  const exited = new Promise((resolve) => server.once("exit", resolve));
  server.kill("SIGINT");

  const gaveUp = new Promise((resolve) => setTimeout(resolve, 2000).unref?.());
  await Promise.race([exited, gaveUp]);
  if (server.exitCode === null) server.kill("SIGKILL");
}
