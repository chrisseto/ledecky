import { mkdirSync, rmSync } from "node:fs";
import { join } from "node:path";
import { SLOW } from "../playwright.config.mjs";

import { expect, test } from "./support/fixtures.mjs";

import { addCard, addProject, cardIn, moveCard, openCard, turnRefs } from "./support/board.mjs";
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
  await expect(page.getByRole("button", { name: "Start agent" })).toBeVisible({ timeout: SLOW });

  // The conversation the card is holding on to is now gone.
  //
  // NB: the directory is put back. Agents left running by earlier specs append
  // to their own transcripts as they work, and taking the directory out from
  // under them kills them mid-write — a failure that lands in whichever spec
  // happens to be running, not this one.
  const sessions = join(DATA_HOME, "fake-agent-sessions");
  rmSync(sessions, { recursive: true, force: true });
  mkdirSync(sessions, { recursive: true });

  await page.getByRole("button", { name: "Start agent" }).click();

  // It got its task in again, which it could not have done had the resume stuck.
  await expect.poll(() => turnRefs(cardId).length, { timeout: SLOW }).toBe(2);
});
