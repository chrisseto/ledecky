import { test as base } from "@playwright/test";

import { ROOT } from "./paths.mjs";
import { boot, provision, shutdown } from "./server.mjs";

/**
 * A server per worker, over a data directory and scratch repository of its own.
 *
 * Worker-scoped, so the cost is one boot per worker rather than one per test,
 * and `beforeAll` + module-level card ids keep working inside a file. The
 * specs address it through `baseURL`, which is test-scoped because Playwright
 * refuses to widen the scope of a fixture it already defines — a test-scoped
 * fixture reading a worker-scoped one is the supported direction.
 */
export const test = base.extend({
  server: [
    async ({}, use) => {
      provision(ROOT);
      const server = await boot({ root: ROOT });
      await use(server);
      await shutdown(server);
    },
    { scope: "worker" },
  ],

  baseURL: async ({ server }, use) => {
    await use(server.url);
  },
});

export { expect } from "@playwright/test";
