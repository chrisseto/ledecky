// Copies the icons we actually use out of lucide-static into static/icons/.
// The server inlines them at render time, so unpoly fragment swaps need no JS re-init.
import { mkdir, copyFile, readdir, rm } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..", "..");
const SRC = join(ROOT, "node_modules", "lucide-static", "icons");
const OUT = join(ROOT, "static", "icons");

const ICONS = [
  "alert-triangle", "arrow-left", "check", "chevron-down", "chevron-right",
  "circle", "circle-dot", "file-diff", "folder", "folder-git-2", "git-branch",
  "git-commit-horizontal", "git-merge", "loader", "message-square", "play",
  "plus", "search", "settings", "square", "terminal", "trash-2", "x",
];

await rm(OUT, { recursive: true, force: true });
await mkdir(OUT, { recursive: true });

const available = new Set(await readdir(SRC));
const missing = ICONS.filter((n) => !available.has(`${n}.svg`));
if (missing.length) {
  console.error(`lucide-static is missing: ${missing.join(", ")}`);
  process.exit(1);
}

await Promise.all(
  ICONS.map((n) => copyFile(join(SRC, `${n}.svg`), join(OUT, `${n}.svg`))),
);
console.log(`icons: ${ICONS.length} copied`);
