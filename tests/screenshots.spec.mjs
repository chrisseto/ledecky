import { expect, test } from "./support/fixtures.mjs";

import { ROOT } from "./support/paths.mjs";
import {
  addCard,
  addProject,
  cardIn,
  comment,
  fileSection,
  openCard,
  turnRefs,
  worktreeGit,
} from "./support/board.mjs";
import { terminalRows } from "./support/dom.mjs";

// Not assertions so much as a way to look at the thing. Run with
// `pnpm e2e tests/screenshots.spec.mjs` and open tests/.shots/.
test.describe.configure({ mode: "serial" });

const shot = (page, name) => page.screenshot({ path: `tests/.shots/${name}.png`, fullPage: true });

let projectUrl;
let cardId;

test("capture the whole flow @shots", async ({ page }) => {
  await page.goto("/");
  await shot(page, "01-empty");

  await page.goto("/projects/new");
  await page.getByLabel("Repository directory").fill(`${ROOT}/`);
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
  await expect.poll(() => turnRefs(cardId).length).toBe(1);

  await page.goto(projectUrl);
  await shot(page, "07-board-in-review");

  await openCard(page, cardId);
  await page.locator('label[for="tab-agent"]').click();
  await expect(terminalRows(page)).toContainText("fake-agent");
  await shot(page, "08-agent");

  await page.locator('label[for="tab-review"]').click();
  await shot(page, "09-review");

  // The colour coding only means anything with more than one kind of point in
  // the list, so give it a commit of the agent's own to sit beside the turn.
  worktreeGit(cardId, "-c", "user.email=a@b.c", "-c", "user.name=a", "commit", "-qam", "banner: land it");
  await page.reload();
  await page.locator('label[for="tab-review"]').click();
  await page.locator("[data-range-menu] summary").click();
  await expect(page.locator("[data-range-menu] .menu-item.anchor-commit")).toBeVisible();
  await shot(page, "09b-range-menu");
  await page.locator("[data-range-menu] summary").click();

  await comment(page, fileSection(page, "main.rs").locator(".line.l-added").first(), "Say hello instead.");
  await expect(page.locator("#review .comment")).toBeVisible();
  await shot(page, "10-draft-comment");
});
