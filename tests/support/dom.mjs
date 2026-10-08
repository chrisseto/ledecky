/**
 * Locators for markup the specs do not own.
 *
 * xterm.js renders its own DOM, so `.xterm-rows` and `.xterm-helper-textarea`
 * are a vendor's class names rather than ours. Naming them once here keeps an
 * upgrade to a single edit, and keeps the specs reading in terms of the thing
 * on screen. Anchored on `data-terminal`, which the drawer already emits, so
 * they cannot pick up a second terminal elsewhere on the page.
 */

/** The terminal pane for the open card. */
export const terminal = (page) => page.locator("[data-terminal]");

/** The rows xterm has painted — what the agent's screen currently says. */
export const terminalRows = (page) => terminal(page).locator(".xterm-rows");

/** Where a keystroke goes; xterm reads from this rather than the rows. */
export const terminalInput = (page) => terminal(page).locator(".xterm-helper-textarea");

/** The grid xterm reports mouse positions against: its own box, not the pane's. */
export const terminalScreen = (page) => terminal(page).locator(".xterm-screen");

/**
 * Pastes `text` into the terminal pane, as a person would.
 *
 * Built in the page rather than with `dispatchEvent`, which knows nothing about
 * `paste` and would hand xterm a plain `Event` carrying no `clipboardData` —
 * and built on the textarea, because xterm's own handler stops the event, so
 * this fires it exactly once. Chromium's Ctrl+V is not an option: Playwright
 * sends the keys without the editing command behind them, so nothing pastes.
 */
export async function pasteIntoTerminal(page, text) {
  await terminalInput(page).evaluate((textarea, pasted) => {
    const data = new DataTransfer();
    data.setData("text/plain", pasted);
    textarea.dispatchEvent(
      new ClipboardEvent("paste", { bubbles: true, cancelable: true, clipboardData: data }),
    );
  }, text);
}
