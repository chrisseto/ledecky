/**
 * What a change costs, and who pays for it.
 *
 * These are counting tests rather than rendering ones: the thing under test is
 * that a change reaches exactly the fragments it concerns, and that reading the
 * board never makes the server stage a worktree. Both are properties nothing on
 * the screen shows, and both are easy to lose to a well-meaning refactor.
 */
import { expect, test } from "./support/fixtures.mjs";

import {
  addCard,
  addProject,
  cardIn,
  editWorktree,
  moveCard,
  openAgent,
  openCard,
  pollsOfPath,
} from "./support/board.mjs";

test.describe.configure({ mode: "serial" });

const FIRST = "Count the first card";
const SECOND = "Count the second card";

let projectUrl;
let first;
let second;

/** The card's `+x −y` chip on the board, in whichever lane it has reached. */
const stat = (page, cardId) => page.locator(`[data-card-id="${cardId}"] .stat`);

/**
 * How many lines the chip says the card has added.
 *
 * A number rather than the chip's text because the agent has done work of its
 * own before any of this runs, and what these tests are about is the chip
 * *moving* by exactly what was just written.
 */
const additions = async (page, cardId) =>
  Number((await stat(page, cardId).textContent()).match(/\+(\d+)/)?.[1]);

/**
 * The same number, read over plain HTTP.
 *
 * Rendering the board is a GET like any other; what a browser adds is the
 * `EventSource`. Asking this way is how a test can watch the board while
 * leaving the server with nothing listening.
 */
const additionsUnwatched = async (page, cardId) => {
  const html = await (await page.request.get(projectUrl)).text();
  const card = html.split(`data-card-id="${cardId}"`)[1]?.split("</article>")[0] ?? "";
  return Number(card.match(/\+(\d+)/)?.[1]);
};

test.beforeAll(async ({ browser }) => {
  const page = await browser.newPage();
  projectUrl = await addProject(page);

  for (const title of [FIRST, SECOND]) {
    await addCard(page, projectUrl, { title, description: `${title}.` });
  }
  first = await cardIn(page, "todo", FIRST).getAttribute("data-card-id");
  second = await cardIn(page, "todo", SECOND).getAttribute("data-card-id");

  // Both need a worktree, which is what entering In Progress makes.
  await moveCard(page, first, "in_progress");
  await moveCard(page, second, "in_progress");
  await page.close();
});

test("the board's counts follow a worktree with no drawer open", async ({ page }) => {
  await page.goto(projectUrl);
  await expect(stat(page, first)).toBeVisible();
  const before = await additions(page, first);

  // Nothing on this page reads a diff. The board renders from what is already
  // memoised and never stages a worktree itself, so the number moving is the
  // watcher having staged it and then said so — not this page going to look.
  editWorktree(first, "counted.txt", "one\ntwo\nthree\n");
  await expect.poll(() => additions(page, first)).toBe(before + 3);

  editWorktree(first, "counted.txt", "one\ntwo\nthree\nfour\n");
  await expect.poll(() => additions(page, first)).toBe(before + 4);
});

test("a write in one worktree leaves another card's open pane alone", async ({ page }) => {
  const mine = pollsOfPath(page, `/cards/${first}/diff`);

  await openCard(page, first);
  await expect(page.locator("#review")).toBeVisible();

  // The stream opens with a resync, which is one fetch of this pane. Waiting
  // for it rather than racing it is what makes the counts below mean anything.
  await expect.poll(() => mine.length).toBe(1);
  const settled = mine.length;

  // The other card's worktree moving is not this pane's business. Both changes
  // arrive on the same stream; only the one naming this card is listened for.
  const elsewhere = await additions(page, second);
  editWorktree(second, "elsewhere.txt", "not about the open card\n");
  await expect.poll(() => additions(page, second)).toBe(elsewhere + 1);
  expect(mine).toHaveLength(settled);

  // And its own still reaches it, so the filter is not simply deaf.
  editWorktree(first, "mine.txt", "about the open card\n");
  await expect(page.locator(`#review .file[data-path="mine.txt"]`)).toBeVisible();
  expect(mine).toHaveLength(settled + 1);
});

test("the agent pane asks for itself rather than the whole card", async ({ page }) => {
  const whole = pollsOfPath(page, `/cards/${second}`);
  const pane = pollsOfPath(page, `/cards/${second}/agent`);

  // On the Agent tab, because the drawer lands on Review once there is
  // something to review and a hidden pane is not what this is measuring.
  await openAgent(page, second);

  // Same resync as above, and the same reason for waiting it out.
  await expect.poll(() => pane.length).toBe(1);
  const navigated = whole.length;
  const drawn = pane.length;

  // Stopping is a state change, which is what the pane listens for. It costs
  // one fetch of the pane and none of the page behind it — that page is a board
  // and a review pane, and it used to be rendered in full to pick this section
  // out of.
  await page.request.post(`/cards/${second}/stop`);
  await expect(page.locator(".pane-agent .empty")).toBeVisible();

  expect(pane.length).toBeGreaterThan(drawn);
  expect(whole).toHaveLength(navigated);
});

test("a worktree that moved while nothing was watching is still counted", async ({
  page,
  browser,
}) => {
  // Read the chip with a board up, so there is an exact number to move from,
  // and then take that board away again.
  const watching = await browser.newPage();
  await watching.goto(projectUrl);
  await expect(stat(watching, first)).toBeVisible();
  const before = await additions(watching, first);
  await watching.close();

  // Closing the tab is not enough to stop the server listening: a parked
  // stream is waiting on the bus, and only finds out its client has gone when
  // it next tries to write. So this write is the one that retires it — the
  // announce fails and the receiver drops — and it still gets staged. Read the
  // result over plain HTTP, which opens no stream of its own, so that the
  // board can be watched without being connected to.
  editWorktree(first, "retires-the-stream.txt", "still listening\n");
  await expect.poll(() => additionsUnwatched(page, first)).toBe(before + 1);

  // *This* one lands with nobody listening, so the watcher forgets the head
  // rather than staging it — there is no reader to be ahead of. Arriving
  // afterwards has to be enough to produce one, and what makes that awkward is
  // that the watch outlived the connection that made it: "already watched" is
  // not "already staged", so a connect cannot decide by that alone.
  editWorktree(first, "unwatched.txt", "one\ntwo\nthree\nfour\nfive\n");

  await page.goto(projectUrl);
  await expect.poll(() => additions(page, first)).toBe(before + 6);
});
