import { execFileSync } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { DATA_HOME, REPO, ROOT } from "./support/paths.mjs";

const PROJECT = join(dirname(fileURLToPath(import.meta.url)), "..");

const git = (cwd, ...args) =>
  execFileSync("git", ["-C", cwd, ...args], { encoding: "utf8" }).trim();

export default function globalSetup() {
  // The bundle is a build artifact the server reads off disk, so it has to be
  // current before the server boots.
  execFileSync("pnpm", ["build"], { cwd: PROJECT, stdio: "inherit" });

  rmSync(ROOT, { recursive: true, force: true });
  mkdirSync(DATA_HOME, { recursive: true });
  mkdirSync(REPO, { recursive: true });

  git(REPO, "init", "-q", "-b", "main");
  git(REPO, "config", "user.email", "e2e@kanban2.test");
  git(REPO, "config", "user.name", "kanban2 e2e");
  // The fake agent edits this file; keeping it small keeps diff assertions legible.
  // Long enough that a 3-line context window does not already show the whole
  // file, so widening it is observable.
  const filler = Array.from({ length: 24 }, (_, i) => `fn spare_${i}() -> u32 { ${i} }`);
  writeFileSync(
    join(REPO, "main.rs"),
    `${filler.join("\n")}\n\nfn main() {\n    println!("hi");\n}\n`,
  );
  writeFileSync(join(REPO, "README.md"), "# scratch\n");
  git(REPO, "add", "-A");
  git(REPO, "commit", "-qm", "init");

  // A second branch so the base-branch picker has something to choose between.
  git(REPO, "branch", "release");
}
