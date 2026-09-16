import { expect, test } from "@playwright/test";

// The empty state is a property of a board with nothing on it, and the suite
// shares one server and one database. The numeric filename prefix is what keeps
// this ahead of every spec that adds a project — Playwright walks test files in
// path order, and the config pins `workers: 1` with `fullyParallel: false`.
test("the landing page invites a first project", async ({ page }) => {
  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Projects" })).toBeVisible();
  await expect(page.locator(".empty")).toContainText("No projects yet");
  await expect(page.locator(".project-card")).toHaveCount(0);
  await expect(page.getByRole("link", { name: "Add project" })).toBeVisible();
});
