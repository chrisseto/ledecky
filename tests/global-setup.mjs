import { execFileSync } from "node:child_process";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { PROJECT } from "./support/server.mjs";

export default function globalSetup() {
  // A directory of its own per run, so nothing survives from the last one and
  // two runs — two agents, two worktrees — cannot wipe each other's databases.
  // Set here rather than derived in each process: the workers inherit this
  // environment, and under `nix develop` `TMPDIR` is per-invocation, so each of
  // them working it out alone would disagree about where the data lives.
  //
  // Honoured if already set, which is how you keep a run's state to poke at.
  if (!process.env.LEDECKY_TEST_ROOT) {
    process.env.LEDECKY_TEST_ROOT = mkdtempSync(join(tmpdir(), "ledecky-e2e-"));
    // Ours to remove again; a root handed in from outside is not.
    process.env.LEDECKY_TEST_ROOT_OWNED = "1";
  }

  // Built once, here, rather than by each worker: `build.rs` produces the web
  // bundle as a side effect, so this is also what keeps `static/` current —
  // the property `cargo run` used to provide when it started the server.
  execFileSync("cargo", ["build", "--quiet"], { cwd: PROJECT, stdio: "inherit" });
}
