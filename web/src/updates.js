/**
 * The page is redrawn by the server announcing changes, never by a timer.
 *
 * Fragments listen for a named event on the board's one `EventSource`; firing
 * the same event by hand is how the client asks for a redraw it already knows
 * it needs.
 */
export const refreshBoard = () => window.htmx.trigger(document.body, "board");
