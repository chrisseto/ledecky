import { expect, test } from "./support/fixtures.mjs";
import { PAST_GRACE, SLOW } from "../playwright.config.mjs";

import {
  addCard,
  addProject,
  cardIn,
  comment,
  fileSection,
  openAgent,
  openCard,
  pollsOfPath,
  turnRefs,
} from "./support/board.mjs";
import { terminalInput, terminalRows } from "./support/dom.mjs";

test.describe.configure({ mode: "serial" });

const TITLE = "Runs unattended";
const TASK = "Tighten the loop.";

let projectUrl;
let cardId;

test.beforeAll(async ({ browser }) => {
  const page = await browser.newPage();
  projectUrl = await addProject(page);
  await addCard(page, projectUrl, {
    title: TITLE,
    description: TASK,
    base: "main",
    permissions: "bypassPermissions",
  });
  cardId = await cardIn(page, "todo", TITLE).getAttribute("data-card-id");
  await page.close();
});

/**
 * A modal owns the keyboard and discards pasted text; a bare Enter answers the
 * modal instead. The bypass-permissions dialog defaults to "No, exit", so an
 * unverified injection used to quietly kill the agent on startup.
 */
test("a consent dialog holds the opening prompt instead of being answered by it", async ({ page }) => {
  await page.goto(projectUrl);
  await cardIn(page, "todo", TITLE).dragTo(page.locator('[data-lane="in_progress"]'));

  await openAgent(page, cardId);
  const rows = terminalRows(page);
  await expect(rows).toContainText("Bypass Permissions mode");

  // The card says it is blocked rather than pretending to work, and steps into
  // In Review so the board says so too.
  await expect(page.locator("#agent-state")).toContainText("needs you");
  await expect(cardIn(page, "in_review", TITLE)).toBeVisible();

  // Give the server real chances to misbehave rather than a wall-clock number
  // big enough to hope it had some: every tick here is a delivery retry that
  // could have answered the dialog.
  const ticks = pollsOfPath(page, `/cards/${cardId}/state`);
  // Past the dialog grace, so a server that had given up would have shown it.
  await expect.poll(() => ticks.length, { timeout: PAST_GRACE }).toBeGreaterThan(14);

  await expect(rows).toContainText("Bypass Permissions mode");
  await expect(rows).not.toContainText("exiting");
  expect(turnRefs(cardId)).toHaveLength(0);
});

test("answering the dialog in the terminal releases the queued prompt", async ({ page }) => {
  await openAgent(page, cardId);
  const rows = terminalRows(page);
  await expect(rows).toContainText("Bypass Permissions mode");

  // Typing goes straight down the websocket, the same as a real keystroke.
  await terminalInput(page).press("2");
  await expect(rows).toContainText("bypass permissions accepted", { timeout: SLOW });

  // The prompt the server has been holding is now delivered on its own.
  await expect.poll(() => turnRefs(cardId).length).toBe(1);
  await expect(page.locator("#agent-state")).toContainText("idle");

  await page.goto(projectUrl);
  await expect(cardIn(page, "in_review", TITLE)).toBeVisible();
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
