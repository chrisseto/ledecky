import { mkdirSync, rmSync } from "node:fs";
import { join } from "node:path";
import { SLOW } from "../playwright.config.mjs";

import { expect, test } from "./support/fixtures.mjs";

import {
  addCard,
  draftBox,
  addProject,
  cardIn,
  comment,
  fileSection,
  moveCard,
  openAgent,
  openCard,
  turnRefs,
  showTab,
} from "./support/board.mjs";
import { terminalRows } from "./support/dom.mjs";
import { DATA_HOME } from "./support/paths.mjs";

test.describe.configure({ mode: "serial" });

const TITLE = "Survive a lost session";
const TASK = "Write a line to main.rs.";

let projectUrl;
let cardId;

test.beforeAll(async ({ browser }) => {
  const page = await browser.newPage();
  projectUrl = await addProject(page);
  await addCard(page, projectUrl, { title: TITLE, description: TASK, base: "main" });
  cardId = await cardIn(page, "todo", TITLE).getAttribute("data-card-id");
  await page.close();
});

/**
 * A card records the session it is working in so that restarting picks the
 * conversation back up. The transcript behind it can go away — never written,
 * or pruned — and `--resume` then exits on the spot without drawing anything.
 * The card has to come back on a fresh session rather than be unable to start.
 */
test("a card whose session has gone starts a fresh one", async ({ page }) => {
  await moveCard(page, cardId, "in_progress");
  await expect.poll(() => turnRefs(cardId).length, { timeout: SLOW }).toBe(1);

  await openCard(page, cardId);
  await page.getByRole("button", { name: "Stop agent" }).click();
  await expect(page.getByRole("button", { name: "Resume" })).toBeVisible({ timeout: SLOW });

  // The conversation the card is holding on to is now gone.
  //
  // NB: the directory is put back. Agents left running by earlier specs append
  // to their own transcripts as they work, and taking the directory out from
  // under them kills them mid-write — a failure that lands in whichever spec
  // happens to be running, not this one.
  const sessions = join(DATA_HOME, "fake-agent-sessions");
  rmSync(sessions, { recursive: true, force: true });
  mkdirSync(sessions, { recursive: true });

  await page.getByRole("button", { name: "Resume" }).click();

  // It got its task in again, which it could not have done had the resume stuck.
  await expect.poll(() => turnRefs(cardId).length, { timeout: SLOW }).toBe(2);
});

/**
 * A card's permission mode seeds its first session only. A plan-mode card whose
 * plan was approved has left plan mode, and restarting it must not put it back.
 */
test("a resumed session keeps the mode it was last in", async ({ page }) => {
  const title = "Leave plan mode behind";
  await addCard(page, projectUrl, {
    title,
    description: "[mode:acceptEdits]",
    base: "main",
    permissions: "plan",
  });
  const id = await cardIn(page, "todo", title).getAttribute("data-card-id");

  await moveCard(page, id, "in_progress");
  await expect.poll(() => turnRefs(id).length, { timeout: SLOW }).toBe(1);

  await openCard(page, id);
  await page.getByRole("button", { name: "Stop agent" }).click();
  await expect(page.getByRole("button", { name: "Resume" })).toBeVisible({ timeout: SLOW });
  await page.getByRole("button", { name: "Resume" }).click();

  await openAgent(page, id);
  await expect(terminalRows(page)).toContainText("mode acceptEdits", { timeout: SLOW });
});

/**
 * A batch is worth nothing if sending it depends on the agent still being up.
 * A stopped card starts one, and the message rides in the way an opening task
 * does — on the command line, so there is no screen to wait for and a cold
 * start costs the send nothing.
 */
test("a review sent to a stopped card starts an agent to take it", async ({ page }) => {
  const title = "Stopped when the review came in";
  await addCard(page, projectUrl, { title, description: TASK, base: "main" });
  const id = await cardIn(page, "todo", title).getAttribute("data-card-id");

  await moveCard(page, id, "in_progress");
  await expect.poll(() => turnRefs(id).length, { timeout: SLOW }).toBe(1);

  await openCard(page, id);
  await page.getByRole("button", { name: "Stop agent" }).click();
  await expect(page.getByRole("button", { name: "Resume" })).toBeVisible({ timeout: SLOW });

  await showTab(page, "review");
  const line = fileSection(page, "main.rs").locator(".line.l-added").first();
  await comment(page, line, "Say hello instead.");
  await page.getByRole("button", { name: /Send \d+ to agent/ }).click();

  // The batch left the screen, so it was taken; the turn it produced is what
  // says it actually arrived. SLOW: an agent start stands between the two.
  await expect(draftBox(line)).toHaveCount(0);
  await expect.poll(() => turnRefs(id).length, { timeout: SLOW }).toBe(2);

  // A message is not the card's task. The session it went in on was already
  // named, so the card keeps the name it had rather than taking the review's.
  await expect(page.locator(`[data-card-id="${id}"]`)).toContainText(title);
});
