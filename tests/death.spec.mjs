import { expect, test } from "./support/fixtures.mjs";
import { SLOW } from "../playwright.config.mjs";

import { addCard, addProject, cardIn, openAgent, turnRefs } from "./support/board.mjs";
import { terminalInput, terminalRows } from "./support/dom.mjs";

test.describe.configure({ mode: "serial" });

const TITLE = "Dies unannounced";

let projectUrl;
let cardId;

test.beforeAll(async ({ browser }) => {
  const page = await browser.newPage();
  projectUrl = await addProject(page);
  await addCard(page, projectUrl, {
    title: TITLE,
    description: "Work until something kills you.",
    base: "main",
  });
  cardId = await cardIn(page, "todo", TITLE).getAttribute("data-card-id");
  await page.close();
});

/**
 * An agent that crashes, or is killed from outside, sends no `SessionEnd` — the
 * pty reaching EOF is the only notice anyone gets. Nothing consumed that before:
 * the card kept whatever state it was last told, the registry kept a corpse, and
 * a websocket opened onto it would never close.
 *
 * The stand-in dies on `k` for exactly this, with no hook on the way out.
 */
test("an agent that dies without a SessionEnd still stops its card", async ({ page }) => {
  await page.goto(projectUrl);
  await cardIn(page, "todo", TITLE).dragTo(page.locator('[data-lane="in_progress"]'));

  await openAgent(page, cardId);
  await expect(page.locator("#agent-state")).toContainText("idle", { timeout: SLOW });
  expect(turnRefs(cardId)).toHaveLength(1);

  // Straight down the websocket, the same as a real keystroke.
  await terminalInput(page).press("k");

  // Nothing reported this: the server noticed because the pty closed.
  await expect(page.locator("#agent-state")).toContainText("stopped", { timeout: SLOW });
});

/**
 * The corpse is out of the registry too, not merely relabelled. A card with a
 * registered-but-dead agent still offered a terminal, and the socket it opened
 * could not close — `RecvError::Closed` is unreachable while the `Agent` that
 * owns the broadcast sender is still registered.
 */
test("a dead agent leaves no terminal to open", async ({ page }) => {
  await page.goto(`/cards/${cardId}`);

  await expect(page.locator("#agent-state")).toContainText("stopped");
  await expect(page.locator("[data-terminal]")).toHaveCount(0);
});

/**
 * And the card can be started again over the top of it, which is what the
 * eviction is ultimately for.
 */
test("the card starts again after its agent died", async ({ page }) => {
  await page.goto(`/cards/${cardId}`);
  await page.getByRole("button", { name: /start/i }).click();

  await openAgent(page, cardId);
  await expect(terminalRows(page)).toContainText(`worktrees/${cardId}`, { timeout: SLOW });
  await expect(page.locator("#agent-state")).not.toContainText("stopped");
});
