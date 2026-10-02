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

// ---- the base picker --------------------------------------------------------
// The rows come from the server and commit themselves. These are the two
// gestures no row can be: typing a revision git knows and pressing Enter, and
// stepping into a remote.

// A remote is somewhere to look rather than a base to pick, so taking one types
// its prefix into the search box and lets that re-render the list.
//
// NB: the same gesture as `.completions button[data-path]` above, and kept
// apart from it because the two fragments find their input differently — the
// picker can be on the page twice over, so it has to be the nearest one.
document.addEventListener("click", (event) => {
  const button = event.target.closest?.(".branch-options button[data-fill]");
  if (!button) return;

  event.preventDefault();
  const search = button.closest(".branch-menu").querySelector(".search");
  search.value = button.dataset.fill;
  search.focus();
  search.dispatchEvent(new Event("input", { bubbles: true }));
});

/** What the form currently holds, which a refused commit has to keep. */
const picked = (search) => search.closest(".branch-menu").querySelector("#base-branch").value;

// Enter commits whatever has been typed — a pasted sha above all, which is a
// revision nobody wants to go hunting for in a list.
//
// NB: the typed value, not the row the server offered for it. That row arrives
// 120ms after the last keystroke, and paste-then-Enter is faster than that.
// Sending the text means the server is still the only thing that decides
// whether it resolves.
document.addEventListener("keydown", (event) => {
  if (event.key !== "Enter") return;

  const search = event.target.closest?.(".branch-menu .search");
  if (!search) return;
  // Swallowed either way: the box is in a form on the card page, and Enter in
  // it is a commit rather than a submit.
  event.preventDefault();

  const base_branch = search.value.trim();
  if (!base_branch) return;
  const url = search.dataset.commitUrl;

  // A card takes the new base as a write and answers with the whole page; a
  // refusal is that page with the reason on it. The form has nothing to write
  // to yet, so it asks the picker back with the value as the base — and gets
  // the old one back, with the status line saying why, if git disagrees.
  if (search.hasAttribute("data-commit-post")) {
    htmx.ajax("POST", url, {
      values: { base_branch },
      target: "body",
      swap: "outerMorph",
      // NB: the string, not the boolean. htmx normalizes `'true'` to the URL
      // the response came back from; anything else it pushes verbatim, and a
      // bare `true` lands the address bar on `/cards/true`.
      source: search,
      push: "true",
    });
  } else {
    htmx.ajax("GET", url, {
      values: { base_branch, chosen: picked(search) },
      target: "#branch-picker",
      swap: "outerHTML",
      source: search,
    });
  }
});

// ---- review comments --------------------------------------------------------

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

  // The server renders this block only for a line that has comments or a box.
  // The block is therefore present, or it does not exist yet.
  const block = line.nextElementSibling?.matches(".anchored")
    ? line.nextElementSibling
    : null;

  // NB: a box that contains text keeps the click. A request would replace the
  // block, and the new box holds the text that the server has — which is up to
  // one throttle interval behind the box on screen.
  const box = block?.querySelector(".compose textarea");
  if (box?.value.trim()) {
    box.focus();
    return;
  }
  // A click on a line that has an empty box closes that box.
  const open = !box;

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

// A field the server drew has to be focused once it arrives: the base search
// when a refusal reopened the menu around it.
document.addEventListener("htmx:after:settle", () => {
  const field = document.querySelector(".branch-menu[open] .search[data-autofocus]");
  if (!field) return;
  if (document.activeElement !== field) field.focus();

  // NB: the search box outlives the response that asked for it — the menu
  // stays open and re-filters in place — so the ask has to be spent, or every
  // later settle drags the cursor back out of whatever it moved to.
  field.removeAttribute("data-autofocus");
});
