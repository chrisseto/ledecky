// NB: unpoly ships a CommonJS bundle that only assigns `window.up` — it has no
// usable default export, so this is an import for the side effect. Reading the
// global is the supported way to reach the API.
import "unpoly";
import Sortable from "sortablejs";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";

const { up } = window;

// ---- overlays ---------------------------------------------------------------
// Drawers and modals are server-rendered into #overlay, so closing one is a
// navigation like any other: Escape follows the same link the scrim carries.

up.on("keydown", (event) => {
  if (event.key !== "Escape") return;

  const compose = document.querySelector(".compose:not([hidden])");
  if (compose) {
    compose.dispatchEvent(new CustomEvent("cancel-comment", { bubbles: true }));
    return;
  }

  document.querySelector("#overlay [data-close-overlay]")?.click();
});

// ⌘↵ submits a form; adding shift takes the second button, which keeps the form
// open for the next one.
up.compiler("[data-submit-shortcuts]", (form) => {
  const submit = (event) => {
    if (event.key !== "Enter" || !(event.metaKey || event.ctrlKey)) return;
    event.preventDefault();

    const buttons = form.querySelectorAll('button[type="submit"]');
    (event.shiftKey ? buttons[0] : buttons[buttons.length - 1]).click();
  };

  form.addEventListener("keydown", submit);
});

// ---- server-rendered autocomplete -------------------------------------------
// The input names its own target and endpoint; every keystroke re-renders that
// fragment from the server. No client-side filtering or matching logic.

up.compiler("[data-complete-for]", (input) => {
  const { completeFor: target, completeUrl: url } = input.dataset;
  let timer;

  const refresh = () => {
    clearTimeout(timer);
    timer = setTimeout(() => {
      up.render({
        target,
        url: `${url}?q=${encodeURIComponent(input.value)}`,
        cache: false,
      });
    }, 120);
  };

  input.addEventListener("input", refresh);
  input.addEventListener("focus", refresh);
});

up.on("click", ".completions button[data-path]", (event, button) => {
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

up.compiler("[data-branch-filter]", (input) => {
  const menu = input.parentElement.querySelector(".combo-menu");
  const options = [...menu.querySelectorAll("[data-branch]")];
  const noMatch = menu.querySelector(".empty-match");

  const filter = () => {
    const wanted = input.value.trim().toLowerCase();
    let shown = 0;

    for (const option of options) {
      const matches = option.textContent.trim().toLowerCase().includes(wanted);
      option.hidden = !matches;
      shown += matches ? 1 : 0;
    }

    noMatch.hidden = shown > 0;
    menu.hidden = false;
  };

  input.addEventListener("focus", filter);
  input.addEventListener("input", filter);
  // Late enough for a click on an option to land first.
  input.addEventListener("blur", () => setTimeout(() => { menu.hidden = true; }, 120));

  menu.addEventListener("click", (event) => {
    const option = event.target.closest("[data-branch]");
    if (!option) return;

    input.value = option.textContent.trim();
    menu.hidden = true;
  });
});

// ---- kanban drag and drop ---------------------------------------------------
// SortableJS moves the DOM node; the server is told the destination lane and the
// drop index, and owns the ordering from there.

up.compiler("[data-sortable]", (lane) => {
  const board = lane.closest("[up-poll]");

  const sortable = Sortable.create(lane, {
    group: "cards",
    animation: 120,
    draggable: ".card",
    ghostClass: "card-ghost",

    // Polling mid-drag would yank the card out from under the cursor.
    onStart: () => board && up.radio.stopPolling(board),

    onEnd: async (event) => {
      const id = event.item.dataset.cardId;
      try {
        await up.request(`/cards/${id}/move`, {
          method: "post",
          params: { lane: event.to.dataset.lane, index: event.newIndex },
        });
      } finally {
        if (board) {
          up.radio.startPolling(board);
          // This reload reconciles the optimistic move above with server truth,
          // so it must not be answered with a 304 — a failed move would leave
          // the card sitting in the wrong lane. The swap brings a fresh etag.
          board.removeAttribute("up-etag");
          up.reload(board);
        }
      }
    },
  });

  return () => sortable.destroy();
});

// ---- terminal ---------------------------------------------------------------
// The socket carries raw pty bytes in both directions. Resize goes over HTTP so
// the socket never needs a message envelope.

up.compiler("[data-terminal]", (host) => {
  const term = new Terminal({
    convertEol: false,
    cursorBlink: true,
    fontFamily: getComputedStyle(document.documentElement).getPropertyValue("--mono").trim(),
    fontSize: 13,
    scrollback: 5000,
    // The ground and text of the pane it sits in, which xterm needs as hex.
    theme: { background: "#0f1318", foreground: "#d5d0c8" },
  });

  const fit = new FitAddon();
  term.loadAddon(fit);
  term.open(host);

  // xterm cancels a wheel only when it actually scrolled the viewport with it; at
  // either end of the scrollback it lets the event through and the page behind the
  // drawer scrolls instead. Nothing in this pane should ever move the board.
  host.addEventListener("wheel", (event) => event.preventDefault(), { passive: false });

  let sent = "";

  // NB: the review tab hides this pane, which then measures 0x0. Fitting to
  // that would reflow the agent's screen into a single cell — for nobody, and
  // destructively: the pty is the agent's real terminal. Measuring only while
  // the pane is on screen leaves the last good size in place until it is back.
  const resize = () => {
    if (!host.clientWidth || !host.clientHeight) return;

    fit.fit();
    const { rows, cols } = term;
    const key = `${rows}x${cols}`;
    if (key === sent) return;

    sent = key;
    up.request(host.dataset.resizeUrl, { method: "post", params: { rows, cols } });
  };

  // Fitting before the socket opens is what lets `rows` below be this screen's.
  resize();

  const url = new URL(host.dataset.terminal, location.href);
  url.protocol = location.protocol === "https:" ? "wss:" : "ws:";
  // The scrollback the server replays has to be scrolled down by a full screen
  // before it repaints over it, and that screen is ours, not the pty's — a
  // taller client would otherwise never see the newest history.
  url.searchParams.set("rows", term.rows);

  const socket = new WebSocket(url);
  socket.binaryType = "arraybuffer";

  socket.addEventListener("open", () => resize());
  socket.addEventListener("message", (event) => {
    term.write(new Uint8Array(event.data));
  });
  socket.addEventListener("close", () => {
    term.write("\r\n\x1b[2m-- agent disconnected --\x1b[0m\r\n");
  });

  const encoder = new TextEncoder();
  term.onData((data) => {
    if (socket.readyState === WebSocket.OPEN) socket.send(encoder.encode(data));
  });

  let debounce;
  const observer = new ResizeObserver(() => {
    clearTimeout(debounce);
    debounce = setTimeout(resize, 80);
  });
  observer.observe(host);

  return () => {
    observer.disconnect();
    socket.close();
    term.dispose();
  };
});

// ---- review pane ------------------------------------------------------------

/**
 * Holds the pane still while the reader is part-way through something.
 *
 * Stopping the timer is not enough on its own: a poll issued moments earlier is
 * still on its way, and its response would swap away the comment box being
 * typed into or the menu being read.
 *
 * NB: the *request* is aborted, not the fragment. `up.fragment.abort` would
 * also cancel whatever the reader goes on to click, since that request is bound
 * to this fragment too — which reads as a dropdown that does nothing.
 */
const holdPane = (review) => {
  up.radio.stopPolling(review);
  up.network.abort((request) => request.background);
};

/**
 * Polls only while the pane is the tab on screen and nobody is mid-sentence.
 *
 * Both panes are always in the DOM so that switching tabs never tears down the
 * terminal, and unpoly polls whatever carries `up-poll` whether or not it is
 * visible — so without this the agent's tab would stage the worktree every few
 * seconds for a diff nobody is looking at.
 */
up.compiler(".review", (review) => {
  const tabs = document.querySelector(".drawer-card .tabs");
  if (!tabs) return;

  const follow = () => {
    const showing = tabs.querySelector("#tab-review")?.checked;
    const busy = review.querySelector(".compose:not([hidden]), [data-range-menu][open]");

    if (showing && !busy) up.radio.startPolling(review);
    else up.radio.stopPolling(review);
  };

  follow();
  tabs.addEventListener("change", follow);
  // NB: the tabs outlive this fragment, so the listener has to come off with
  // it — otherwise every poll leaves another one behind.
  return () => tabs.removeEventListener("change", follow);
});

/** The reader is part-way through something the pane must not move under. */
const paneBusy = () =>
  !!document.querySelector("#review .compose:not([hidden]), #review [data-range-menu][open]");

/** Which range a request asked for. Only the pane's own polls name one. */
const rangeOf = (url) => new URL(url, location.href).searchParams.get("scope");

// The last word on whether a poll gets to redraw the pane, and the only one that
// closes the window completely: stopping the timer and aborting in flight both
// happen before a response exists, so neither can stop one already downloaded
// and on its way to being rendered — which reads as the picker closing itself
// mid-choice, or a range change that does nothing.
const skipResponse = up.fragment.config.skipResponse;
up.fragment.config.skipResponse = (props) => {
  if (skipResponse(props)) return true;
  if (!props.request.background) return false;

  // NB: scoped to the pane's own polls. Everything else that polls the page —
  // the board, the agent-state chip — has no range to name, and skipping those
  // would freeze them on whatever they first rendered.
  const asked = rangeOf(props.request.url);
  if (!asked) return false;

  if (paneBusy()) return true;

  // A poll that set out before the reader picked a different range describes
  // the one they just left, and rendering it would undo the click.
  const showing = document.querySelector("#review")?.dataset.scope;
  return !!showing && asked !== showing;
};

// ---- review comments --------------------------------------------------------
// Clicking a diff line moves the (single) compose box under it and points it at
// that line. Clicking away saves it as a draft; the server owns everything else.

up.compiler(".review", (review) => {
  const form = review.querySelector(".compose");
  if (!form) return;

  const textarea = form.querySelector("textarea");

  const close = () => {
    form.hidden = true;
    textarea.value = "";
    review.querySelectorAll(".line.commenting").forEach((line) => line.classList.remove("commenting"));
    up.radio.startPolling(review);
  };

  const open = (line) => {
    const [side, number] = line.dataset.anchor.split(":");
    form.elements.file_path.value = line.dataset.file;
    form.elements.side.value = side;
    form.elements.line.value = number;

    line.after(form);
    form.hidden = false;
    line.classList.add("commenting");
    textarea.focus();

    // A poll swaps the pane out from under this box, half-typed comment and
    // all. Nothing is lost by holding still until it is put away.
    holdPane(review);
  };

  review.addEventListener("click", (event) => {
    if (event.target.closest(".compose, .thread, a, button, summary")) return;

    const line = event.target.closest(".line");
    if (!line) return;

    const reopening = line.classList.contains("commenting");
    close();
    if (!reopening) open(line);
  });

  // Clicking away is what saves: an empty box was a change of mind.
  textarea.addEventListener("blur", () => {
    if (form.hidden) return;
    if (textarea.value.trim()) up.submit(form);
    else close();
  });

  form.addEventListener("cancel-comment", close);
});

// ---- file tree as a jump list -----------------------------------------------
// Every file in the range is already in the diff, so picking one out of the tree
// is a scroll rather than a round trip. Which node is highlighted follows the
// scroller instead of the click, so it stays honest when the reader scrolls past
// a file on their own.

up.compiler(".review", (review) => {
  const lines = review.querySelector("#diff-lines");
  const tree = review.querySelector(".tree-body");
  if (!lines || !tree) return;

  const nodeFor = (section) => tree.querySelector(`[href="#${section.id}"]`);

  tree.addEventListener("click", (event) => {
    const node = event.target.closest(".file-node");
    if (!node) return;

    event.preventDefault();
    // Not the browser's own hash navigation: that pushes history, which unpoly
    // then has to reconcile against a fragment it never navigated to.
    review.querySelector(node.getAttribute("href"))?.scrollIntoView({ block: "start" });
  });

  // The file being read is the first one not yet scrolled past, which is what
  // the sticky header is showing. Derived from geometry rather than from the
  // entries, because any one entry only reports its own file.
  const observer = new IntersectionObserver(
    () => {
      const sections = [...lines.querySelectorAll(".file")];
      const top = lines.getBoundingClientRect().top;
      const reading =
        sections.find((section) => section.getBoundingClientRect().bottom > top + 1) ??
        sections[sections.length - 1];

      for (const section of sections) {
        nodeFor(section)?.classList.toggle("selected", section === reading);
      }
      if (reading) nodeFor(reading)?.scrollIntoView({ block: "nearest" });
    },
    { root: lines, threshold: [0, 1] },
  );

  for (const section of lines.querySelectorAll(".file")) observer.observe(section);

  return () => observer.disconnect();
});

// ---- diff range -------------------------------------------------------------
// Every anchor and both halves of the toggle are ordinary links the server
// built, so picking a range needs no script at all. What is left is keeping the
// pane's own poll out of the way of someone using them.

// Following any of them has to hold the poll off until the swap lands: the
// timer would otherwise fire while the click's own request is still out, and
// that poll carries the *old* `up-source`, so its response would put the pane
// back on the range just left. The replacement fragment brings `up-poll` with
// it, which starts it again.
up.compiler(".review", (review) => {
  review.addEventListener("click", (event) => {
    if (event.target.closest("a[up-follow], [up-submit]")) holdPane(review);
  });
});

up.compiler("[data-range-menu]", (menu) => {
  const review = menu.closest(".review");

  // Opening it holds the pane: a swap underneath would close it mid-choice.
  //
  // NB: nothing closes it on the way out. Picking a range replaces the pane,
  // and the menu that arrives with it is closed — whereas closing this one by
  // hand fires `toggle`, which would start the poll again while the click's own
  // request is still out, and that poll would answer with the range just left.
  menu.addEventListener("toggle", () => {
    if (menu.open) holdPane(review);
    else up.radio.startPolling(review);
  });
});

// ---- the review tab's stat --------------------------------------------------
// The tab label sits outside `#review`, so a poll cannot reach it. The pane
// carries its own totals; this copies them across after each render.

up.compiler(".review", (review) => {
  const stat = document.querySelector("#tab-stat");
  if (!stat) return;

  stat.hidden = review.dataset.hasDiff !== "1";
  stat.querySelector(".adds").textContent = `+${review.dataset.additions}`;
  stat.querySelector(".dels").textContent = `−${review.dataset.deletions}`;
});
