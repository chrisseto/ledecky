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
 * The consent dialog owns the keyboard, and it defaults to "No, exit" — so
 * anything the server sends blind answers it and kills the agent. Nothing is
 * sent now: the task went in on the command line and the client holds it behind
 * the dialog itself.
 *
 * The card still has to notice. Neither this dialog nor the workspace-trust
 * prompt fires a hook, and neither numbers its options, so the only thing that
 * says a dialog is up is the screen — and a `SessionStart` that never arrives.
 */
test("a consent dialog leaves the card waiting on the user, not working", async ({ page }) => {
  await page.goto(projectUrl);
  await cardIn(page, "todo", TITLE).dragTo(page.locator('[data-lane="in_progress"]'));

  await openAgent(page, cardId);
  const rows = terminalRows(page);
  await expect(rows).toContainText("Bypass Permissions mode");

  // The card says it is blocked rather than pretending to work, and steps into
  // In Review so the board says so too. Getting here at all means the server
  // read the unnumbered dialog as a dialog and gave up waiting for the session
  // to report in — the startup timeout has already passed.
  await expect(page.locator("#agent-state")).toContainText("needs you", { timeout: SLOW });
  await expect(cardIn(page, "in_review", TITLE)).toBeVisible();

  // The dialog is still the user's to answer: unanswered, and no turn behind it.
  // There is no retry loop to count against any more — the server writes nothing
  // to the pty at startup — so what is checked is that nothing moved.
  await expect(rows).toContainText("Bypass Permissions mode");
  await expect(rows).not.toContainText("exiting");
  expect(turnRefs(cardId)).toHaveLength(0);

  // The card must not be rescued by the dialog grace either. It elapses on the
  // pty output the watcher runs on, so a fragment fetch is the clock: the card
  // is still waiting once one has gone by past the grace.
  const ticks = pollsOfPath(page, `/cards/${cardId}/state`);
  await page.reload();
  await openAgent(page, cardId);
  await expect.poll(() => ticks.length, { timeout: PAST_GRACE }).toBeGreaterThan(0);
  await expect(page.locator("#agent-state")).toContainText("needs you");
});

test("answering the dialog releases the task the client was holding", async ({ page }) => {
  await openAgent(page, cardId);
  const rows = terminalRows(page);
  await expect(rows).toContainText("Bypass Permissions mode");

  // Typing goes straight down the websocket, the same as a real keystroke.
  await terminalInput(page).press("2");
  await expect(rows).toContainText("bypass permissions accepted", { timeout: SLOW });

  // The task went in on the command line and the client has been holding it
  // behind the dialog; answering releases it, with nothing sent from here.
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
