import { execFileSync, spawn } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { AGENT_TIMINGS } from "../../playwright.config.mjs";

export const PROJECT = join(dirname(fileURLToPath(import.meta.url)), "..", "..");

/// Where a run's jj configuration lives, under its own root.
///
/// NB: derived here rather than taken from `paths.mjs`, which throws unless
/// `LEDECKY_TEST_ROOT` is set — and `global-setup` imports *this* file in order
/// to set it. `paths.mjs` exports the same path for the workers, which load
/// after it exists.
const jjConfig = (root) => join(root, "jj.toml");

const git = (cwd, ...args) =>
  execFileSync("git", ["-C", cwd, ...args], { encoding: "utf8" }).trim();

/** The filler the fake agent edits, and a second file to widen against. */
function seed(repo) {
  // Long enough that a 3-line context window does not already show the whole
  // file, so widening it is observable.
  const filler = Array.from({ length: 24 }, (_, i) => `fn spare_${i}() -> u32 { ${i} }`);
  writeFileSync(
    join(repo, "main.rs"),
    `${filler.join("\n")}\n\nfn main() {\n    println!("hi");\n}\n`,
  );
  writeFileSync(join(repo, "README.md"), "# scratch\n");
}

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
  // The fake agent edits these; keeping them small keeps diff assertions
  // legible.
  seed(repo);
  git(repo, "add", "-A");
  git(repo, "commit", "-qm", "init");

  // A second branch so the base picker has something to choose between.
  git(repo, "branch", "release");

  // A remote, and branches under it. NB: refs written by hand rather than
  // fetched — the picker reads `refs/remotes/`, and nothing here should need a
  // network or a second repository on disk to have something to read.
  git(repo, "remote", "add", "origin", "https://ledecky.test/scratch.git");
  for (const name of ["main", "upstream-only"]) {
    git(repo, "update-ref", `refs/remotes/origin/${name}`, "HEAD");
  }

  // An empty config, so jj never reads the developer's own. Not about the
  // identity — jj warns and carries on without one — but about `git.colocate`,
  // snapshot limits and templates, none of which should decide a run. Written
  // before the first jj call below, which reads it.
  writeFileSync(jjConfig(root), "");

  // NB: the config on every jj call the harness makes, not just the server's.
  // `provision` runs jj too, and one reading the developer's own config would
  // be reading the very thing the empty file above exists to avoid.
  const jj = (cwd, ...args) =>
    execFileSync("jj", args, {
      cwd,
      encoding: "utf8",
      env: { ...process.env, JJ_CONFIG: jjConfig(root) },
    }).trim();

  // And a colocated jj repository, which is the only kind the board offers jj
  // for: `refs/heads` and the object store stay where every git read expects
  // them. `--colocate` spelled out rather than left to the default, since the
  // default is exactly what `git.colocate` overrides.
  const jjRepo = join(root, "jj-repo");
  mkdirSync(jjRepo, { recursive: true });
  git(jjRepo, "init", "-q", "-b", "main");
  git(jjRepo, "config", "user.email", "e2e@ledecky.test");
  git(jjRepo, "config", "user.name", "ledecky e2e");
  seed(jjRepo);
  git(jjRepo, "add", "-A");
  git(jjRepo, "commit", "-qm", "init");
  jj(jjRepo, "git", "init", "--colocate");
}

/** Starts a server on a port of its own choosing and reports where it landed. */
export async function boot({ root, agentBin } = {}) {
  const server = spawn(binary(), {
    cwd: PROJECT,
    env: {
      ...process.env,
      // A free port, so a run never fights the dev server, another worker, or a
      // leftover of its own for a fixed one.
      LEDECKY_PORT: "0",
      LEDECKY_HOOK_PORT: "0",
      // Rocket's liftoff line is the one place the bound port is reported, and
      // it is logged at `info`.
      LEDECKY_LOG_LEVEL: "info",
      LEDECKY_LOG_FORMAT: "pretty",
      LEDECKY_CLI_COLORS: "false",
      // Isolation: the app derives every path it writes from this, so a test
      // run never touches a real board and workers never touch each other's.
      XDG_DATA_HOME: join(root, "data"),
      // Likewise, never read the real config.
      XDG_CONFIG_HOME: join(root, "config"),
      // The server is what spawns `jj`, so this is where that isolation has to
      // land. `provision` writes the file.
      JJ_CONFIG: jjConfig(root),
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
      // NB: still drained once found; a full pipe would block the server.
      if (out === null) return;
      out += chunk;
      const found = out.match(/launched on (\S+)/)?.[1];
      if (found) {
        out = null;
        resolve(found);
      }
    });
    // Nothing else says where the server is, so a death here is terminal.
    server.on("exit", (code) => reject(new Error(`the server exited with ${code}`)));
  });

  return { server, url };
}

/**
 * Stops a server started by `boot`.
 *
 * Either SIGINT or SIGTERM starts Rocket's graceful shutdown, which runs the
 * fairing that kills the card's agents on the way out; a KILL is the fallback.
 */
export async function shutdown({ server }) {
  if (!server || server.exitCode !== null) return;

  const exited = new Promise((resolve) => server.once("exit", resolve));
  server.kill("SIGINT");

  const gaveUp = new Promise((resolve) => setTimeout(resolve, 2000).unref?.());
  await Promise.race([exited, gaveUp]);
  if (server.exitCode === null) server.kill("SIGKILL");
}
