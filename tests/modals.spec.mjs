import { expect, test } from "./support/fixtures.mjs";
import { SLOW } from "../playwright.config.mjs";

import {
  addCard,
  addProject,
  cardIn,
  comment,
  fileSection,
  openCard,
  turnRefs,
} from "./support/board.mjs";
import { terminalInput, terminalRows } from "./support/dom.mjs";

const TITLE = "Asks first";

let projectUrl;
let cardId;

test.beforeAll(async ({ browser }) => {
  const page = await browser.newPage();
  projectUrl = await addProject(page);
  await addCard(page, projectUrl, { title: TITLE, description: "Tighten the loop.", base: "main" });
  cardId = await cardIn(page, "todo", TITLE).getAttribute("data-card-id");

  // A first turn, so there is a diff to comment on. SLOW: an agent start.
  await cardIn(page, "todo", TITLE).dragTo(page.locator('[data-lane="in_progress"]'));
  await expect.poll(() => turnRefs(cardId).length, { timeout: SLOW }).toBe(1);
  await page.close();
});

/**
 * Nothing reports that a permission was answered, so the state used to sit on
 * the card until the turn ended — minutes of work reported as blocked. The
 * terminal is the only signal, and the fake agent holds the turn open between
 * answering the dialog and `f` so that gap can be inspected rather than raced.
 */
test("a tool permission prompt shows on the card and resumes when answered", async ({ page }) => {
  await openCard(page, cardId);

  // The marker makes the fake agent raise a permission dialog on its next turn.
  await page.locator('label[for="tab-review"]').click();
  await comment(
    page,
    fileSection(page, "main.rs").locator(".line.l-added").first(),
    "[needs-permission] run the formatter",
  );
  await page.getByRole("button", { name: /Send \d+ to agent/ }).click();

  await expect(page.locator("#agent-state")).toContainText("needs you");
  await expect(cardIn(page, "in_review", TITLE)).toBeVisible();

  await page.locator('label[for="tab-agent"]').click();
  const rows = terminalRows(page);
  await expect(rows).toContainText("needs approval");
  const terminal = terminalInput(page);
  await terminal.press("1");

  // The dialog is gone but the turn has not ended, which is where the state used
  // to stay stuck. The card is working again, and in the lane for it.
  await expect(page.locator("#agent-state")).toContainText("working");
  await expect(cardIn(page, "in_progress", TITLE)).toBeVisible();
  expect(turnRefs(cardId)).toHaveLength(1);

  // Only now does the turn end.
  await terminal.press("f");
  await expect.poll(() => turnRefs(cardId).length).toBe(2);
  await expect(page.locator("#agent-state")).toContainText("idle");
  await expect(cardIn(page, "in_review", TITLE)).toBeVisible();
});
