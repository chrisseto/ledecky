import { join } from "node:path";

/**
 * Everything a test run writes lives under here and is wiped on each start.
 *
 * NB: deliberately not `os.tmpdir()` — under `nix develop` that points at a
 * per-invocation directory, so the server and the specs would disagree about
 * where the data lives, and stale databases would survive between runs.
 */
export const ROOT = process.env.KANBAN2_TEST_ROOT ?? "/tmp/kanban2-e2e";

/** The app derives every path it writes from this. */
export const DATA_HOME = join(ROOT, "data");

/** The scratch git repository the board points at. */
export const REPO = join(ROOT, "repo");
