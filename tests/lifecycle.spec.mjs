import { expect, test } from "@playwright/test";

import {
  addCard,
  addProject,
  addedLines,
  cardIn,
  comment,
  git,
  openCard,
  turnRefs,
} from "./support/board.mjs";

test.describe.configure({ mode: "serial" });

const TITLE = "Add a build banner";
const TASK = "Print a banner line when the program starts.";

let projectUrl;
let cardId;

/** Turn snapshots are written by a hook, so they land a moment after the UI settles. */
const expectTurns = (count) =>
  expect.poll(() => turnRefs(cardId).length, { timeout: 25_000 }).toBe(count);

test.beforeAll(async ({ browser }) => {
  const page = await browser.newPage();
  projectUrl = await addProject(page);
  await addCard(page, projectUrl, { title: TITLE, description: TASK, base: "main" });
  cardId = await cardIn(page, "todo", TITLE).getAttribute("data-card-id");
  await page.close();
});

test("entering In Progress creates a detached worktree and starts an agent", async ({ page }) => {
  await page.goto(projectUrl);
  await cardIn(page, "todo", TITLE).dragTo(page.locator('[data-lane="in_progress"]'));

  await expect
    .poll(() => git("worktree", "list"), { timeout: 15_000 })
    .toMatch(new RegExp(`worktrees/${cardId}\\s+\\w+ \\(detached HEAD\\)`));

  // The starting commit is pinned so diffs have a fixed origin.
  expect(git("for-each-ref", "--format=%(refname)", `refs/kanban2/${cardId}/base`)).toBe(
    `refs/kanban2/${cardId}/base`,
  );
});

test("the terminal streams the agent's screen", async ({ page }) => {
  await openCard(page, cardId);

  const rows = page.locator(".terminal .xterm-rows");
  await expect(rows).toContainText("fake-agent", { timeout: 15_000 });
  await expect(rows).toContainText(TITLE);
  // The agent is running inside the card's worktree, not the project checkout.
  await expect(rows).toContainText(`worktrees/${cardId}`);
});

test("the opening task is delivered and the finished turn moves the card to In Review", async ({ page }) => {
  await expectTurns(1);

  await page.goto(projectUrl);
  await expect(cardIn(page, "in_review", TITLE)).toBeVisible({ timeout: 15_000 });

  // The board card carries what the turn changed.
  await expect(cardIn(page, "in_review", TITLE).locator(".stat")).toContainText("+");

  // The snapshot captured the working tree even though the agent never committed.
  expect(addedLines(`refs/kanban2/${cardId}/base`, `refs/kanban2/${cardId}/turn-1`)).toContain(TASK);
});

test("the review pane lists the changed files with scopes for each turn", async ({ page }) => {
  await openCard(page, cardId);

  const review = page.locator("#review");
  await expect(review.locator(".file-node")).toHaveText([/main\.rs/]);
  await expect(review.locator(".diff-head .path")).toContainText("main.rs");
  // Two lines change per turn now: the appended record, and a rewritten word.
  await expect(review.locator(".line.l-added", { hasText: TASK })).toBeVisible();

  await expect(review.locator("[data-scope-select] option")).toHaveText([
    /All changes/,
    /Turn 1/,
    /Since turn 1/,
  ]);

  // The agent's closing message is surfaced outside the terminal.
  await expect(review.locator(".last-message")).toContainText("applied turn 1");
});

test("only the word that changed is marked, not the whole line", async ({ page }) => {
  await openCard(page, cardId);

  // The agent rewrote `"hi"` to `"turn-1"` on an existing line.
  const rewritten = page.locator("#review .line.l-added", { hasText: "println!" });
  await expect(rewritten).toBeVisible();

  const marked = rewritten.locator(".chg");
  await expect(marked).toHaveText("turn-1");

  // `println!` is untouched, so it must not carry the marker.
  await expect(rewritten.locator(".chg", { hasText: "println" })).toHaveCount(0);
});

test("syntax highlighting arrives as classes the page controls", async ({ page }) => {
  await openCard(page, cardId);

  const review = page.locator("#review");
  await expect(review.locator(".tok-string").first()).toBeVisible();
  await expect(review.locator(".tok-comment").first()).toBeVisible();

  // Colour belongs to the stylesheet, so nothing should carry an inline one.
  await expect(review.locator(".code [style*='color']")).toHaveCount(0);
});

test("a folded hunk opens a gap at a time and stays open around a comment", async ({ page }) => {
  await openCard(page, cardId);

  const lines = page.locator("#review .line");
  const narrow = await lines.count();

  await page.getByRole("link", { name: /Expand \d+ lines above/ }).first().click();
  await expect.poll(() => lines.count()).toBeGreaterThan(narrow);

  const opened = await lines.count();

  // The whole file is strictly more again, and once it is open there is nothing
  // left to expand.
  await page.getByRole("link", { name: "Expand whole file" }).click();
  await expect.poll(() => lines.count()).toBeGreaterThan(opened);
  await expect(page.getByRole("link", { name: /Expand/ })).toHaveCount(0);

  const whole = await lines.count();

  // And the widened view survives a comment landing on it.
  await comment(page, lines.first(), "still wide?");
  await expect(page.locator("#review .comment")).toBeVisible();
  await expect(lines).toHaveCount(whole);
});

test("a file can be ticked off, which folds it away until it is untucked", async ({ page }) => {
  await openCard(page, cardId);

  await page.getByRole("button", { name: "Viewed" }).click();
  await expect(page.locator("#review .collapsed")).toBeVisible();
  await expect(page.locator("#review .file-node.viewed")).toBeVisible();

  // The tick outlives the fragment it was made on.
  await page.reload();
  await expect(page.locator("#review .collapsed")).toBeVisible();

  await page.getByRole("button", { name: /Viewed — collapsed/ }).click();
  await expect(page.locator("#review .line").first()).toBeVisible();
});

test("a review comment goes back to the agent and produces its own turn", async ({ page }) => {
  await openCard(page, cardId);

  // Clicking a diff line opens the compose box beneath it; clicking away saves.
  await comment(page, page.locator("#review .line.l-added").first(), "Say hello instead.");

  const draft = page.locator("#review .comment", { hasText: "Say hello instead." });
  await expect(draft).toBeVisible();
  await expect(draft.locator(".tag")).toHaveText("draft");
  await expect(page.locator("#review .batch-label")).toContainText("pending");

  await page.getByRole("button", { name: /Send \d+ to agent/ }).click();

  // Once sent the comment stays put, marked as delivered.
  await expect(draft.locator(".tag")).toHaveText("sent to agent");
  await expectTurns(2);

  // Scoping to the second turn shows only what the review round added.
  const scoped = addedLines(`refs/kanban2/${cardId}/turn-1`, `refs/kanban2/${cardId}/turn-2`);
  expect(scoped).toContain("Say hello instead.");
  expect(scoped).not.toContain(TASK);
});

test("drafts can be thrown away in one go", async ({ page }) => {
  await openCard(page, cardId);

  await comment(page, page.locator("#review .line.l-added").first(), "Second thoughts.");
  await expect(page.locator("#review .comment-draft")).toBeVisible();

  await page.getByRole("button", { name: "Discard" }).click();

  await expect(page.locator("#review .comment-draft")).toHaveCount(0);
  // What was already sent is not a draft, so it stays.
  await expect(page.locator("#review .comment-submitted")).toBeVisible();
});

test("merging lands the work on the base branch and retires the card", async ({ page }) => {
  const before = git("rev-parse", "main");

  await openCard(page, cardId);
  await page.getByRole("button", { name: "Merge" }).click();

  await expect.poll(() => git("rev-parse", "main"), { timeout: 25_000 }).not.toBe(before);

  // main now carries the agent's work.
  expect(git("show", "main:main.rs")).toContain(TASK);
  expect(git("show", "main:main.rs")).toContain("Say hello instead.");

  await page.goto(projectUrl);
  await expect(cardIn(page, "done", TITLE)).toBeVisible({ timeout: 20_000 });

  // The worktree is pruned, but the turn history is kept.
  expect(git("worktree", "list")).not.toContain(`worktrees/${cardId}`);
  expect(turnRefs(cardId)).toHaveLength(2);
});
