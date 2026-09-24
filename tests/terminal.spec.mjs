import { expect, test } from "./support/fixtures.mjs";
import { SLOW } from "../playwright.config.mjs";

import { addCard, addProject, cardIn, openAgent, turnRefs } from "./support/board.mjs";
import { terminalInput, terminalRows, terminalScreen } from "./support/dom.mjs";

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

  // Built in the page rather than with `dispatchEvent`, which knows nothing about
  // `paste` and would hand xterm a plain `Event` carrying no `clipboardData` —
  // and built on the textarea, because xterm's own handler stops the event, so
  // this fires it exactly once. Chromium's Ctrl+V is not an option: Playwright
  // sends the keys without the editing command behind them, so nothing pastes.
  await terminalInput(page).evaluate((textarea, text) => {
    const data = new DataTransfer();
    data.setData("text/plain", text);
    textarea.dispatchEvent(
      new ClipboardEvent("paste", { bubbles: true, cancelable: true, clipboardData: data }),
    );
  }, pasted);

  await expect(terminalRows(page)).toContainText("[Pasted text #1 +3 lines]");
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
