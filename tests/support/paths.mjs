import { join } from "node:path";

/**
 * Everything this run writes lives under here.
 *
 * `global-setup` makes the directory — one `mkdtemp` per run — and hands it on
 * through the environment. NB: it has to be made once and passed, not derived:
 * under `nix develop` `TMPDIR` is per-invocation, so a worker computing its own
 * would land somewhere the server it talks to has never heard of.
 */
export const BASE = process.env.LEDECKY_TEST_ROOT;

if (!BASE) {
  throw new Error(
    "LEDECKY_TEST_ROOT is unset — `tests/global-setup.mjs` sets it, so this module is only good inside a Playwright run",
  );
}

/**
 * This worker's slice of it.
 *
 * Playwright sets `TEST_PARALLEL_INDEX` in the worker process before it loads
 * any test file, and a worker restarted after a crash keeps its slot — so the
 * constants below are already per-worker by the time anything imports them, and
 * the helpers that close over them need no notion of which worker they are on.
 */
export const ROOT = join(BASE, `w${process.env.TEST_PARALLEL_INDEX ?? "0"}`);

/** The app derives every path it writes from this. */
export const DATA_HOME = join(ROOT, "data");

/** The scratch git repository the board points at. */
export const REPO = join(ROOT, "repo");
