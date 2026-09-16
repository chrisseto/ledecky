// NB: unpoly ships a CommonJS bundle that only assigns `window.up` — it has no
// usable default export, so this is an import for the side effect. Reading the
// global is the supported way to reach the API.
import "unpoly";
import Sortable from "sortablejs";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";

const { up } = window;

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
    theme: { background: "#0a0c10", foreground: "#dbe0ea" },
  });

  const fit = new FitAddon();
  term.loadAddon(fit);
  term.open(host);
  fit.fit();

  const url = new URL(host.dataset.terminal, location.href);
  url.protocol = location.protocol === "https:" ? "wss:" : "ws:";

  const socket = new WebSocket(url);
  socket.binaryType = "arraybuffer";

  socket.addEventListener("open", () => postSize());
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
  const postSize = () => {
    const { rows, cols } = term;
    const key = `${rows}x${cols}`;
    if (key === sent) return;
    sent = key;
    up.request(host.dataset.resizeUrl, { method: "post", params: { rows, cols } });
  };

  let debounce;
  const observer = new ResizeObserver(() => {
    clearTimeout(debounce);
    debounce = setTimeout(() => {
      fit.fit();
      postSize();
    }, 80);
  });
  observer.observe(host);

  return () => {
    observer.disconnect();
    socket.close();
    term.dispose();
  };
});

// ---- review comments --------------------------------------------------------
// Clicking a diff line moves the (single) comment form under it and points it at
// that line. The server owns everything else.

up.compiler(".diff", (diff) => {
  const form = diff.querySelector(".comment-form");
  if (!form) return;

  const close = () => {
    form.hidden = true;
    form.querySelector("textarea").value = "";
    diff.querySelectorAll("tr.commenting").forEach((tr) => tr.classList.remove("commenting"));
  };

  const open = (row) => {
    const [side, line] = row.dataset.anchor.split(":");
    form.elements.file_path.value = row.dataset.file;
    form.elements.side.value = side;
    form.elements.line.value = line;

    const holder = document.createElement("tr");
    const cell = document.createElement("td");
    cell.colSpan = 3;
    cell.appendChild(form);
    holder.appendChild(cell);
    row.after(holder);

    form.hidden = false;
    row.classList.add("commenting");
    form.querySelector("textarea").focus();
  };

  diff.addEventListener("click", (event) => {
    if (event.target.closest(".comment-form, .comment, a, button, select")) return;

    const row = event.target.closest("tr.l");
    if (!row) return;

    const reopening = row.classList.contains("commenting");
    close();
    if (!reopening) open(row);
  });

  form.querySelector("[data-cancel-comment]").addEventListener("click", close);
});

// ---- diff scope -------------------------------------------------------------

up.compiler("[data-diff-scope]", (select) => {
  select.addEventListener("change", () => {
    up.render({
      target: "#diff",
      url: `${select.dataset.diffScope}?scope=${encodeURIComponent(select.value)}`,
      cache: false,
    });
  });
});
