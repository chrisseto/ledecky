import { expect, test } from "@playwright/test";

import {
  addCard,
  addProject,
  addedLines,
  baseRef,
  cardIn,
  commitInRepo,
  comment,
  editWorktree,
  fileSection,
  git,
  openCard,
  pollsOfPath,
  turnRefs,
  worktreeGit,
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

  // The starting commit is recorded, which is what diffs are measured from.
  expect(git("for-each-ref", "--format=%(refname)", `refs/ledecky/${cardId}/base`)).toBe(
    `refs/ledecky/${cardId}/base`,
  );
});

test("the terminal streams the agent's screen", async ({ page }) => {
  await openCard(page, cardId);

  const rows = page.locator("div[data-terminal] .xterm-rows");
  await expect(rows).toContainText("fake-agent", { timeout: 15_000 });
  // The agent is running inside the card's worktree, not the project checkout.
  await expect(rows).toContainText(`worktrees/${cardId}`);
});

test("a wheel over the terminal never reaches the page behind it", async ({ page }) => {
  await openCard(page, cardId);

  const terminal = page.locator("div[data-terminal]");
  await expect(terminal.locator(".xterm-rows")).toContainText("fake-agent", { timeout: 15_000 });

  const box = await terminal.boundingBox();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);

  // xterm only cancels a wheel it actually scrolled with, so at either end of
  // the scrollback the page is what moves. Watching `defaultPrevented` from the
  // document catches that whether or not this viewport happens to be scrollable.
  for (const [dx, dy] of [
    [0, 600],
    [0, -600],
    [600, 0],
  ]) {
    const prevented = page.evaluate(
      () =>
        new Promise((resolve) => {
          document.addEventListener("wheel", (event) => resolve(event.defaultPrevented), {
            once: true,
            passive: true,
          });
        }),
    );
    await page.mouse.wheel(dx, dy);
    expect(await prevented).toBe(true);
  }

  expect(
    await page.evaluate(() => [
      document.documentElement.scrollTop,
      document.querySelector("#board")?.scrollLeft ?? 0,
    ]),
  ).toEqual([0, 0]);
});

test("the scrollback the agent printed before the drawer opened is scrollable", async ({ page }) => {
  await openCard(page, cardId);

  const rows = page.locator("div[data-terminal] .xterm-rows");
  await expect(rows).toContainText("fake-agent", { timeout: 15_000 });

  // The banner scrolled off the pty long before this client connected, so it is
  // only on screen if the server replayed its scrollback.
  const box = await page.locator("div[data-terminal]").boundingBox();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  for (let i = 0; i < 20; i++) await page.mouse.wheel(0, -600);

  await expect(rows).toContainText("banner-0");
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

test("the card takes the name the session gave itself", async ({ page }) => {
  await page.goto(projectUrl);

  // The title the human typed is only a label until the agent names its
  // session; from then on the card follows that name.
  await expect(page.locator(`#card-${cardId}`)).toContainText(`${TITLE} (named)`, {
    timeout: 25_000,
  });
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

  // One point per row, newest first; the toggle beside it says which side.
  await expect(review.locator("[data-range-menu] .menu-item")).toHaveText([
    /Turn 1/,
    /What this card is based on/,
  ]);
  await expect(review.locator("[data-range-menu] summary")).toContainText("All changes");
  await expect(review.locator(".modes .mode")).toHaveText(["Just this", "Since this"]);

  // The agent's closing message is surfaced outside the terminal, in the tree
  // footer rather than in the diff column where it used to crowd out the diff.
  await expect(review.locator(".tree-foot .last-message")).toContainText("applied turn 1");
  await expect(review.locator(".diff .last-message")).toHaveCount(0);
});

test("an empty range keeps the picker, so there is a way back out of it", async ({ page }) => {
  await openCard(page, cardId);

  const review = page.locator("#review");
  const menu = review.locator("[data-range-menu]");

  // With one turn and a clean worktree, "since turn 1" is that turn against
  // itself: no files.
  // The mode carries across a change of anchor, and the default is "since".
  await menu.locator("summary").click();
  await menu.getByText("Turn 1").click();

  await expect(review.locator("#diff-lines > .empty")).toBeVisible();
  await expect(review.locator(".file")).toHaveCount(0);
  await expect(menu.locator("summary")).toContainText("Since turn 1");

  await menu.locator("summary").click();
  await menu.getByText("What this card is based on").click();
  await expect(review.locator(".file")).toHaveCount(2);
});

test("uncommitted work is on screen before any turn captures it", async ({ page }) => {
  // No hook fires for this: it is the agent mid-turn, which is exactly the case
  // the pane used to render as "Nothing yet on main".
  editWorktree(cardId, "scratch.txt", "written between turns\n");
  const before = turnRefs(cardId).length;

  await openCard(page, cardId);
  const review = page.locator("#review");

  await expect(fileSection(page, "scratch.txt")).toBeVisible();
  await expect(
    fileSection(page, "scratch.txt").locator(".line.l-added", { hasText: "between turns" }),
  ).toBeVisible();

  // The picker offers it as a point of its own, above the turns.
  await review.locator("[data-range-menu] summary").click();
  await expect(review.locator("[data-range-menu] .menu-item").first()).toContainText(
    "Uncommitted work",
  );

  // And none of that recorded a turn.
  expect(turnRefs(cardId).length).toBe(before);
});

test("a commit the agent made is a point of its own in the picker", async ({ page }) => {
  // NB: only this file. The agent's own edits are uncommitted too — a turn
  // snapshot captures them without the worktree's HEAD ever moving.
  worktreeGit(cardId, "add", "scratch.txt");
  worktreeGit(cardId, "-c", "user.email=a@b.c", "-c", "user.name=a", "commit", "-qm", "banner: land it");

  await openCard(page, cardId);
  const review = page.locator("#review");
  const menu = review.locator("[data-range-menu]");

  await menu.locator("summary").click();
  const entry = menu.locator(".menu-item.anchor-commit", { hasText: "banner: land it" });
  await expect(entry).toBeVisible();

  // Reading just that commit shows only what it changed.
  await entry.click();
  await expect(menu.locator("summary")).toContainText("Since ");

  await review.locator(".modes a.mode", { hasText: "Just this" }).click();
  await expect(review.locator(".file-head .path")).toHaveText(["scratch.txt"]);
});

test("the pane keeps up with the worktree on its own", async ({ page }) => {
  const polls = pollsOfPath(page, `/cards/${cardId}/diff`);
  await openCard(page, cardId);
  await expect(page.locator("#review .file").first()).toBeVisible();

  editWorktree(cardId, "later.txt", "arrived while the drawer was open\n");

  // No reload: the pane polls its own URL.
  await expect(fileSection(page, "later.txt")).toBeVisible({ timeout: 25_000 });

  // And once it settles, an unchanged diff is answered 304 rather than swapped
  // — which only holds because the worktree's tree id is stable.
  await expect.poll(() => polls.filter((s) => s === 304).length, { timeout: 25_000 }).toBeGreaterThan(0);
});

test("a comment being written survives the poll", async ({ page }) => {
  await openCard(page, cardId);

  const line = page.locator("#review .line").first();
  await line.click();

  const textarea = page.locator(".compose textarea");
  await textarea.fill("half a thought");

  // Long enough for several ticks to have gone by had polling not stopped.
  await page.waitForTimeout(3_000);
  await expect(textarea).toHaveValue("half a thought");
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

  // The pane polls this one, so a dropped parameter would reset the range on
  // every tick rather than just on a click.
  const source = await page.locator("#review").getAttribute("up-source");
  expect(source).not.toContain("amp;");
  expect(source).toContain("scope=");
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

test("a rebase keeps upstream commits out of the card's diff", async ({ page }) => {
  const started = baseRef(cardId);

  // Step one of the merge prompt: a rebase needs a clean worktree, and the fake
  // agent only ever commits when asked to merge.
  worktreeGit(cardId, "add", "-A");
  worktreeGit(cardId, "commit", "-qm", "card work");

  const upstream = commitInRepo("upstream.txt", "landed while the card was open\n", "upstream work");

  await openCard(page, cardId);

  // Drift on its own is invisible: the worktree is detached and does not
  // contain the upstream commit, so there is nothing yet to correct.
  await expect(fileSection(page, "upstream.txt")).toHaveCount(0);
  expect(baseRef(cardId)).toBe(started);

  worktreeGit(cardId, "rebase", "main");

  // The pane's own poll is what notices; nothing external nudges the server.
  await expect.poll(() => baseRef(cardId), { timeout: 25_000 }).toBe(upstream);

  // The regression this exists for — without the base following the rebase,
  // every upstream file would show up as the card's own work.
  await expect(fileSection(page, "upstream.txt")).toHaveCount(0);
  await expect(fileSection(page, "main.rs")).toBeVisible();
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

  // The worktree is pruned, but the turn history is kept. Three turns, not two:
  // the rebase above pulled `upstream.txt` into the worktree, so the snapshot
  // that followed had a tree of its own rather than matching turn 2 and
  // returning `None`.
  expect(git("worktree", "list")).not.toContain(`worktrees/${cardId}`);
  expect(turnRefs(cardId)).toHaveLength(3);
});
