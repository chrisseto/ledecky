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

  const url = new URL(host.dataset.terminal, location.href);
  url.protocol = location.protocol === "https:" ? "wss:" : "ws:";

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

  resize();

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
  };

  review.addEventListener("click", (event) => {
    if (event.target.closest(".compose, .thread, a, button, select")) return;

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

// ---- diff range -------------------------------------------------------------
// The pane carries its own view, so the selector only has to say what changed.

up.compiler("[data-scope-select]", (select) => {
  select.addEventListener("change", () => {
    const card = select.closest(".review").dataset.card;

    up.render({
      target: "#review",
      url: `/cards/${card}/diff?scope=${encodeURIComponent(select.value)}`,
      cache: false,
    });
  });
});
