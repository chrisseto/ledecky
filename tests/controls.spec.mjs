import { expect, test } from "./support/fixtures.mjs";
import { SLOW } from "../playwright.config.mjs";

import { addCard, addProject, cardIn, moveCard, openCard, turnRefs } from "./support/board.mjs";

test.describe.configure({ mode: "serial" });

const TITLE = "Driven from the drawer";

let projectUrl;
let cardId;

test.beforeAll(async ({ browser }) => {
  const page = await browser.newPage();
  projectUrl = await addProject(page);
  await addCard(page, projectUrl, { title: TITLE, description: "Write a line.", base: "main" });
  cardId = await cardIn(page, "todo", TITLE).getAttribute("data-card-id");
  await page.close();
});

test("Start moves a card out of To Do and starts its agent", async ({ page }) => {
  await openCard(page, cardId);
  await page.getByRole("button", { name: "Start", exact: true }).click();

  await expect(cardIn(page, "todo", TITLE)).toHaveCount(0);
  await expect.poll(() => turnRefs(cardId).length, { timeout: SLOW }).toBe(1);
  await expect(cardIn(page, "in_review", TITLE)).toBeVisible();
});

test("stopping the agent leaves the card in its lane", async ({ page }) => {
  await openCard(page, cardId);
  await page.getByRole("button", { name: "Stop agent" }).click();

  await expect(page.locator("#agent-state")).toContainText("stopped");
  await expect(page.getByRole("button", { name: "Resume" })).toBeVisible();
  await expect(cardIn(page, "in_review", TITLE)).toBeVisible();
});

test("resuming the agent leaves the card in its lane", async ({ page }) => {
  await openCard(page, cardId);
  await page.getByRole("button", { name: "Resume" }).click();

  await expect(page.getByRole("button", { name: "Stop agent" })).toBeVisible();
  await expect(page.locator("#agent-state")).not.toContainText("stopped");
  await expect(cardIn(page, "in_review", TITLE)).toBeVisible();
});

test("moving a card to Done stops its agent", async ({ page }) => {
  await moveCard(page, cardId, "done");

  await openCard(page, cardId);
  await expect(page.locator("#agent-state")).toContainText("stopped");
  await expect(page.locator("[data-terminal]")).toHaveCount(0);
  // Done offers no way back to a running agent.
  await expect(page.getByRole("button", { name: /Start|Resume/ })).toHaveCount(0);
});
