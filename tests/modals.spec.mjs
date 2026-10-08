import { expect, test } from "./support/fixtures.mjs";
import { SLOW } from "../playwright.config.mjs";

import {
  addCard,
  draftBox,
  addProject,
  cardIn,
  comment,
  fileSection,
  openCard,
  showTab,
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
 *
 * The dialog is also the one thing that can turn a send down, which is why the
 * refused batch is checked here rather than on a card of its own: this is the
 * only place a dialog is up with a diff to comment on behind it.
 */
test("a tool permission prompt shows on the card and resumes when answered", async ({ page }) => {
  await openCard(page, cardId);

  // The marker makes the fake agent raise a permission dialog on its next turn.
  await showTab(page, "review");
  await comment(
    page,
    fileSection(page, "main.rs").locator(".line.l-added").first(),
    "[needs-permission] run the formatter",
  );
  await page.getByRole("button", { name: /Send \d+ to agent/ }).click();

  await expect(page.locator("#agent-state")).toContainText("needs you");
  await expect(cardIn(page, "in_review", TITLE)).toBeVisible();

  // The dialog owns the keyboard: there is no input box to paste a review into,
  // and the submit key behind one would answer the dialog instead — on the
  // startup prompts, with "No, exit". So the send is turned down and the batch
  // stays where it was written, rather than being marked delivered.
  const line = fileSection(page, "main.rs").locator(".line.l-added").first();
  await comment(page, line, "And rename it while you are in there.");
  await expect(page.locator("#review .batch-label")).toContainText("1 comment pending");

  const [answer] = await Promise.all([
    page.waitForResponse((res) => res.url().endsWith(`/cards/${cardId}/review`)),
    page.getByRole("button", { name: /Send \d+ to agent/ }).click(),
  ]);
  expect(answer.status()).toBe(409);

  // The pane comes back saying so, with the batch still under it. A bare 409 is
  // swapped into the target like any other answer, which replaced both with an
  // error page.
  await expect(page.locator("#review .review-note")).toContainText("waiting on you");
  await expect(page.locator("#review .batch-label")).toContainText("1 comment pending");
  await expect(draftBox(line)).toHaveValue("And rename it while you are in there.");
  expect(turnRefs(cardId)).toHaveLength(1);

  // Discarded rather than left to land on the next paragraph's send: a batch
  // arriving once the dialog is gone is `lifecycle.spec`'s to prove, and an
  // extra prompt here would be an extra turn under the assertions below.
  await page.getByRole("button", { name: "Discard" }).click();
  await expect(page.locator("#review .batch-label")).toContainText("Click a line to comment");

  await showTab(page, "agent");
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
