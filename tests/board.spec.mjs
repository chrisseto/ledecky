import { expect, test } from "@playwright/test";

import { addCard, addProject, cardIn, lane } from "./support/board.mjs";

test.describe.configure({ mode: "serial" });

let projectUrl;

test.beforeAll(async ({ browser }) => {
  const page = await browser.newPage();
  projectUrl = await addProject(page);
  await page.close();
});

test("a new card offers the repository's branches and lands in To Do", async ({ page }) => {
  await page.goto(`${projectUrl}/cards/new`);

  // Branches come from the repository, with the checked-out one first.
  const branches = page.getByLabel("Base branch");
  await expect(branches.locator("option")).toHaveText(["main", "release"]);

  await expect(page.getByLabel("Permissions")).toHaveValue("acceptEdits");

  await addCard(page, projectUrl, {
    title: "Teach it to whistle",
    description: "Whistle on startup.",
  });

  await expect(cardIn(page, "todo", "Teach it to whistle")).toBeVisible();
  await expect(lane(page, "todo").locator(".card")).toHaveCount(1);
  // The card shows which branch it will be based on.
  await expect(cardIn(page, "todo", "Teach it to whistle")).toContainText("main");
});

test("a card can be dragged between lanes and the move sticks", async ({ page }) => {
  await page.goto(projectUrl);

  const card = cardIn(page, "todo", "Teach it to whistle");
  await card.dragTo(lane(page, "in_review"));

  // The board reloads itself after the move request settles.
  await expect(cardIn(page, "in_review", "Teach it to whistle")).toBeVisible();
  await expect(lane(page, "todo").locator(".card")).toHaveCount(0);

  // Survives a reload, so the server recorded it rather than the DOM just moving.
  await page.reload();
  await expect(cardIn(page, "in_review", "Teach it to whistle")).toBeVisible();
});

test("cards keep their order within a lane", async ({ page }) => {
  await addCard(page, projectUrl, { title: "Second card" });
  await addCard(page, projectUrl, { title: "Third card" });

  await page.goto(projectUrl);
  await expect(lane(page, "todo").locator(".card-title")).toHaveText(["Second card", "Third card"]);

  await cardIn(page, "todo", "Third card").dragTo(cardIn(page, "todo", "Second card"));
  await expect(lane(page, "todo").locator(".card-title")).toHaveText(["Third card", "Second card"]);

  await page.reload();
  await expect(lane(page, "todo").locator(".card-title")).toHaveText(["Third card", "Second card"]);
});

test("deleting a card removes it from the board", async ({ page }) => {
  await page.goto(projectUrl);
  const before = await lane(page, "todo").locator(".card").count();

  const id = await cardIn(page, "todo", "Second card").getAttribute("data-card-id");
  const response = await page.request.post(`/cards/${id}/delete`);
  expect(response.ok()).toBeTruthy();

  await page.reload();
  await expect(lane(page, "todo").locator(".card")).toHaveCount(before - 1);
  await expect(cardIn(page, "todo", "Second card")).toHaveCount(0);
});
