import { expect, test } from "@playwright/test";

import { addCard, addProject, cardIn, lane, moveCard, pollsOf } from "./support/board.mjs";

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

test("a poll that finds nothing new leaves the board alone", async ({ page }) => {
  const polls = pollsOf(page, projectUrl);

  await page.goto(projectUrl);
  await expect(cardIn(page, "todo", "Third card")).toBeVisible();

  // Server-rendered, so even the first poll carries an If-None-Match.
  await expect(page.locator("#board")).toHaveAttribute("up-etag", /^".+"$/);

  await page.evaluate(() => {
    window.__board = document.querySelector("#board");
    window.__card = document.querySelector("#board .card");
  });

  // Asserting the count too, so the test cannot pass by polling never firing.
  await expect.poll(() => polls.length).toBeGreaterThanOrEqual(3);
  expect(polls).toEqual(polls.map(() => 304));

  const kept = await page.evaluate(() => window.__board.isConnected && window.__card.isConnected);
  expect(kept).toBe(true);
});

test("a poll picks up a change made elsewhere", async ({ page }) => {
  await page.goto(projectUrl);
  const id = await cardIn(page, "todo", "Third card").getAttribute("data-card-id");

  // Server-side move; this page is never told about it directly.
  await moveCard(page, id, "in_review", 0);

  await expect(cardIn(page, "in_review", "Third card")).toBeVisible();
});

test("a swap leaves the cards it did not change alone", async ({ page }) => {
  await addCard(page, projectUrl, { title: "Neighbour" });
  await page.goto(projectUrl);

  await page.evaluate(() => {
    window.__card = document.querySelector('[data-lane="todo"] .card');
  });
  const untouched = await page.evaluate(() => window.__card.id);

  const id = await cardIn(page, "todo", "Neighbour").getAttribute("data-card-id");
  await moveCard(page, id, "done", 0);
  await expect(cardIn(page, "done", "Neighbour")).toBeVisible();

  // The board really was replaced; `up-keep` is what saved this node.
  expect(untouched).not.toBe(`card-${id}`);
  expect(await page.evaluate(() => window.__card.isConnected)).toBe(true);
});

test("a swap keeps each lane's scroll position", async ({ page }) => {
  await page.goto(projectUrl);

  // Shrink the lane rather than seeding filler cards — every spec file shares
  // one project. A style tag also survives the swap; an inline style would not.
  await page.addStyleTag({ content: "#lane-todo { min-height: 0; max-height: 30px; }" });

  // Set and read in one round trip: a tick landing between them would reset it.
  const cards = lane(page, "todo");
  const before = await cards.evaluate((el) => {
    el.scrollTop = 60;
    return el.scrollTop;
  });
  expect(before).toBeGreaterThan(0);

  // Change a different lane, so this one's own content is untouched.
  const id = await cardIn(page, "done", "Neighbour").getAttribute("data-card-id");
  await moveCard(page, id, "in_review", 0);
  await expect(cardIn(page, "in_review", "Neighbour")).toBeVisible();

  expect(await cards.evaluate((el) => el.scrollTop)).toBe(before);
});

test("a card with nothing to review still offers the range picker", async ({ page }) => {
  await addCard(page, projectUrl, { title: "Untouched" });
  await page.goto(projectUrl);
  await cardIn(page, "todo", "Untouched").click();

  // Nothing has run, so the review tab is not the one that leads.
  await page.locator('label[for="tab-review"]').click();

  // The picker is always on the page; with no turns there is nothing to pick.
  const picker = page.locator("#review [data-scope-select]");
  await expect(picker).toBeDisabled();
  await expect(picker.locator("option")).toHaveText([/All changes/]);
  await expect(page.locator("#diff-lines > .empty")).toContainText("Nothing yet on main");
});
