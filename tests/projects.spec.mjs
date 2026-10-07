import { expect, test } from "./support/fixtures.mjs";

import { REPO, ROOT } from "./support/paths.mjs";

test("the directory field completes paths and flags git repositories", async ({ page }) => {
  await page.goto("/projects/new");

  const input = page.getByLabel("Repository directory");
  await input.fill(`${ROOT}/`);

  const completions = page.locator(".completions li");
  // NB: by path, not by text. The fixture lays down `jj-repo` beside `repo`, so
  // a substring match names both, and the entry's own `data-path` is exact.
  const scratch = page.locator(`.completions button[data-path="${REPO}/"]`);
  await expect(scratch).toBeVisible();

  // The scratch repo is a git checkout; the data directory beside it is not.
  await expect(scratch.locator(".repo")).toBeVisible();
  await expect(completions.filter({ hasText: "data" }).locator(".repo")).toHaveCount(0);

  // Clicking a completion drives the input, which re-queries one level down.
  await scratch.click();
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
