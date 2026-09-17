import { expect, test } from "@playwright/test";

// The empty state is a property of a board with nothing on it, and the suite
// shares one server and one database. The numeric filename prefix is what keeps
// this ahead of every spec that adds a project — Playwright walks test files in
// path order, and the config pins `workers: 1` with `fullyParallel: false`.
test("the landing page invites a first project", async ({ page }) => {
  await page.goto("/");
  await expect(page.locator(".board-empty")).toContainText("No projects yet");
  await expect(page.locator(".lane")).toHaveCount(0);
  await expect(page.getByRole("link", { name: "Add project" })).toBeVisible();
});

test("the add-project form opens over the board", async ({ page }) => {
  await page.goto("/");
  await page.getByRole("link", { name: "Add project" }).click();

  await expect(page.locator(".modal-project")).toBeVisible();
  await expect(page.getByLabel("Repository directory")).toBeVisible();

  // Escape closes it, the same as the scrim behind it.
  await page.keyboard.press("Escape");
  await expect(page.locator(".modal-project")).toHaveCount(0);
});
