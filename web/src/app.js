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

// ---- branch picker ----------------------------------------------------------
// The whole branch list is already on the page, so narrowing it is a filter over
// the rendered options rather than a round trip.

const filterBranches = (input) => {
  const menu = input.parentElement.querySelector(".combo-menu");
  if (!menu) return;

  const wanted = input.value.trim().toLowerCase();
  let shown = 0;

  for (const option of menu.querySelectorAll("[data-branch]")) {
    const matches = option.textContent.trim().toLowerCase().includes(wanted);
    option.hidden = !matches;
    shown += matches ? 1 : 0;
  }

  const noMatch = menu.querySelector(".empty-match");
  if (noMatch) noMatch.hidden = shown > 0;
  menu.hidden = false;
};

const onBranchInput = (event) => {
  const input = event.target.closest?.("[data-branch-filter]");
  if (input) filterBranches(input);
};

document.addEventListener("input", onBranchInput);
document.addEventListener("focusin", onBranchInput);

document.addEventListener("focusout", (event) => {
  const input = event.target.closest?.("[data-branch-filter]");
  if (!input) return;

  const menu = input.parentElement.querySelector(".combo-menu");
  // Late enough for a click on an option to land first.
  setTimeout(() => {
    if (menu) menu.hidden = true;
  }, 120);
});

document.addEventListener("click", (event) => {
  const option = event.target.closest?.(".combo-menu [data-branch]");
  if (!option) return;

  const input = option.closest(".combo")?.querySelector("[data-branch-filter]");
  if (!input) return;

  input.value = option.textContent.trim();
  option.closest(".combo-menu").hidden = true;
});

// ---- review comments --------------------------------------------------------
// Which line is being commented on lives in the pane's query string, so the box
// arrives from the server already in place. Clicking away is what saves it as a
// draft; an empty box was a change of mind.

// NB: one listener for every line, rather than an `hx-get` on each. The pane
// holds every line of every file at once, and htmx processes each element it
// finds — so per-line attributes cost the whole diff's length on every comment,
// expansion, tick and update, and put four attributes per line on the wire.
// Issued from `.lines` so it joins the pane's own request queue.
//
// NB: delegated from `document` like the rest of this file rather than an
// `hx-on:click` on the pane. That attribute is evaluated in global scope, so it
// would need this body inlined in the template or a function hung on `window`.
document.addEventListener("click", (event) => {
  const line = event.target.closest?.("#review .line");
  if (!line) return;

  const review = line.closest(".review");
  const key = `${line.dataset.file}#${line.dataset.anchor}`;
  // Clicking the line whose box is open is how it closes again: the same view
  // with one query parameter dropped.
  const base = review.dataset.commentBase;
  const url = line.classList.contains("commenting")
    ? base
    : `${base}&comment=${encodeURIComponent(key)}`;

  htmx.ajax("GET", url, {
    target: "#review",
    swap: "outerMorph",
    source: line.closest(".lines"),
  });
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
