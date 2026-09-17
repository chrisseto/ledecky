import { expect, test } from "@playwright/test";

import { addCard, addProject, cardIn, comment, openCard, turnRefs } from "./support/board.mjs";

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
  await shot(page, "02-add-project");

  projectUrl = await addProject(page);
  await shot(page, "03-empty-board");

  await addCard(page, projectUrl, {
    title: "Add a build banner",
    description: "Print a banner line when the program starts.",
    base: "main",
  });
  await page.goto(`${projectUrl}/cards/new`);
  await page.getByLabel("Task").fill("Teach the CLI to speak JSON");
  await shot(page, "04-new-card");

  await page.goto(projectUrl);
  cardId = await cardIn(page, "todo", "Add a build banner").getAttribute("data-card-id");
  await shot(page, "05-board");

  await page.getByTitle("Switch project").click();
  await expect(page.locator(".drawer-projects")).toBeVisible();
  await shot(page, "06-projects");

  await page.goto(projectUrl);
  await cardIn(page, "todo", "Add a build banner").dragTo(page.locator('[data-lane="in_progress"]'));
  await expect.poll(() => turnRefs(cardId).length, { timeout: 25_000 }).toBe(1);

  await page.goto(projectUrl);
  await shot(page, "07-board-in-review");

  await openCard(page, cardId);
  await page.locator('label[for="tab-agent"]').click();
  await expect(page.locator(".terminal .xterm-rows")).toContainText("fake-agent");
  await shot(page, "08-agent");

  await page.locator('label[for="tab-review"]').click();
  await shot(page, "09-review");

  await comment(page, page.locator("#review .line.l-added").first(), "Say hello instead.");
  await expect(page.locator("#review .comment")).toBeVisible();
  await shot(page, "10-draft-comment");
});
