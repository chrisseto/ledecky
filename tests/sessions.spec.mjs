import { rmSync } from "node:fs";
import { join } from "node:path";

import { expect, test } from "@playwright/test";

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
  test.slow();

  await moveCard(page, cardId, "in_progress");
  await expect.poll(() => turnRefs(cardId).length, { timeout: 40_000 }).toBe(1);

  await openCard(page, cardId);
  await page.getByRole("button", { name: "Stop agent" }).click();
  await expect(page.getByRole("button", { name: "Start agent" })).toBeVisible({ timeout: 20_000 });

  // The conversation the card is holding on to is now gone.
  rmSync(join(DATA_HOME, "fake-agent-sessions"), { recursive: true, force: true });

  await page.getByRole("button", { name: "Start agent" }).click();

  // It got its task in again, which it could not have done had the resume stuck.
  await expect.poll(() => turnRefs(cardId).length, { timeout: 40_000 }).toBe(2);
});
