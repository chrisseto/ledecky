import { expect, test } from "./support/fixtures.mjs";
import { SLOW } from "../playwright.config.mjs";

import {
  addCard,
  draftBox,
  addProject,
  cardIn,
  comment,
  fileSection,
  openAgent,
  turnRefs,
  showTab,
} from "./support/board.mjs";
import {
  pasteIntoTerminal,
  terminalInput,
  terminalRows,
  terminalScreen,
} from "./support/dom.mjs";

// Reading the clipboard back is a permission in Chromium. Granting it to the page
// is also what gives the refusal below its meaning: the pane declines the agent's
// read while the page around it is allowed one and takes it.
test.use({ permissions: ["clipboard-read", "clipboard-write"] });

const TITLE = "Clicks and pastes";

let projectUrl;
let cardId;

test.beforeAll(async ({ browser }) => {
  const page = await browser.newPage();
  projectUrl = await addProject(page);
  await addCard(page, projectUrl, { title: TITLE, description: "Take the mouse.", base: "main" });
  cardId = await cardIn(page, "todo", TITLE).getAttribute("data-card-id");

  // SLOW: an agent start.
  await cardIn(page, "todo", TITLE).dragTo(page.locator('[data-lane="in_progress"]'));
  await expect.poll(() => turnRefs(cardId).length, { timeout: SLOW }).toBe(1);
  await page.close();
});

/**
 * A card of its own, deliberately: mouse reporting turns the wheel into
 * something the agent reads, so a pane with it on is a pane that no longer
 * scrolls — and `lifecycle.spec.mjs` scrolls this one's sibling to prove the
 * server replayed its scrollback.
 *
 * xterm.js reports the default encoding over `onBinary` and only SGR over
 * `onData`. A pane wiring `onData` alone drops every click the agent asked for
 * and says nothing about it, which is what this catches.
 */
test("a click reaches the pty once the agent asks for mouse reports", async ({ page }) => {
  await openAgent(page, cardId);

  const rows = terminalRows(page);
  await expect(rows).toContainText(`worktrees/${cardId}`);

  await terminalInput(page).press("m");
  await expect(rows).toContainText("mouse on");

  // One pixel inside the grid's own top-left is cell 1,1 whatever the pane
  // measured, so the report is exact rather than approximately somewhere.
  const box = await terminalScreen(page).boundingBox();
  await page.mouse.click(box.x + 1, box.y + 1);

  // xterm's own button codes: 0 is the left button going down, and 3 is a
  // release, which this encoding cannot put a button on.
  await expect(rows).toContainText("mouse 0 at 1,1");
  await expect(rows).toContainText("mouse 3 at 1,1");

  // And the wheel with it, which is the reason the mode is behind a key: 64 is
  // the wheel bit, 65 one turn down.
  await page.mouse.wheel(0, 200);
  await expect(rows).toContainText("mouse 65 at 1,1");
});

test("a paste arrives bracketed, so the client takes it as one thing", async ({ page }) => {
  await openAgent(page, cardId);
  await expect(terminalRows(page)).toContainText(`worktrees/${cardId}`);

  // Three lines and past the client's 200-character threshold. Typed character
  // by character the composer would read it back in full, so the collapsed form
  // is the proof that both markers arrived with the whole payload between them.
  const pasted = ["first line", "x".repeat(220), "third line"].join("\n");
  await pasteIntoTerminal(page, pasted);

  await expect(terminalRows(page)).toContainText("[Pasted text #1 +3 lines]");
});

/**
 * Whatever the input box holds goes out with the next submit key, and the box
 * cannot say what it holds — a multi-line paste collapses to `[Pasted text #1
 * +N lines]`, which names nobody. So the server will only write into an empty
 * one. The check that instead waited to see *its own* message appear answered
 * to that same placeholder, and submitted somebody's abandoned paste as the
 * review.
 *
 * A card of its own: what is set up here is a box holding exactly one abandoned
 * paste, and the sibling above leaves one of its own in the shared card's.
 */
test("a send will not write into a box that already holds something", async ({ page }) => {
  const title = "Something already in the box";
  await addCard(page, projectUrl, { title, description: "Take the mouse.", base: "main" });
  const id = await cardIn(page, "todo", title).getAttribute("data-card-id");

  // SLOW: an agent start.
  await cardIn(page, "todo", title).dragTo(page.locator('[data-lane="in_progress"]'));
  await expect.poll(() => turnRefs(id).length, { timeout: SLOW }).toBe(1);

  await openAgent(page, id);
  await expect(terminalRows(page)).toContainText(`worktrees/${id}`);

  // Past the 200-character threshold, so it collapses to the placeholder — the
  // form the server cannot tell its own paste apart from.
  await pasteIntoTerminal(page, ["never asked for this", "y".repeat(220)].join("\n"));
  await expect(terminalRows(page)).toContainText("Pasted text");

  await showTab(page, "review");
  const line = fileSection(page, "main.rs").locator(".line.l-added").first();
  await comment(page, line, "Say hello instead.");

  const [refused] = await Promise.all([
    page.waitForResponse((res) => res.url().endsWith(`/cards/${id}/review`)),
    page.getByRole("button", { name: /Send \d+ to agent/ }).click(),
  ]);
  expect(refused.status()).toBe(409);
  await expect(page.locator("#review .review-note")).toContainText("unsent text");
  await expect(page.locator("#review .batch-label")).toContainText("1 comment pending");
  await expect(draftBox(line)).toHaveValue("Say hello instead.");
  expect(turnRefs(id)).toHaveLength(1);

  // Where this stops: that the batch goes once the box is free is
  // `agent.rs`'s to prove, against a pty it paints itself. Clearing the box
  // from here means driving xterm's own input, which delivers more than the
  // keystroke asked for and leaves the box holding it.
});

test("the agent can put something on the clipboard, and cannot read it back", async ({ page }) => {
  await openAgent(page, cardId);

  const rows = terminalRows(page);
  await expect(rows).toContainText(`worktrees/${cardId}`);

  await terminalInput(page).press("y");

  await expect
    .poll(() => page.evaluate(() => navigator.clipboard.readText().catch(() => "")))
    .toBe("copied out of the agent");

  // The read does not land: OSC 52 with a `?` is answered with nothing, so no
  // amount of agent output talks the pane into handing over whatever the user
  // last copied. The empty answer is the pane's rather than the browser's, since
  // this page is permitted to read the clipboard and just did.
  await expect(rows).toContainText('clipboard ""');
});
