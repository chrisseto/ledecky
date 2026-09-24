import { Terminal } from "@xterm/xterm";
import { AttachAddon } from "@xterm/addon-attach";
import { Base64, ClipboardAddon } from "@xterm/addon-clipboard";
import { FitAddon } from "@xterm/addon-fit";

/**
 * OSC 52, one way only.
 *
 * The addon's own provider answers a read out of `navigator.clipboard.readText()`
 * as readily as it writes, so anything the agent printed could ask for whatever
 * the user last copied. A copy the agent performs is useful; a read it performs
 * is a channel out of the page driven by output nobody vetted.
 */
const clipboard = {
  // NB: still an answer, just an empty one. The addon replies either way, and a
  // client waiting on one should not be left waiting.
  readText: () => "",
  writeText: (selection, text) =>
    selection === "c" ? navigator.clipboard.writeText(text) : undefined,
};

/** Opens a terminal on `host` and attaches it to the agent behind it. */
const attach = (host) => {
  // `stop()` disposes the terminal, and the socket's `close` lands after it.
  let stopped = false;

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
  term.loadAddon(new ClipboardAddon(new Base64(), clipboard));
  term.open(host);
  fit.fit();

  const url = new URL(host.dataset.terminal, location.href);
  url.protocol = location.protocol === "https:" ? "wss:" : "ws:";
  url.searchParams.set("rows", term.rows);
  url.searchParams.set("cols", term.cols);

  // The server sizes the pty to this before it renders a byte, so the screen
  // that comes back is already ours and nothing has to be reflowed into place.
  let sent = `${term.rows}x${term.cols}`;

  const socket = new WebSocket(url);
  // Ahead of the addon's own, which is set from `open` below — a frame must not
  // arrive as a Blob in the meantime.
  socket.binaryType = "arraybuffer";

  // Both directions: keystrokes and pastes over `onData`, and mouse reports in
  // the default encoding over `onBinary`, which xterm keeps separate because
  // those carry bytes past 0x7f that are not text.
  //
  // NB: from `open` rather than now. A send while the socket is still
  // CONNECTING *throws* in the addon rather than being dropped, and typing into
  // a pane that has only just appeared is the ordinary case. No message can be
  // dispatched ahead of `open` on the same socket, so the replay is not at risk.
  socket.addEventListener("open", () => {
    if (!stopped) term.loadAddon(new AttachAddon(socket));
  });

  // The addon disposes itself on a close and takes its own handlers with it, so
  // saying so is ours to do.
  socket.addEventListener("close", () => {
    if (!stopped) term.write("\r\n\x1b[2m-- agent disconnected --\x1b[0m\r\n");
  });

  const resize = () => {
    if (stopped) return;

    fit.fit();
    const { rows, cols } = term;
    const key = `${rows}x${cols}`;
    if (key === sent) return;

    sent = key;
    fetch(host.dataset.resizeUrl, {
      method: "POST",
      headers: { "Content-Type": "application/x-www-form-urlencoded" },
      body: new URLSearchParams({ rows, cols }),
    });
  };

  // The first fit measures whatever font is up at the time, and the box it
  // measured in never changes afterwards — so nothing else would ever notice
  // the real one arriving.
  document.fonts.ready.then(resize);

  return {
    resize,
    stop: () => {
      stopped = true;
      socket.close();
      // Takes the addons with it; none of them is ours to dispose.
      term.dispose();
    },
  };
};

/**
 * The agent's live screen, over a socket carrying raw pty bytes both ways.
 *
 * Carries `hx-morph-skip`: xterm builds this subtree on the client and the
 * server knows nothing about it, so a morph would reconcile it away. The id
 * names the card, so opening a different one replaces the element and its
 * socket outright rather than reusing this one.
 */
export class TerminalPane extends HTMLElement {
  connectedCallback() {
    // xterm cancels a wheel only when it actually scrolled the viewport with it; at
    // either end of the scrollback it lets the event through and the page behind the
    // drawer scrolls instead. Nothing in this pane should ever move the board.
    this.onWheel = (event) => event.preventDefault();
    this.addEventListener("wheel", this.onWheel, { passive: false });

    // NB: the review tab hides this pane, and the drawer opens on it whenever
    // there is a diff to read — so this connects at 0x0 as often as not. xterm
    // opened into that measures no cell at all and never re-measures, and the
    // replay would land in its 80x24 default, wrapped at a width that is not the
    // agent's. Nothing starts until the pane is on screen.
    const tick = () => {
      if (!this.clientWidth || !this.clientHeight) return;

      if (this.session) this.session.resize();
      else this.session = attach(this);
    };

    this.observer = new ResizeObserver(() => {
      clearTimeout(this.debounce);
      this.debounce = setTimeout(tick, 80);
    });
    this.observer.observe(this);
    tick();
  }

  disconnectedCallback() {
    this.observer?.disconnect();
    clearTimeout(this.debounce);
    this.removeEventListener("wheel", this.onWheel);
    this.session?.stop();
    this.observer = null;
    this.session = null;
  }
}
