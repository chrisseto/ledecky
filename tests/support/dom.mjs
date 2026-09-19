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
export const terminal = (page) => page.locator("div[data-terminal]");

/** The rows xterm has painted — what the agent's screen currently says. */
export const terminalRows = (page) => terminal(page).locator(".xterm-rows");

/** Where a keystroke goes; xterm reads from this rather than the rows. */
export const terminalInput = (page) => terminal(page).locator(".xterm-helper-textarea");
