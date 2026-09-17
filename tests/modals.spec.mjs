import { expect, test } from "@playwright/test";

import { addCard, addProject, cardIn, comment, openCard, turnRefs } from "./support/board.mjs";

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

  await openCard(page, cardId);
  const rows = page.locator(".terminal .xterm-rows");
  await expect(rows).toContainText("Bypass Permissions mode", { timeout: 15_000 });

  // The card says it is blocked rather than pretending to work.
  await expect(page.locator("#agent-state")).toContainText("needs permission", { timeout: 15_000 });

  // Give the server well past its retry window, then confirm it neither answered
  // the dialog nor gave up on the agent.
  await page.waitForTimeout(6_000);
  await expect(rows).toContainText("Bypass Permissions mode");
  await expect(rows).not.toContainText("exiting");
  expect(turnRefs(cardId)).toHaveLength(0);
});

test("answering the dialog in the terminal releases the queued prompt", async ({ page }) => {
  await openCard(page, cardId);
  const rows = page.locator(".terminal .xterm-rows");
  await expect(rows).toContainText("Bypass Permissions mode", { timeout: 15_000 });

  // Typing goes straight down the websocket, the same as a real keystroke.
  await page.locator(".terminal .xterm-helper-textarea").press("2");
  await expect(rows).toContainText("bypass permissions accepted", { timeout: 10_000 });

  // The prompt the server has been holding is now delivered on its own.
  await expect.poll(() => turnRefs(cardId).length, { timeout: 25_000 }).toBe(1);
  await expect(page.locator("#agent-state")).toContainText("idle", { timeout: 15_000 });

  await page.goto(projectUrl);
  await expect(cardIn(page, "in_review", TITLE)).toBeVisible({ timeout: 15_000 });
});

test("a tool permission prompt shows on the card and resumes when answered", async ({ page }) => {
  await openCard(page, cardId);

  // The marker makes the fake agent raise a PermissionRequest for its next turn.
  await page.locator('label[for="tab-review"]').click();
  await comment(
    page,
    page.locator("#review .line.l-added").first(),
    "[needs-permission] run the formatter",
  );
  await page.getByRole("button", { name: /Send \d+ to agent/ }).click();

  await expect(page.locator("#agent-state")).toContainText("needs permission", { timeout: 15_000 });

  await page.locator('label[for="tab-agent"]').click();
  const rows = page.locator(".terminal .xterm-rows");
  await expect(rows).toContainText("needs approval");
  await page.locator(".terminal .xterm-helper-textarea").press("1");

  await expect.poll(() => turnRefs(cardId).length, { timeout: 25_000 }).toBe(2);
  await expect(page.locator("#agent-state")).toContainText("idle", { timeout: 15_000 });
});
