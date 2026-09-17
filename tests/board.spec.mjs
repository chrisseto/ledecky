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
  await expect(branches).toHaveValue("main");
  await branches.click();
  await expect(page.locator(".combo-menu [data-branch]")).toHaveText(["main", "release"]);

  // Typing narrows the list without going back to the server.
  await branches.fill("rel");
  await expect(page.locator(".combo-menu [data-branch]:visible")).toHaveText(["release"]);

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

test("the task's first line titles the card and the rest is kept for the agent", async ({ page }) => {
  await page.goto(`${projectUrl}/cards/new`);
  await page.getByLabel("Task").fill("Title line\n\nThe body the agent is given.");
  await page.getByRole("button", { name: "Create more" }).click();

  // "Create more" leaves the form open on an empty task for the next one.
  await expect(page.locator(".modal-card")).toBeVisible();
  await expect(page.getByLabel("Task")).toHaveValue("");

  await page.goto(projectUrl);
  const card = cardIn(page, "todo", "Title line");
  await expect(card).toBeVisible();
  await expect(card).not.toContainText("The body the agent is given.");
});

test("the keyboard creates a card without reaching for the buttons", async ({ page }) => {
  await page.goto(`${projectUrl}/cards/new`);
  await page.getByLabel("Task").fill("Typed and sent");
  await page.keyboard.press("Control+Enter");

  await expect(page).toHaveURL(projectUrl);
  await expect(cardIn(page, "todo", "Typed and sent")).toBeVisible();
});

test("a card can be dragged between lanes and the move sticks", async ({ page }) => {
  await page.goto(projectUrl);

  const card = cardIn(page, "todo", "Teach it to whistle");
  await card.dragTo(lane(page, "in_review"));

  // The board reloads itself after the move request settles.
  await expect(cardIn(page, "in_review", "Teach it to whistle")).toBeVisible();
  await expect(cardIn(page, "todo", "Teach it to whistle")).toHaveCount(0);

  // Survives a reload, so the server recorded it rather than the DOM just moving.
  await page.reload();
  await expect(cardIn(page, "in_review", "Teach it to whistle")).toBeVisible();
});

test("the drawer moves a card without leaving the board", async ({ page }) => {
  await page.goto(projectUrl);
  await cardIn(page, "in_review", "Teach it to whistle").click();

  const drawer = page.locator(".drawer-card");
  await expect(drawer).toBeVisible();
  await expect(drawer.locator(".lane-menu summary")).toContainText("In Review");

  await drawer.locator(".lane-menu summary").click();
  await drawer.getByRole("button", { name: "Done" }).click();

  // The drawer stays open on the card, now reading the lane it was moved to.
  await expect(page.locator(".drawer-card .lane-menu summary")).toContainText("Done");
  await expect(cardIn(page, "done", "Teach it to whistle")).toBeVisible();
});

test("cards keep their order within a lane", async ({ page }) => {
  await addCard(page, projectUrl, { title: "Second card" });
  await addCard(page, projectUrl, { title: "Third card" });

  await page.goto(projectUrl);
  const titles = lane(page, "todo").locator(".card-open");
  await expect(titles).toHaveText([/Title line/, /Typed and sent/, /Second card/, /Third card/]);

  await cardIn(page, "todo", "Third card").dragTo(cardIn(page, "todo", "Second card"));
  await expect(titles).toHaveText([/Title line/, /Typed and sent/, /Third card/, /Second card/]);

  await page.reload();
  await expect(titles).toHaveText([/Title line/, /Typed and sent/, /Third card/, /Second card/]);
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
