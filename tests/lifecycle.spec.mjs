import { expect, test } from "@playwright/test";

import {
  addCard,
  addProject,
  addedLines,
  cardIn,
  comment,
  fileSection,
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
  expect(git("for-each-ref", "--format=%(refname)", `refs/ledecky/${cardId}/base`)).toBe(
    `refs/ledecky/${cardId}/base`,
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
  expect(addedLines(`refs/ledecky/${cardId}/base`, `refs/ledecky/${cardId}/turn-1`)).toContain(TASK);
});

test("the review pane stacks every changed file, with scopes for each turn", async ({ page }) => {
  await openCard(page, cardId);

  const review = page.locator("#review");
  await expect(review.locator(".file-node")).toHaveText([/README\.md/, /main\.rs/]);

  // Both files are on the page at once, each under its own header.
  await expect(review.locator(".file")).toHaveCount(2);
  await expect(review.locator(".file-head .path")).toHaveText(["README.md", "main.rs"]);

  // Two lines change per turn now: the appended record, and a rewritten word.
  await expect(fileSection(page, "main.rs").locator(".line.l-added", { hasText: TASK })).toBeVisible();

  await expect(review.locator("[data-scope-select] option")).toHaveText([
    /All changes/,
    /Turn 1/,
    /Since turn 1/,
  ]);

  // The agent's closing message is surfaced outside the terminal, in the tree
  // footer rather than in the diff column where it used to crowd out the diff.
  await expect(review.locator(".tree-foot .last-message")).toContainText("applied turn 1");
  await expect(review.locator(".diff .last-message")).toHaveCount(0);
});

test("the tree jumps to a file instead of reloading the pane", async ({ page }) => {
  await openCard(page, cardId);

  const lines = page.locator("#diff-lines");
  // The diff column has room of its own, which is the complaint this replaced:
  // an unbounded sibling used to squeeze it to nothing.
  await expect.poll(() => lines.evaluate((el) => el.clientHeight)).toBeGreaterThan(200);

  // The fixture diff is short, so shrink the window rather than pad the repo.
  await page.setViewportSize({ width: 900, height: 400 });
  await expect
    .poll(() => lines.evaluate((el) => el.scrollHeight - el.clientHeight))
    .toBeGreaterThan(0);

  const top = () => lines.evaluate((el) => el.scrollTop);
  const url = page.url();

  await page.locator('.file-node[href="#file-1"]').click();
  await expect.poll(top).toBeGreaterThan(0);

  // And back up to the first file.
  await page.locator('.file-node[href="#file-0"]').click();
  await expect.poll(top).toBe(0);

  // Both jumps were scrolls, not navigations: the tree left the URL alone.
  expect(page.url()).toBe(url);
});

test("every link out of the pane carries a usable query", async ({ page }) => {
  await openCard(page, cardId);

  // `&amp;` written into a template variable gets escaped a second time, which
  // silently drops the parameter after it.
  const hrefs = await page.locator("#review a[href]").evaluateAll((els) =>
    els.map((el) => el.getAttribute("href")),
  );
  expect(hrefs.filter((href) => href.includes("amp;"))).toEqual([]);
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

  const main = fileSection(page, "main.rs");
  const lines = main.locator(".line");
  const narrow = await lines.count();

  await main.getByRole("link", { name: /Expand \d+ lines above/ }).first().click();
  await expect.poll(() => lines.count()).toBeGreaterThan(narrow);

  const opened = await lines.count();

  // The whole file is strictly more again, and once it is open there is nothing
  // left to expand.
  await main.getByRole("link", { name: "Expand whole file" }).click();
  await expect.poll(() => lines.count()).toBeGreaterThan(opened);
  await expect(main.getByRole("link", { name: /Expand/ })).toHaveCount(0);

  const whole = await lines.count();

  // And the widened view survives a comment landing on it.
  await comment(page, lines.first(), "still wide?");
  await expect(page.locator("#review .comment")).toBeVisible();
  await expect(lines).toHaveCount(whole);
});

test("a file can be ticked off, which folds it away until it is untucked", async ({ page }) => {
  await openCard(page, cardId);

  const main = fileSection(page, "main.rs");
  await main.getByRole("button", { name: "Viewed" }).click();
  await expect(main.locator(".collapsed")).toBeVisible();
  await expect(page.locator("#review .file-node.viewed")).toHaveText([/main\.rs/]);
  // The other file is untouched by the tick.
  await expect(fileSection(page, "README.md").locator(".line").first()).toBeVisible();

  // The tick outlives the fragment it was made on.
  await page.reload();
  await expect(main.locator(".collapsed")).toBeVisible();

  await main.getByRole("button", { name: /Viewed — collapsed/ }).click();
  await expect(main.locator(".line").first()).toBeVisible();
});

test("a review comment goes back to the agent and produces its own turn", async ({ page }) => {
  await openCard(page, cardId);

  // Clicking a diff line opens the compose box beneath it; clicking away saves.
  await comment(page, fileSection(page, "main.rs").locator(".line.l-added").first(), "Say hello instead.");

  const draft = page.locator("#review .comment", { hasText: "Say hello instead." });
  await expect(draft).toBeVisible();
  await expect(draft.locator(".tag")).toHaveText("draft");
  await expect(page.locator("#review .batch-label")).toContainText("pending");

  await page.getByRole("button", { name: /Send \d+ to agent/ }).click();

  // Once sent the comment stays put, marked as delivered.
  await expect(draft.locator(".tag")).toHaveText("sent to agent");
  await expectTurns(2);

  // Scoping to the second turn shows only what the review round added.
  const scoped = addedLines(`refs/ledecky/${cardId}/turn-1`, `refs/ledecky/${cardId}/turn-2`);
  expect(scoped).toContain("Say hello instead.");
  expect(scoped).not.toContain(TASK);
});

test("drafts can be thrown away in one go", async ({ page }) => {
  await openCard(page, cardId);

  await comment(page, fileSection(page, "main.rs").locator(".line.l-added").first(), "Second thoughts.");
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
