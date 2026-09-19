import { expect, test } from "./support/fixtures.mjs";

import { REPO, ROOT } from "./support/paths.mjs";

test("the directory field completes paths and flags git repositories", async ({ page }) => {
  await page.goto("/projects/new");

  const input = page.getByLabel("Repository directory");
  await input.fill(`${ROOT}/`);

  const completions = page.locator(".completions li");
  await expect(completions.filter({ hasText: "repo" })).toBeVisible();

  // The scratch repo is a git checkout; the data directory beside it is not.
  await expect(completions.filter({ hasText: "repo" }).locator(".repo")).toBeVisible();
  await expect(completions.filter({ hasText: "data" }).locator(".repo")).toHaveCount(0);

  // Clicking a completion drives the input, which re-queries one level down.
  await completions.filter({ hasText: "repo" }).click();
  await expect(input).toHaveValue(`${REPO}/`);
});

test("a directory without a .git is rejected in place", async ({ page }) => {
  await page.goto("/projects/new");
  await page.getByLabel("Repository directory").fill(ROOT);
  await page.getByRole("button", { name: "Add project" }).click();

  await expect(page.locator(".error")).toContainText("is not a git repository");
  // The form keeps what was typed rather than resetting it.
  await expect(page.getByLabel("Repository directory")).toHaveValue(ROOT);
});

test("adding a repository opens its board", async ({ page }) => {
  await page.goto("/projects/new");
  await page.getByLabel("Repository directory").fill(REPO);
  await page.getByRole("button", { name: "Add project" }).click();

  await expect(page).toHaveURL(/\/projects\/\d+$/);
  for (const name of ["To Do", "In Progress", "In Review", "Done"]) {
    await expect(page.getByText(name, { exact: true })).toBeVisible();
  }

  // And it is listed in the switcher, which opens over the board it came from.
  await page.getByTitle("Switch project").click();
  await expect(page.locator(".drawer-projects .project-card.current")).toContainText("repo");
});
