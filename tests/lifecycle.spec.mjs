import { existsSync } from "node:fs";

import { expect, test } from "./support/fixtures.mjs";
import { SLOW } from "../playwright.config.mjs";

import {
  addCard,
  addedLines,
  addProject,
  anchoredTo,
  baseRef,
  cardDirOf,
  cardIn,
  cardRefs,
  comment,
  commitInRepo,
  draftBox,
  editWorktree,
  fileSection,
  git,
  lane,
  moveCard,
  openAgent,
  openCard,
  pollsOfPath,
  refreshDiff,
  removeInWorktree,
  saveSettled,
  showTab,
  staleDiff,
  turnRefs,
  worktreeGit,
  worktreeOf,
} from "./support/board.mjs";
import { terminal, terminalRows } from "./support/dom.mjs";

test.describe.configure({ mode: "serial" });

const TITLE = "Add a build banner";
const TASK = "Print a banner line when the program starts.";

let projectUrl;
let cardId;

/** Turn snapshots are written by a hook, so they land a moment after the UI settles. */
const expectTurns = (count) =>
  expect.poll(() => turnRefs(cardId).length).toBe(count);

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
    .poll(() => git("worktree", "list"))
    .toMatch(new RegExp(`worktrees/${cardId}\\s+\\w+ \\(detached HEAD\\)`));

  // The starting commit is recorded, which is what diffs are measured from.
  expect(git("for-each-ref", "--format=%(refname)", `refs/ledecky/${cardId}/base`)).toBe(
    `refs/ledecky/${cardId}/base`,
  );
});

test("the terminal streams the agent's screen", async ({ page }) => {
  await openAgent(page, cardId);

  const rows = terminalRows(page);
  // The agent is running inside the card's worktree, not the project checkout.
  //
  // NB: asserted rather than the agent's banner line. The banner is the very
  // first thing it prints, so it is also the first thing any scroll takes away
  // — and a turn landing before this looks is enough to do it.
  await expect(rows).toContainText(`worktrees/${cardId}`);
});

test("a wheel over the terminal never reaches the page behind it", async ({ page }) => {
  await openAgent(page, cardId);

  await expect(terminalRows(page)).toContainText(`worktrees/${cardId}`);

  const box = await terminal(page).boundingBox();
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
  await openAgent(page, cardId);

  const rows = terminalRows(page);
  await expect(rows).toContainText(`worktrees/${cardId}`);

  // The banner scrolled off the pty long before this client connected, so it is
  // only on screen if the server replayed its scrollback.
  const box = await terminal(page).boundingBox();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  for (let i = 0; i < 20; i++) await page.mouse.wheel(0, -600);

  await expect(rows).toContainText("banner-0");
});

test("the opening task is delivered and the finished turn moves the card to In Review", async ({ page }) => {
  await expectTurns(1);

  await page.goto(projectUrl);
  await expect(cardIn(page, "in_review", TITLE)).toBeVisible();

  // The board card carries what the turn changed.
  await expect(cardIn(page, "in_review", TITLE).locator(".stat")).toContainText("+");

  // The snapshot captured the working tree even though the agent never committed.
  expect(addedLines(`refs/ledecky/${cardId}/base`, `refs/ledecky/${cardId}/turn-1`)).toContain(TASK);
});

test("the card takes the name the session gave itself", async ({ page }) => {
  await page.goto(projectUrl);

  // The title the human typed is only a label until the agent names its
  // session; from then on the card follows that name.
  await expect(page.locator(`#card-${cardId}`)).toContainText(`${TITLE} (named)`);
});

test("a terminal opened behind the review tab still fits its own pane", async ({ page }) => {
  // Now that there is a diff to read, the drawer lands on Review — so the agent
  // pane is hidden, and measures nothing, at the moment the drawer compiles.
  await openCard(page, cardId);
  await expect(page.locator("#tab-review")).toBeChecked();

  await showTab(page, "agent");

  const rows = terminalRows(page);
  await expect(rows).toContainText("fake-agent", { timeout: 15_000 });

  const box = await terminal(page).boundingBox();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  for (let i = 0; i < 20; i++) await page.mouse.wheel(0, -600);

  // One line of the replayed scrollback is 100 columns wide. A terminal that
  // took the replay at xterm's 80-column default holds it in two rows, and the
  // fit that comes with the tab switch only reflows the damage.
  const ruler = "=".repeat(100);
  await expect
    .poll(() =>
      rows.locator("> div").evaluateAll(
        (divs, want) => divs.filter((div) => div.textContent.trimEnd() === want).length,
        ruler,
      ),
    )
    .toBe(1);
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
});

test("an empty range keeps the picker, so there is a way back out of it", async ({ page }) => {
  await openCard(page, cardId);

  const review = page.locator("#review");
  const menu = review.locator("[data-range-menu]");

  // The mode carries across a change of anchor, and the default is "since",
  // which includes the point itself.
  await menu.locator("summary").click();
  await menu.getByText("Turn 1").click();
  await expect(menu.locator("summary")).toContainText("Since turn 1");
  await expect(review.locator(".file")).toHaveCount(2);

  // A commit that changed nothing, read on its own: no files.
  worktreeGit(cardId, "-c", "user.email=a@b.c", "-c", "user.name=a", "commit", "-q", "--allow-empty", "-m", "nothing at all");
  await openCard(page, cardId);
  await menu.locator("summary").click();
  await menu.locator(".menu-item.anchor-commit", { hasText: "nothing at all" }).click();
  await review.locator(".modes a.mode", { hasText: "Just this" }).click();

  await expect(review.locator("#diff-lines > .empty")).toBeVisible();
  await expect(review.locator(".file")).toHaveCount(0);

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

test("a commit's message is on screen, and takes comments like a diff line", async ({ page }) => {
  worktreeGit(
    cardId, "-c", "user.email=a@b.c", "-c", "user.name=a",
    "commit", "-q", "--allow-empty", "-m", "banner: explain it", "-m", "Why it prints first.",
  );

  await openCard(page, cardId);
  const review = page.locator("#review");
  const commits = review.locator("#commits");

  await expect(commits.locator(".line", { hasText: "banner: explain it" })).toBeVisible();
  const body = commits.locator(".line", { hasText: "Why it prints first." });

  await comment(page, body, "Say what it prints.");
  await expect(draftBox(body)).toHaveValue("Say what it prints.");
  await expect(review.locator('.file-node[href="#commits"] .badge')).toHaveText("1");
  await expect(review.locator(".batch-label")).toContainText("1 comment pending");

  await page.getByRole("button", { name: "Discard" }).click();
  await expect(commits.locator(".compose")).toHaveCount(0);
});

test("the pane says when the worktree has moved, and redraws when asked", async ({ page }) => {
  const refetches = pollsOfPath(page, `/cards/${cardId}/diff`);
  await openCard(page, cardId);
  await expect(page.locator("#review .file").first()).toBeVisible();
  await expect(staleDiff(page)).toBeHidden();

  editWorktree(cardId, "later.txt", "arrived while the drawer was open\n");

  // No reload, and no redraw either: the watcher announces the write, and the
  // pane offers the redraw rather than taking it. Nothing on screen moves until
  // the reader says so.
  await expect(staleDiff(page)).toBeVisible();
  await expect(fileSection(page, "later.txt")).toHaveCount(0);
  expect(refetches).toHaveLength(1);

  await refreshDiff(page);
  await expect(fileSection(page, "later.txt")).toBeVisible();
  expect(refetches).toHaveLength(2);

  // And the light comes back on for the next one, which is what says the watch
  // outlived the redraw rather than being a one-off.
  editWorktree(cardId, "later-still.txt", "and another\n");
  await refreshDiff(page);
  await expect(fileSection(page, "later-still.txt")).toBeVisible();
  expect(refetches).toHaveLength(3);
});

test("work in a directory the agent just made announces itself too", async ({ page }) => {
  const refetches = pollsOfPath(page, `/cards/${cardId}/diff`);
  await openCard(page, cardId);
  await expect(page.locator("#review .file").first()).toBeVisible();

  // Nothing git ignores is watched, so a new directory has to be picked up as
  // it arrives — and the file inside it lands before the watch on it does.
  editWorktree(cardId, "pkg/arrived.txt", "written into a directory that did not exist\n");
  await refreshDiff(page);
  await expect(fileSection(page, "pkg/arrived.txt")).toBeVisible();

  // And it is watched from then on rather than merely swept up the once: a
  // second write to the same directory lights it again, and only once.
  const afterFirst = refetches.length;
  editWorktree(cardId, "pkg/again.txt", "and again, into one that now exists\n");
  await refreshDiff(page);
  await expect(fileSection(page, "pkg/again.txt")).toBeVisible();
  expect(refetches).toHaveLength(afterFirst + 1);
});

test("a directory replaced wholesale is watched again", async ({ page }) => {
  const refetches = pollsOfPath(page, `/cards/${cardId}/diff`);
  await openCard(page, cardId);

  // Watched for certain before it is replaced: the write has to have landed.
  editWorktree(cardId, "swap/first.txt", "a directory to take away again\n");
  await refreshDiff(page);
  await expect(fileSection(page, "swap/first.txt")).toBeVisible();

  // Deleted and remade inside one burst. The descriptor was on the old inode,
  // which the kernel dropped with it, so the watch has to be laid again — the
  // directory being there under the same name says nothing about that.
  removeInWorktree(cardId, "swap");
  editWorktree(cardId, "swap/remade.txt", "the directory was replaced under it\n");
  await refreshDiff(page);
  await expect(fileSection(page, "swap/remade.txt")).toBeVisible();

  // And it is the new directory that is watched, rather than the burst that
  // replaced it having swept the file up the once.
  const afterRemake = refetches.length;
  editWorktree(cardId, "swap/once-more.txt", "still keeping up\n");
  await refreshDiff(page);
  await expect(fileSection(page, "swap/once-more.txt")).toBeVisible();
  expect(refetches).toHaveLength(afterRemake + 1);
});

test("a comment being written survives an update to the diff", async ({ page }) => {
  await openCard(page, cardId);

  const line = page.locator("#review .line").first();
  await line.click();

  const textarea = draftBox(line);
  await textarea.fill("half a thought");

  editWorktree(cardId, "during-comment.txt", "written while a comment was open\n");

  // The write is announced and nothing else happens. This is the whole reason
  // the pane stopped redrawing itself: a box being typed into is not something
  // to move out from under someone.
  await expect(staleDiff(page)).toBeVisible();
  await expect(textarea).toHaveValue("half a thought");
  await saveSettled(page);

  // The redraw adds the new file and keeps the box, with its text. The box is
  // the draft itself, not a copy of it.
  await refreshDiff(page);
  await expect(fileSection(page, "during-comment.txt")).toBeVisible();
  await expect(textarea).toHaveValue("half a thought");
});

test("the redraw can be taken with a comment half-written", async ({ page }) => {
  await openCard(page, cardId);

  const line = page.locator("#review .line").first();
  await line.click();
  await draftBox(line).fill("mid sentence");

  editWorktree(cardId, "while-typing.txt", "written while a comment was open\n");
  await expect(staleDiff(page)).toBeVisible();

  // NB: a redraw that starts in the box is the difficult case. The click
  // removes the focus from the textarea, the blur sends the text that the
  // throttle holds, and the browser fetches the pane again. The save must
  // complete first, or the redraw shows a box without the last word.
  await refreshDiff(page);
  await expect(fileSection(page, "while-typing.txt")).toBeVisible();
  await expect(draftBox(page.locator("#review .line").first())).toHaveValue("mid sentence");
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
  // The tree names the file being read, and it follows the scroller rather
  // than the click — so this is also what says the observer behind it is still
  // wired up. Nothing else in the suite looks at `selected`.
  await expect(page.locator(".file-node.selected")).toHaveAttribute("href", "#file-1");

  // And back up to the top, where the card's commits sit above its files.
  await page.locator('.file-node[href="#commits"]').click();
  await expect.poll(top).toBe(0);
  await expect(page.locator(".file-node.selected")).toHaveAttribute("href", "#file-0");

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

  // The pane refetches this one, so a dropped parameter would reset the range
  // on every update rather than just on a click. It is an element of its own so
  // a fragment can correct it out of band — see `_pane_source.html`.
  const source = await page.locator("#pane-source").getAttribute("hx-get");
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
  //
  // NB: named rather than "a comment is on screen". Drafts outlive the test
  // that wrote them — the batch is card-wide and deliberately not cleared
  // between them — so anything counting on being the only one is a flake
  // waiting for the run where it is not.
  await comment(page, lines.first(), "still wide?");
  await expect(draftBox(lines.first())).toHaveValue("still wide?");
  await expect(lines).toHaveCount(whole);
});

test("a file can be ticked off, which folds it away until it is untucked", async ({ page }) => {
  const panes = pollsOfPath(page, `/cards/${cardId}/diff`);
  const ticks = pollsOfPath(page, `/cards/${cardId}/viewed`);
  await openCard(page, cardId);

  const main = fileSection(page, "main.rs");
  const readme = fileSection(page, "README.md");
  await expect(main.locator(".line").first()).toBeVisible();
  // The stream opens with a resync, so the pane fetches itself once on its own.
  // Count from after that rather than racing it.
  await expect.poll(() => panes.length).toBeGreaterThan(0);
  const drawn = panes.length;

  // A file is a `<details>` and its header is the `<summary>`, so this is the
  // browser's own disclosure — the path is a plain span, and clicking it works
  // the summary the way clicking anywhere else on the header does.
  await main.locator(".file-head .path").click();

  await expect(main.locator(".line").first()).toBeHidden();
  await expect(page.locator("#review .file-node.viewed")).toHaveText([/main\.rs/]);
  // The other file is untouched by the tick.
  await expect(readme.locator(".line").first()).toBeVisible();

  // And the point of doing it this way. The tick is persisted — so the server
  // was spoken to, and waiting for that is what gives the next line its teeth —
  // but it answered 204 and the pane never redrew. A redraw here would be the
  // whole diff over the wire to say something already on screen.
  await expect.poll(() => ticks.length).toBe(1);
  expect(panes.length).toBe(drawn);

  // The tick still outlives the fragment it was made on — it is persisted
  // behind the fold rather than rendered by it.
  await page.reload();
  await expect(main.locator(".line").first()).toBeHidden();
  await expect(readme.locator(".line").first()).toBeVisible();

  await main.locator(".file-head .path").click();
  await expect(main.locator(".line").first()).toBeVisible();

  // NB: and wait for that one to persist too. The fold is reported behind the
  // reader's back, so a test that walks away with the write still in flight
  // leaves the next one to open a card that is about to fold itself.
  await expect.poll(() => ticks.length).toBe(2);
});

test("the lines carry no request of their own, and still open their box", async ({ page }) => {
  const panes = pollsOfPath(page, `/cards/${cardId}/diff`);
  await openCard(page, cardId);

  const line = fileSection(page, "main.rs").locator(".line.l-added").first();
  await expect(line).toBeVisible();
  // Every line of every file is on the page at once, so an `hx-get` per line is
  // the diff's whole length in attributes — and in elements for htmx to process
  // on each update. One delegated listener builds the same view from the line's
  // anchor.
  await expect(page.locator("#review .line[hx-get]")).toHaveCount(0);

  // The same argument for the path and for the two cells that only ever held a
  // constant: the enclosing section already says which file this is, and the
  // gutter `+` and the diff sign are drawn by the stylesheet.
  await expect(page.locator("#review .line[data-file]")).toHaveCount(0);
  await expect(page.locator("#review .line .add, #review .line .sign")).toHaveCount(0);
  await expect(line.locator("> *")).toHaveCount(3);

  // The stream opens with a resync, so the pane fetches itself once on its own.
  await expect.poll(() => panes.length).toBeGreaterThan(0);
  const drawn = panes.length;

  await line.click();
  await expect(draftBox(line)).toBeVisible();
  // The box is empty, and it does not contain the word "none". The text comes
  // from an `Option`, and minijinja renders a none value as that word.
  await expect(draftBox(line)).toHaveValue("");

  // A second click on the same line closes an empty box. A box that contains
  // text is the draft of the line, and it stays open.
  await line.click();
  await expect(anchoredTo(line).locator(".compose")).toHaveCount(0);

  // A box is one line's business, so none of that redrew the pane. It is what
  // keeps an open range menu open, and a comment off the diff's critical path:
  // the whole thing used to come back over the wire to show one textarea.
  await comment(page, line, "and this lands the same way");
  await expect(draftBox(line)).toHaveValue("and this lands the same way");
  expect(panes.length).toBe(drawn);

  // The count beside it still moved, out of band with the block.
  await expect(page.locator("#review .batch-label")).toContainText("pending");
  await page.getByRole("button", { name: "Discard" }).click();
  await expect(anchoredTo(line).locator(".compose")).toHaveCount(0);
});

test("a review comment goes back to the agent and produces its own turn", async ({ page }) => {
  await openCard(page, cardId);

  // A click on a diff line opens the box below it. The box saves its own text.
  const line = fileSection(page, "main.rs").locator(".line.l-added").first();
  await comment(page, line, "Say hello instead.");

  // One box only. The box sends its text against the line, not against a new
  // comment, so two saves change one draft instead of writing two.
  await expect(anchoredTo(line).locator(".compose")).toHaveCount(1);
  await expect(draftBox(line)).toHaveValue("Say hello instead.");
  await expect(page.locator("#review .batch-label")).toContainText("pending");

  await page.getByRole("button", { name: /Send \d+ to agent/ }).click();

  // Feedback already given is not feedback to give: every working range ends at
  // the live head, so sending takes the batch off the screen it was written on.
  await expect(page.locator("#review .compose, #review .comment")).toHaveCount(0);
  await expectTurns(2);

  // Scoping to the second turn shows only what the review round added.
  const scoped = addedLines(`refs/ledecky/${cardId}/turn-1`, `refs/ledecky/${cardId}/turn-2`);
  expect(scoped).toContain("Say hello instead.");
  expect(scoped).not.toContain(TASK);

  // And asking for the turn it was written against is asking for its record:
  // there it is, delivered and no longer yours to withdraw.
  const menu = page.locator("#review [data-range-menu]");
  await menu.locator("summary").click();
  await menu.locator(".menu-item.anchor-turn", { hasText: "Turn 1" }).click();
  await page.locator("#review .modes a.mode", { hasText: "Just this" }).click();

  // The comment is a record, so the pane shows it as text and gives no box.
  const sent = page.locator("#review .comment", { hasText: "Say hello instead." });
  await expect(sent).toHaveCount(1);
  await expect(sent.locator(".tag")).toHaveText("sent to agent");
  await expect(page.locator("#review .compose")).toHaveCount(0);
});

test("a pending comment stays in its box and saves as it is typed", async ({ page }) => {
  await openCard(page, cardId);

  const line = fileSection(page, "main.rs").locator(".line.l-added").first();
  await comment(page, line, "Say hullo instead.");

  // The box gives the turn that the server rendered it for. Without that
  // value, a new turn moves the row that the next save looks for, and the save
  // writes a second comment on the line.
  await expect(anchoredTo(line).locator('input[name="turn"]')).toHaveCount(1);

  // The box remains, and it holds the text. To change the comment, the user
  // types in it again.
  const textarea = draftBox(line);
  await expect(textarea).toHaveValue("Say hullo instead.");
  await textarea.fill("Say hello instead, properly.");

  // No click here. The throttle sends the text.
  await saveSettled(page);

  // The server renders the draft as the box that holds it, so the redraw shows
  // the box again, with its text.
  editWorktree(cardId, "during-edit.txt", "written while a comment was open\n");
  await refreshDiff(page);
  await expect(fileSection(page, "during-edit.txt")).toBeVisible();
  await expect(draftBox(line)).toHaveValue("Say hello instead, properly.");

  // A change writes the same row, so the batch is the same size. The line also
  // has no second box.
  await expect(page.locator("#review .batch-label")).toContainText("1 comment pending");
  await expect(anchoredTo(line).locator(".compose")).toHaveCount(1);

  await page.getByRole("button", { name: "Discard" }).click();
  await expect(anchoredTo(line).locator(".compose")).toHaveCount(0);
});

test("two lines hold two boxes, each saving its own", async ({ page }) => {
  await openCard(page, cardId);

  const lines = fileSection(page, "main.rs").locator(".line.l-added");
  const [first, second] = [lines.nth(0), lines.nth(1)];

  await comment(page, first, "This one.");
  await comment(page, second, "And this other one.");

  // Two boxes are open at the same time. The save identifies the line, so text
  // in one box must not reach the other.
  await expect(draftBox(first)).toHaveValue("This one.");
  await expect(draftBox(second)).toHaveValue("And this other one.");
  await expect(page.locator("#review .batch-label")).toContainText("2 comments pending");

  await draftBox(second).fill("And this other one, reworded.");
  await page.locator(".batch-label").click();
  await saveSettled(page);

  // The server renders the pane again from the database, so these values are
  // the values that the server has.
  await openCard(page, cardId);
  await expect(draftBox(first)).toHaveValue("This one.");
  await expect(draftBox(second)).toHaveValue("And this other one, reworded.");
  await expect(page.locator("#review .batch-label")).toContainText("2 comments pending");

  // A delete of one box keeps the other box.
  await anchoredTo(first).getByRole("button", { name: "Remove" }).click();
  await expect(anchoredTo(first).locator(".compose")).toHaveCount(0);
  await expect(draftBox(second)).toHaveValue("And this other one, reworded.");

  await page.getByRole("button", { name: "Discard" }).click();
  await expect(page.locator("#review .compose")).toHaveCount(0);
});

test("a second box takes the caret, and the first stops asking for it", async ({ page }) => {
  await openCard(page, cardId);

  const lines = fileSection(page, "main.rs").locator(".line.l-added");
  const [first, second] = [lines.nth(0), lines.nth(1)];

  await comment(page, first, "the first one");

  // The cursor goes to the box that opens now. htmx focuses an `autofocus`
  // element in the content that it inserts, so an older box on the page cannot
  // take the cursor from a new one.
  await second.click();
  await expect(draftBox(second)).toBeFocused();

  await page.keyboard.type("the second one");
  await expect(draftBox(second)).toHaveValue("the second one");
  await expect(draftBox(first)).toHaveValue("the first one");

  await page.getByRole("button", { name: "Discard" }).click();
  await expect(page.locator("#review .compose")).toHaveCount(0);
});

test("a save that does not land says so, and the retry lands it", async ({ page }) => {
  await openCard(page, cardId);

  const line = fileSection(page, "main.rs").locator(".line.l-added").first();

  // One save completes first. The retry button must still work after a save
  // that completes, not only after a failure of the first save.
  await comment(page, line, "lands");

  // The server now refuses every save on this line, so the box holds text that
  // the server does not have. The box must show this state, because the user
  // has no other way to see it.
  const saves = `**/cards/${cardId}/comments`;
  await page.route(saves, (route) =>
    route.request().method() === "POST" ? route.fulfill({ status: 500 }) : route.fallback(),
  );
  await draftBox(line).fill("this will not land");

  const box = anchoredTo(line).locator(".compose");
  await expect(box.getByText("Not saved")).toBeVisible();

  // The text is still in the box, so a retry is sufficient to recover.
  await page.unroute(saves);
  await box.getByRole("button", { name: "retry" }).click();
  await saveSettled(page);

  await expect(box.getByText("Not saved")).toBeHidden();
  await expect(page.locator("#review .batch-label")).toContainText("1 comment pending");

  await page.getByRole("button", { name: "Discard" }).click();
  await expect(page.locator("#review .compose")).toHaveCount(0);
});

test("emptying the box withdraws the comment", async ({ page }) => {
  await openCard(page, cardId);

  const line = fileSection(page, "main.rs").locator(".line.l-added").first();
  await comment(page, line, "On reflection, no.");

  // An empty box is not a comment, so the save of an empty box deletes it.
  await draftBox(line).fill("");
  await page.locator(".batch-label").click();
  await expect(page.locator("#review .batch-label")).toContainText("Click a line to comment");

  // The box stays, and the control below it still works. The control
  // identifies the line, so it does not need a draft.
  await expect(draftBox(line)).toBeVisible();
  await anchoredTo(line).getByRole("button", { name: "Remove" }).click();
  await expect(anchoredTo(line).locator(".compose")).toHaveCount(0);
});

test("a pending comment can be withdrawn on its own", async ({ page }) => {
  await openCard(page, cardId);

  const line = fileSection(page, "main.rs").locator(".line.l-added").first();
  await comment(page, line, "And again, no.");

  // The request is a `DELETE`, so the line and the range are query parameters;
  // htmx reads no form for a `DELETE`. The URL identifies the line, because the
  // browser has no comment id.
  await anchoredTo(line).getByRole("button", { name: "Remove" }).click();

  await expect(anchoredTo(line).locator(".compose")).toHaveCount(0);
  // Only the block of that line changes, and the response also contains the
  // comment count below the diff.
  await expect(page.locator("#review .batch-label")).toContainText("Click a line to comment");
  await expect(page.locator("#review [data-range-menu] summary")).toContainText("All changes");

  // The line also stops being the line with an open box. If the fetch URL of
  // the pane named it, the next redraw would open an empty box on that line and
  // move the cursor to it.
  editWorktree(cardId, "after-remove.txt", "written after a comment was withdrawn\n");
  await refreshDiff(page);
  await expect(fileSection(page, "after-remove.txt")).toBeVisible();
  await expect(page.locator("#review .compose")).toHaveCount(0);
});

test("drafts can be thrown away in one go", async ({ page }) => {
  await openCard(page, cardId);

  const line = fileSection(page, "main.rs").locator(".line.l-added").first();
  await comment(page, line, "Second thoughts.");
  await expect(draftBox(line)).toBeVisible();

  await page.getByRole("button", { name: "Discard" }).click();

  await expect(page.locator("#review .compose")).toHaveCount(0);
  // The rounds before this one were sent, and sent comments live on their own
  // turn rather than on the range the card is at now.
  await expect(page.locator("#review .comment")).toHaveCount(0);
});

test("a comment left on an older turn stays there", async ({ page }) => {
  await openCard(page, cardId);

  const review = page.locator("#review");
  const menu = review.locator("[data-range-menu]");

  await menu.locator("summary").click();
  await menu.locator(".menu-item.anchor-turn", { hasText: "Turn 1" }).click();
  await review.locator(".modes a.mode", { hasText: "Just this" }).click();

  await comment(page, fileSection(page, "main.rs").locator(".line.l-added").first(), "Old news.");
  const draft = draftBox(fileSection(page, "main.rs").locator(".line.l-added").first());
  await expect(draft).toHaveValue("Old news.");

  // Back at the head of the card it is not on screen — but the count is
  // card-wide, so it is not lost either.
  await menu.locator("summary").click();
  await menu.getByText("What this card is based on").click();

  await expect(draft).toHaveCount(0);
  await expect(review.locator(".batch-label")).toContainText("1 on another range");

  await page.getByRole("button", { name: "Discard" }).click();
  await expect(review.locator(".batch-label")).not.toContainText("another range");
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
  await expect.poll(() => baseRef(cardId)).toBe(upstream);

  // The regression this exists for — without the base following the rebase,
  // every upstream file would show up as the card's own work.
  await expect(fileSection(page, "upstream.txt")).toHaveCount(0);
  await expect(fileSection(page, "main.rs")).toBeVisible();
});

test("a turn taken before a rebase is still measured from its own base", async ({ page }) => {
  // The turns above were snapshotted against the base the card started from, and
  // the rebase has since moved that base out from under them. Reaching for the
  // card's base here would pair a post-rebase tree with a pre-rebase one and
  // render every upstream file as a deletion.
  await openCard(page, cardId);

  const review = page.locator("#review");
  const menu = review.locator("[data-range-menu]");

  await menu.locator("summary").click();
  await menu.getByText("Turn 1", { exact: true }).click();
  await review.locator(".modes a.mode", { hasText: "Just this" }).click();
  await expect(menu.locator("summary")).toContainText("Turn 1");

  await expect(fileSection(page, "upstream.txt")).toHaveCount(0);
  await expect(fileSection(page, "main.rs")).toBeVisible();

  // Back to the default, so the serial tests after this one find the pane as
  // they left it.
  await menu.locator("summary").click();
  await menu.getByText("What this card is based on").click();
  await expect(menu.locator("summary")).toContainText("All changes");
});

test("a range reaching from before a rebase to the live head is not offered", async ({ page }) => {
  await openCard(page, cardId);

  const review = page.locator("#review");
  const menu = review.locator("[data-range-menu]");
  const since = review.locator(".modes", { hasText: "Since this" });

  // Turn 2 is measured from turn 1, which the rebase left in the era before it —
  // so "since" would read the upstream delta as the card's own additions. Turn 1
  // measures from the base either way and keeps both.
  await menu.locator("summary").click();
  await menu.getByText("Turn 2", { exact: true }).click();
  await expect(menu.locator("summary")).toContainText("Turn 2");
  await expect(since.locator("span.mode")).toHaveAttribute("aria-disabled", "true");
  await expect(since.locator("a.mode", { hasText: "Since this" })).toHaveCount(0);

  // And asking for it by hand lands on the exact range beside it rather than on
  // a blank pane with no picker to escape from.
  await page.goto(`/cards/${cardId}?scope=since-turn-2`);
  await expect(menu.locator("summary")).toContainText("Turn 2");
  await expect(menu).not.toHaveAttribute("hidden", "");
  await expect(fileSection(page, "upstream.txt")).toHaveCount(0);

  await menu.locator("summary").click();
  await menu.getByText("What this card is based on").click();
  await expect(menu.locator("summary")).toContainText("All changes");
});

test("uncommitted work is not offered against a turn from before a rebase", async ({ page }) => {
  // Dirty the worktree: ordinarily that is exactly what puts the row on the
  // list, measured from the newest turn. That turn is now an era behind the
  // head, so there is nothing honest to measure and the row goes.
  editWorktree(cardId, "after-rebase.txt", "written after the rebase\n");

  await openCard(page, cardId);
  const review = page.locator("#review");
  const menu = review.locator("[data-range-menu]");

  await expect(fileSection(page, "after-rebase.txt")).toBeVisible();
  await menu.locator("summary").click();
  await expect(menu.locator(".menu-item.anchor-live")).toHaveCount(0);
  await menu.locator("summary").click();

  // Put the worktree back for the tests after this one, and take the redraw
  // rather than wait for one: the pane offers it, it does not apply it.
  removeInWorktree(cardId, "after-rebase.txt");
  await refreshDiff(page);
  await expect(fileSection(page, "after-rebase.txt")).toHaveCount(0);
});

test("re-pointing a live card aims the merge elsewhere, not the worktree", async ({ page }) => {
  const rooted = baseRef(cardId);
  const head = worktreeGit(cardId, "rev-parse", "HEAD");

  await openCard(page, cardId);
  const chip = page.locator(".branch-menu summary");
  await chip.click();
  await page.locator(".branch-menu").getByRole("button", { name: "release", exact: true }).click();
  await expect(chip).toContainText("release");

  // Renaming the target is not a rebase. The worktree keeps its root, and the
  // anchor the diff is measured from keeps its value — `release` is behind it,
  // and reconciliation only ever moves that forward.
  expect(worktreeGit(cardId, "rev-parse", "HEAD")).toBe(head);
  expect(baseRef(cardId)).toBe(rooted);
  await expect(fileSection(page, "upstream.txt")).toHaveCount(0);

  // What does move is where the work is asked to land.
  await expect(page.getByRole("button", { name: "Merge" })).toHaveAttribute("title", /release/);

  await chip.click();
  await page.locator(".branch-menu").getByRole("button", { name: "main", exact: true }).click();
  await expect(chip).toContainText("main");
  expect(baseRef(cardId)).toBe(rooted);
});

test("merging lands the work on the base branch and retires the card", async ({ page }) => {
  const before = git("rev-parse", "main");

  await openCard(page, cardId);
  await page.getByRole("button", { name: "Merge" }).click();

  await expect.poll(() => git("rev-parse", "main")).not.toBe(before);

  // main now carries the agent's work.
  expect(git("show", "main:main.rs")).toContain(TASK);
  expect(git("show", "main:main.rs")).toContain("Say hello instead.");

  await page.goto(projectUrl);
  await expect(cardIn(page, "done", TITLE)).toBeVisible({ timeout: SLOW });

  // The worktree is pruned, but the turn history is kept. Three turns, not two:
  // the rebase above pulled `upstream.txt` into the worktree, so the snapshot
  // that followed had a tree of its own rather than matching turn 2 and
  // returning `None`.
  // Polled: the merge answers as soon as the branch has moved, and the worktree
  // is pruned just behind it.
  await expect
    .poll(() => git("worktree", "list"), { timeout: SLOW })
    .not.toContain(`worktrees/${cardId}`);
  expect(turnRefs(cardId)).toHaveLength(3);
});

test("collecting the garbage reclaims what the finished card still held", async ({ page }) => {
  // The merge above left the turn refs and the scratch index behind, which is
  // what there is to reclaim.
  expect(cardRefs(cardId).length).toBeGreaterThan(0);
  expect(existsSync(cardDirOf(cardId))).toBe(true);

  await page.goto(projectUrl);
  page.on("dialog", (dialog) => dialog.accept());
  await page.locator(".lane-done .lane-gc").click();

  await expect(cardIn(page, "done", TITLE)).toHaveCount(0);

  // Every ref the card owned is gone — base and turns, not just the working one
  // a teardown drops — so nothing keeps its snapshots reachable any more.
  expect(cardRefs(cardId)).toHaveLength(0);
  expect(existsSync(cardDirOf(cardId))).toBe(false);
  expect(existsSync(worktreeOf(cardId))).toBe(false);
  expect(git("worktree", "list")).not.toContain(`worktrees/${cardId}`);

  // The merge itself is untouched: this collects the card, not its work.
  expect(git("show", "main:main.rs")).toContain(TASK);

  await page.reload();
  await expect(lane(page, "done").locator(".card")).toHaveCount(0);

  // The record survives what the disk lost: this route reads the card's own row
  // and still answers for it, lane and all.
  const state = await page.request.get(`/cards/${cardId}/state`);
  expect(state.status()).toBe(200);
  expect(await state.text()).toContain("stopped");

  // Its review cannot be reopened, though — the refs its turns name are gone, so
  // the drawer declines rather than trying to diff against nothing.
  expect((await page.request.get(`/cards/${cardId}`)).status()).toBe(404);

  // And a merge posted at it is turned away on its lane rather than rebuilding
  // the worktree this just reclaimed to look for commits that left with it.
  // Needing a running agent used to turn it away by itself. 404 rather than the
  // 409 a live card gets: a collected one has no page to answer with either.
  expect((await page.request.post(`/cards/${cardId}/merge`)).status()).toBe(404);
  expect(existsSync(worktreeOf(cardId))).toBe(false);
});

test("a merge already out refuses a change of base", async ({ page }) => {
  // Its own card, based on a branch the merge cannot reach. The agent lands its
  // work on whatever the repository has checked out, so `release` never moves,
  // the request is never satisfied, and the card stays in the state under test.
  await addCard(page, projectUrl, { title: "Stuck merge", base: "release" });
  await page.goto(projectUrl);
  const id = await cardIn(page, "todo", "Stuck merge").getAttribute("data-card-id");

  await moveCard(page, id, "in_progress", 0);
  await expect(cardIn(page, "in_review", "Stuck merge")).toBeVisible({ timeout: SLOW });

  // The request is recorded before the agent is told, so it is set by the time
  // this answers — and the branch it named is what will be watched for the work.
  expect((await page.request.post(`/cards/${id}/merge`)).status()).toBe(200);

  const refused = await page.request.post(`/cards/${id}/base`, { form: { base_branch: "main" } });
  expect(refused.status()).toBe(409);
  // The status carries the refusal; the body is the drawer saying why.
  const body = await refused.text();
  expect(body).toContain("already waiting to land on release");
  // Once, in the drawer's own bar. The review pane is included with the drawer's
  // context, so a shared key put every refusal in its footer as well, worded for
  // a bar it is not.
  expect(body.match(/already waiting to land on release/g)).toHaveLength(1);

  await openCard(page, id);
  await expect(page.locator(".branch-menu summary")).toContainText("release");
});
