import { expect, test } from "./support/fixtures.mjs";

import { addCard, addProject, cardIn, lane, moveCard, openCard, pollsOf } from "./support/board.mjs";

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

test("a card with no name of its own stands in its task, clipped to one line", async ({ page }) => {
  await page.goto(`${projectUrl}/cards/new`);
  const body = "The body the agent is given, at length. ".repeat(4).trim();
  await page.getByLabel("Task").fill(`Title line\n\n${body}`);
  await page.getByRole("button", { name: "Create more" }).click();

  // "Create more" leaves the form open on an empty task for the next one.
  await expect(page.locator(".modal-card")).toBeVisible();
  await expect(page.getByLabel("Task")).toHaveValue("");

  await page.goto(projectUrl);
  const label = cardIn(page, "todo", "Title line").locator(".card-open");

  // Nothing is cut on the way in — the whole task is there, and the card is
  // still one line tall because CSS is what does the clipping.
  await expect(label).toContainText(body);
  expect(
    await label.evaluate((el) => {
      const style = getComputedStyle(el);
      return {
        wrap: style.whiteSpace,
        overflow: style.textOverflow,
        clipped: el.scrollWidth > el.clientWidth,
      };
    }),
  ).toEqual({ wrap: "nowrap", overflow: "ellipsis", clipped: true });
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

test("the drawer's width is draggable and sticks", async ({ page }) => {
  await page.goto(projectUrl);
  const cardId = await cardIn(page, "done", "Teach it to whistle").getAttribute("data-card-id");
  await openCard(page, cardId);

  const drawer = page.locator(".drawer-card");
  const width = async () => (await drawer.boundingBox()).width;

  // 7/8 of the 1440px viewport.
  expect(await width()).toBeCloseTo(1260, 0);

  // The drawer is rtl, which puts its resizer in the bottom-left corner; the
  // slide has to land before that corner is where it looks like it is.
  await drawer.evaluate((el) => Promise.all(el.getAnimations().map((a) => a.finished)));
  const grab = { x: 1440 - (await width()) + 8, y: 900 - 8 };

  // Dragging it right narrows the drawer, since the right edge is pinned.
  await page.mouse.move(grab.x, grab.y);
  await page.mouse.down();
  await page.mouse.move(grab.x + 300, grab.y, { steps: 10 });
  await page.mouse.up();

  expect(await width()).toBeCloseTo(960, 0);

  // The width outlived the page, and the fresh drawer carries it without an
  // inline width of its own — so it is coming from :root, not the resizer.
  await openCard(page, cardId);
  expect(await width()).toBeCloseTo(960, 0);
  expect(await drawer.getAttribute("style")).toBeNull();
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

test("the board fetches once per change and not otherwise", async ({ page }) => {
  const refetches = pollsOf(page, projectUrl);
  const streams = [];
  page.on("request", (request) => {
    if (new URL(request.url()).pathname === "/events") streams.push(request.url());
  });

  await page.goto(projectUrl);
  const card = cardIn(page, "todo", "Third card");
  await expect(card).toBeVisible();
  const id = await card.getAttribute("data-card-id");

  // One connection carries every update, and opening it costs a single resync
  // — so a page rendered just before its stream came up cannot be left stale.
  await expect.poll(() => streams.length).toBe(1);
  await expect.poll(() => refetches.length).toBe(1);

  await page.evaluate(() => {
    window.__board = document.querySelector("#board");
  });

  // Each change costs exactly one more fetch, and the awaited round trips
  // between them are real elapsed time — so anything left on a timer would show
  // up here as a count that climbed on its own.
  await moveCard(page, id, "in_review", 0);
  await expect(cardIn(page, "in_review", "Third card")).toBeVisible();
  expect(refetches).toHaveLength(2);

  await moveCard(page, id, "todo", 0);
  await expect(cardIn(page, "todo", "Third card")).toBeVisible();
  expect(refetches).toHaveLength(3);

  expect(streams).toHaveLength(1);
  expect(await page.evaluate(() => window.__board.isConnected)).toBe(true);
});

test("a change made elsewhere arrives unasked", async ({ page }) => {
  await page.goto(projectUrl);
  const id = await cardIn(page, "todo", "Third card").getAttribute("data-card-id");

  // Server-side move; this page is never told about it directly.
  await moveCard(page, id, "in_review", 0);

  await expect(cardIn(page, "in_review", "Third card")).toBeVisible();
});

test("an update leaves the cards it did not change alone", async ({ page }) => {
  await addCard(page, projectUrl, { title: "Neighbour" });
  await page.goto(projectUrl);

  await page.evaluate(() => {
    window.__card = document.querySelector('[data-lane="todo"] .card');
  });
  const untouched = await page.evaluate(() => window.__card.id);

  const id = await cardIn(page, "todo", "Neighbour").getAttribute("data-card-id");
  await moveCard(page, id, "done", 0);
  await expect(cardIn(page, "done", "Neighbour")).toBeVisible();

  // The board really was redrawn; morphing it by id is what saved this node.
  expect(untouched).not.toBe(`card-${id}`);
  expect(await page.evaluate(() => window.__card.isConnected)).toBe(true);
});

test("an update keeps each lane's scroll position", async ({ page }) => {
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

test("a card with nothing to review says so, without an empty picker", async ({ page }) => {
  await addCard(page, projectUrl, { title: "Untouched" });
  await page.goto(projectUrl);
  await cardIn(page, "todo", "Untouched").click();

  // Nothing has run, so the review tab is not the one that leads.
  await page.locator('label[for="tab-review"]').click();

  // A card that never started has no worktree and no turns, so there is no
  // point in history to measure from and the picker stays off the page.
  await expect(page.locator("#review [data-range-menu]")).toBeHidden();
  await expect(page.locator("#diff-lines > .empty")).toContainText("Nothing yet on main");
});

test("a card waiting in To Do can have its task rewritten", async ({ page }) => {
  await addCard(page, projectUrl, {
    title: "Draft errand",
    description: "First attempt.",
    permissions: "acceptEdits",
  });
  const id = await cardIn(page, "todo", "Draft errand").getAttribute("data-card-id");

  await openCard(page, id);
  await page.locator(".drawer-card").getByRole("link", { name: "Edit" }).click();

  // The form opens on the task exactly as it was typed.
  const form = page.locator(".modal-card");
  await expect(form.getByLabel("Task")).toHaveValue("Draft errand\n\nFirst attempt.");
  await expect(form.getByLabel("Base branch")).toHaveValue("main");
  await expect(form.getByLabel("Permissions")).toHaveValue("acceptEdits");

  await form.getByLabel("Task").fill("Rewritten errand\n\nSecond attempt.");
  await form.getByLabel("Base branch").fill("release");
  await form.getByLabel("Permissions").selectOption("plan");
  await form.getByRole("button", { name: "Save" }).click();

  // Saving lands back on the card it edited, which is still unnamed and so
  // still standing in its task.
  await expect(page.locator(".drawer-card h2")).toHaveText("Rewritten errand\n\nSecond attempt.");
  await expect(page.locator(".drawer-card .branch")).toContainText("release");

  await page.goto(projectUrl);
  await expect(cardIn(page, "todo", "Rewritten errand")).toBeVisible();
  await expect(cardIn(page, "todo", "Draft errand")).toHaveCount(0);

  // The whole task is what the form hands back, and what the agent would get.
  await page.goto(`/cards/${id}/edit`);
  await expect(form.getByLabel("Task")).toHaveValue("Rewritten errand\n\nSecond attempt.");
  await expect(form.getByLabel("Permissions")).toHaveValue("plan");
});

test("a card that has left To Do is no longer a draft", async ({ page }) => {
  await page.goto(projectUrl);
  const id = await cardIn(page, "todo", "Rewritten errand").getAttribute("data-card-id");
  await moveCard(page, id, "done", 0);

  await openCard(page, id);
  await expect(page.locator(".drawer-card").getByRole("link", { name: "Edit" })).toHaveCount(0);

  // And not just hidden: the endpoints turn it down too.
  expect((await page.request.get(`/cards/${id}/edit`)).status()).toBe(409);
  const refused = await page.request.post(`/cards/${id}`, {
    form: { task: "Too late", base_branch: "main", permission_mode: "plan", model: "" },
  });
  expect(refused.status()).toBe(409);

  await page.goto(projectUrl);
  await expect(cardIn(page, "done", "Rewritten errand")).toBeVisible();
});

test("the garbage button clears Done and takes its cards off the board", async ({ page }) => {
  await addCard(page, projectUrl, { title: "Finished work" });
  await page.goto(projectUrl);

  const id = await cardIn(page, "todo", "Finished work").getAttribute("data-card-id");
  const path = new URL(projectUrl).pathname;
  const switcher = `/projects?board=${path.split("/").pop()}`;
  const counted = async () => {
    await page.goto(switcher);
    const text = await page.locator(`.project-card[href="${path}"] .count`).innerText();
    return Number(text.split(" ")[0]);
  };

  await moveCard(page, id, "done");
  await page.goto(projectUrl);
  await expect(cardIn(page, "done", "Finished work")).toBeVisible();

  // Earlier tests in this file retire cards too, so the whole lane goes.
  const collecting = await lane(page, "done").locator(".card").count();
  expect(collecting).toBeGreaterThan(1);
  const before = await counted();

  await page.goto(projectUrl);
  page.on("dialog", (dialog) => dialog.accept());
  await page.locator(".lane-done .lane-gc").click();

  await expect(lane(page, "done").locator(".card")).toHaveCount(0);
  await expect(page.locator(".lane-done .lane-gc")).toHaveCount(0);

  // The collected lane is drawn nowhere, so the card is off the board entirely.
  await page.reload();
  await expect(cardIn(page, "done", "Finished work")).toHaveCount(0);
  await expect(page.locator('[data-lane="garbage_collected"]')).toHaveCount(0);

  // ...and out of the project's count, which reads every card, not every column.
  expect(await counted()).toBe(before - collecting);
});

test("a card cannot be moved into the collected lane by hand", async ({ page }) => {
  await addCard(page, projectUrl, { title: "Stays put" });
  await page.goto(projectUrl);
  const id = await cardIn(page, "todo", "Stays put").getAttribute("data-card-id");

  // Collection is the only way in, because it is the only path that also
  // clears the disk; a plain move would hide the card with its worktree intact.
  for (const path of [`/cards/${id}/move`, `/cards/${id}/lane`]) {
    const response = await page.request.post(path, {
      form: { lane: "garbage_collected", index: 0 },
    });
    expect(response.status()).toBe(400);
  }

  await page.reload();
  await expect(cardIn(page, "todo", "Stays put")).toBeVisible();
});
