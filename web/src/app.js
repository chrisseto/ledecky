// NB: htmx's ESM build assigns `window.htmx` as a side effect and the
// extensions are plain scripts registering against that global, so htmx has to
// be imported first and read off the window afterwards.
import "htmx.org";
import "htmx.org/dist/ext/hx-sse.js";

import { DrawerCard } from "./components/drawer-card.js";
import { FileTree } from "./components/file-tree.js";
import { SortableLane } from "./components/sortable-lane.js";
import { TerminalPane } from "./components/terminal.js";

const { htmx } = window;

// Everything that owns something a swap must not leak — a socket, an observer,
// a Sortable instance — is a custom element, so the browser's own
// `disconnectedCallback` is what tears it down. Registered here rather than
// beside each class so the tag names read as one list.
customElements.define("x-drawer-card", DrawerCard);
customElements.define("x-file-tree", FileTree);
customElements.define("x-sortable-lane", SortableLane);
customElements.define("x-terminal", TerminalPane);

// ---- overlays ---------------------------------------------------------------
// Drawers and modals are server-rendered into #overlay, so closing one is a
// navigation like any other: Escape follows the same link the scrim carries.

document.addEventListener("keydown", (event) => {
  if (event.key !== "Escape") return;

  const cancel = document.querySelector("#review [data-cancel-comment]");
  if (cancel) {
    discardComment(cancel);
    return;
  }

  document.querySelector("#overlay [data-close-overlay]")?.click();
});

// ⌘↵ submits a form; adding shift takes the second button, which keeps the form
// open for the next one.
document.addEventListener("keydown", (event) => {
  if (event.key !== "Enter" || !(event.metaKey || event.ctrlKey)) return;

  const form = event.target.closest?.("[data-submit-shortcuts]");
  if (!form) return;
  event.preventDefault();

  const buttons = form.querySelectorAll('button[type="submit"]');
  (event.shiftKey ? buttons[0] : buttons[buttons.length - 1]).click();
});

// ---- server-rendered autocomplete -------------------------------------------
// The input names its own target and endpoint; every keystroke re-renders that
// fragment from the server. No client-side filtering or matching logic.

let completeTimer;

const completions = (input) => {
  clearTimeout(completeTimer);
  completeTimer = setTimeout(() => {
    const { completeFor: target, completeUrl: url } = input.dataset;
    htmx.ajax("GET", `${url}?q=${encodeURIComponent(input.value)}`, {
      target,
      swap: "outerHTML",
    });
  }, 120);
};

const onCompleteInput = (event) => {
  const input = event.target.closest?.("[data-complete-for]");
  if (input) completions(input);
};

document.addEventListener("input", onCompleteInput);
// `focus` does not bubble; `focusin` is the delegable form of it.
document.addEventListener("focusin", onCompleteInput);

document.addEventListener("click", (event) => {
  const button = event.target.closest?.(".completions button[data-path]");
  if (!button) return;

  event.preventDefault();
  const input = document.querySelector("[data-complete-for]");
  if (!input) return;

  input.value = button.dataset.path;
  input.focus();
  input.dispatchEvent(new Event("input", { bubbles: true }));
});

// ---- review comments --------------------------------------------------------
// A box and the comments under it are one block against one line, asked for and
// swapped on its own. Clicking away is what saves it as a draft; an empty box
// was a change of mind. Which line is open is also in the pane's own fetch URL,
// so the stream's resync brings it back rather than morphing it away.

// NB: one listener for every line, rather than an `hx-get` on each. Only the
// action attributes make htmx initialise an element, and `hx-get` is one — so
// per-line it is the whole diff's length in attributes and in elements to wire
// up on every swap.
//
// NB: this could be an `hx-on:click` on `.lines`, which gets `event`, `this`
// and htmx's own API — it is not global scope. It stays here because the body
// is long enough that an attribute would hide the two sweeps in it.
document.addEventListener("click", (event) => {
  const line = event.target.closest?.("#review .line");
  if (!line) return;

  // The path is the section's, not the line's: repeating it on every line is
  // the diff's length in attributes for something the enclosing file already
  // says. A line outside one has no anchor to build, so there is nothing to ask
  // for — better than posting a comment against the path `undefined`.
  const file = line.closest("[data-path]");
  if (!file) return;

  const review = line.closest(".review");
  const key = `${file.dataset.path}#${line.dataset.anchor}`;

  // A line's comments and its open box are one block, rendered only for the
  // lines that have either — so it is there to swap or it is not there yet.
  const block = line.nextElementSibling?.matches(".anchored")
    ? line.nextElementSibling
    : null;
  // Clicking the line whose box is open is how it closes again.
  const open = !block?.querySelector(".compose");

  const url =
    `/cards/${review.dataset.card}/comments/at` +
    `?key=${encodeURIComponent(key)}` +
    `&scope=${encodeURIComponent(review.dataset.scope)}` +
    `&expand=${encodeURIComponent(review.dataset.expand)}` +
    (open ? "&open=true" : "");

  // The highlight used to come with the server's copy of the line. The line is
  // not being redrawn any more, so it is set here.
  for (const lit of document.querySelectorAll("#review .line.commenting")) {
    lit.classList.remove("commenting");
  }
  line.classList.toggle("commenting", open);

  htmx.ajax("GET", url, block ? { target: block, swap: "outerHTML" } : { target: line, swap: "afterend" });
});

// NB: closing the box blurs it either way, and a blur is what saves — so both
// ways out have to say which they are. A mouse announces itself by pressing
// Cancel; Escape has to say so on its own.
let discarding = false;

document.addEventListener("mousedown", (event) => {
  discarding = !!event.target.closest?.("[data-cancel-comment]");
});

/** Throws the box away: marks the blur that follows, then closes it. */
function discardComment(cancel) {
  discarding = true;
  cancel.click();
}

document.addEventListener("focusout", (event) => {
  const textarea = event.target.closest?.(".compose textarea");
  if (!textarea) return;

  // One blur per way out, so the mark cannot outlive what set it.
  if (discarding) {
    discarding = false;
    return;
  }

  const form = textarea.closest("form");
  if (textarea.value.trim()) form.requestSubmit();
  else form.querySelector("[data-cancel-comment]")?.click();
});

// The box is server-rendered, so it has to be focused once it arrives.
document.addEventListener("htmx:after:settle", () => {
  const textarea = document.querySelector(".compose textarea[data-autofocus]");
  if (textarea && document.activeElement !== textarea) textarea.focus();
});
