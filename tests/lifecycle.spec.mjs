import { expect, test } from "@playwright/test";

import { addCard, addProject, addedLines, cardIn, git, turnRefs } from "./support/board.mjs";

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
  await page.goto(`/cards/${cardId}`);

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

  // The snapshot captured the working tree even though the agent never committed.
  expect(addedLines(`refs/kanban2/${cardId}/base`, `refs/kanban2/${cardId}/turn-1`)).toContain(TASK);
});

test("the diff pane renders the change with scopes for each turn", async ({ page }) => {
  await page.goto(`/cards/${cardId}`);

  const diff = page.locator("#diff");
  await expect(diff.locator(".file-head")).toContainText("main.rs");
  // Two lines change per turn now: the appended record, and a rewritten word.
  await expect(diff.locator("tr.l-added", { hasText: TASK })).toBeVisible();

  await expect(diff.locator("[data-diff-param='scope'] option")).toHaveText([
    /All changes/,
    /Turn 1/,
    /Since turn 1/,
  ]);

  // The agent's closing message is surfaced outside the terminal.
  await expect(diff.locator(".last-message")).toContainText("applied turn 1");
});

test("only the word that changed is marked, not the whole line", async ({ page }) => {
  await page.goto(`/cards/${cardId}`);

  // The agent rewrote `"hi"` to `"turn-1"` on an existing line.
  const rewritten = page.locator("#diff tr.l-added", { hasText: "println!" });
  await expect(rewritten).toBeVisible();

  const marked = rewritten.locator(".chg");
  await expect(marked).toHaveText("turn-1");

  // `println!` is untouched, so it must not carry the marker.
  await expect(rewritten.locator(".chg", { hasText: "println" })).toHaveCount(0);
});

test("syntax highlighting arrives as classes the page controls", async ({ page }) => {
  await page.goto(`/cards/${cardId}`);

  const diff = page.locator("#diff");
  await expect(diff.locator(".tok-string").first()).toBeVisible();
  await expect(diff.locator(".tok-comment").first()).toBeVisible();

  // Colour belongs to the stylesheet, so nothing should carry an inline one.
  await expect(diff.locator("td.code [style*='color']")).toHaveCount(0);
});

test("the context selector widens the window without another diff run", async ({ page }) => {
  await page.goto(`/cards/${cardId}`);

  const rows = page.locator("#diff tr.l");
  const narrow = await rows.count();

  // Selected by label: the value is usize::MAX, which JS cannot hold exactly.
  await page.locator("[data-diff-param='context']").selectOption({ label: "Whole file" });

  // Whole-file context shows strictly more of the file.
  await expect.poll(() => rows.count()).toBeGreaterThan(narrow);

  // And the widened view survives a comment landing on it.
  await page.locator("#diff tr.l").first().click();
  await page.locator(".comment-form textarea").fill("still wide?");
  await page.getByRole("button", { name: "Add comment" }).click();

  await expect(page.locator("#diff .comment")).toBeVisible();
  await expect.poll(() => rows.count()).toBeGreaterThan(narrow);
});

test("a review comment goes back to the agent and produces its own turn", async ({ page }) => {
  await page.goto(`/cards/${cardId}`);

  // Clicking a diff line opens the comment form beneath it.
  await page.locator("#diff tr.l-added").first().click();
  const form = page.locator(".comment-form");
  await expect(form).toBeVisible();

  await form.locator("textarea").fill("Say hello instead.");
  await form.getByRole("button", { name: "Add comment" }).click();

  const comment = page.locator("#diff .comment");
  await expect(comment).toContainText("Say hello instead.");
  await expect(comment.locator(".tag")).toHaveText("draft");

  await page.getByRole("button", { name: /Submit review/ }).click();

  // Once sent the comment stays put, marked as delivered.
  await expect(page.locator("#diff .comment .tag")).toHaveText("sent to agent");
  await expectTurns(2);

  // Scoping to the second turn shows only what the review round added.
  const scoped = addedLines(`refs/kanban2/${cardId}/turn-1`, `refs/kanban2/${cardId}/turn-2`);
  expect(scoped).toContain("Say hello instead.");
  expect(scoped).not.toContain(TASK);
});

test("merging lands the work on the base branch and retires the card", async ({ page }) => {
  const before = git("rev-parse", "main");

  await page.goto(`/cards/${cardId}`);
  await page.getByRole("button", { name: /Merge into main/ }).click();

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
