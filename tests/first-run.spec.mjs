import { join } from "node:path";

import { expect, test } from "@playwright/test";

import { ROOT } from "./support/paths.mjs";
import { boot, provision, shutdown } from "./support/server.mjs";

// The empty state is a property of a board with nothing on it, so this spec
// gets a server and data directory of its own rather than relying on running
// before anything that adds a project. Its old numeric filename prefix used to
// buy that ordering; nothing depends on file order any more.
const HOME = join(ROOT, "first-run");

let server;

test.beforeAll(async () => {
  provision(HOME);
  server = await boot({ root: HOME });
});

test.afterAll(async () => {
  await shutdown(server);
});

/** Absolute, because this spec does not use the shared `baseURL`. */
const home = () => server.url;

test("the landing page invites a first project", async ({ page }) => {
  await page.goto(home());
  await expect(page.locator(".board-empty")).toContainText("No projects yet");
  await expect(page.locator(".lane")).toHaveCount(0);
  await expect(page.getByRole("link", { name: "Add project" })).toBeVisible();
});

test("the add-project form opens over the board", async ({ page }) => {
  await page.goto(home());
  await page.getByRole("link", { name: "Add project" }).click();

  await expect(page.locator(".modal-project")).toBeVisible();
  await expect(page.getByLabel("Repository directory")).toBeVisible();

  // Escape closes it, the same as the scrim behind it.
  await page.keyboard.press("Escape");
  await expect(page.locator(".modal-project")).toHaveCount(0);
});
