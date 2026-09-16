import { expect, test } from "@playwright/test";

import { addCard, addProject, cardIn, turnRefs } from "./support/board.mjs";

// Not assertions so much as a way to look at the thing. Run with
// `pnpm e2e tests/screenshots.spec.mjs` and open tests/.shots/.
test.describe.configure({ mode: "serial" });

const shot = (page, name) => page.screenshot({ path: `tests/.shots/${name}.png`, fullPage: true });

let projectUrl;
let cardId;

test("capture the whole flow", async ({ page }) => {
  test.slow();

  await page.goto("/");
  await shot(page, "01-empty");

  await page.goto("/projects/new");
  await page.getByLabel("Repository directory").fill("/tmp/kanban2-e2e/");
  await expect(page.locator(".completions li").first()).toBeVisible();
  await shot(page, "02-autocomplete");

  projectUrl = await addProject(page);
  await shot(page, "03-empty-board");

  await addCard(page, projectUrl, {
    title: "Add a build banner",
    description: "Print a banner line when the program starts.",
    base: "main",
  });
  await page.goto(`${projectUrl}/cards/new`);
  await shot(page, "04-new-card");

  await page.goto(projectUrl);
  cardId = await cardIn(page, "todo", "Add a build banner").getAttribute("data-card-id");
  await shot(page, "05-board");

  await cardIn(page, "todo", "Add a build banner").dragTo(page.locator('[data-lane="in_progress"]'));
  await expect.poll(() => turnRefs(cardId).length, { timeout: 25_000 }).toBe(1);

  await page.goto(projectUrl);
  await shot(page, "06-board-in-review");

  await page.goto(`/cards/${cardId}`);
  await expect(page.locator(".terminal .xterm-rows")).toContainText("fake-agent");
  await shot(page, "07-card-focus");

  await page.locator("#diff tr.l-added").first().click();
  await page.locator(".comment-form textarea").fill("Say hello instead.");
  await shot(page, "08-comment-form");

  await page.getByRole("button", { name: "Add comment" }).click();
  await expect(page.locator("#diff .comment")).toBeVisible();
  await shot(page, "09-draft-comment");
});
