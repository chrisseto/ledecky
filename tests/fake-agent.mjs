#!/usr/bin/env node
//
// A scripted stand-in for `claude`, used by the end-to-end suite.
//
// It imitates only the parts of the real client that ledecky actually couples
// to:
//
//   * an opening task taken as a positional argument, held while a modal is up
//     and submitted on its own once the modal is answered;
//   * a per-session inbox socket, whose path it reports by running the
//     SessionStart command hook out of its own --settings — which is both how
//     the server sends it anything and how the server knows it has started;
//   * a startup window where it has drawn nothing yet and the SessionStart hook
//     has not run, which the server must not mistake for being ready;
//   * a modal that owns the keyboard and treats a bare Enter as "exit", which is
//     how the trust and bypass-permissions dialogs behave. Neither numbers its
//     options, so neither is matched by looking for a leading "1.";
//   * dying on "k" without a SessionEnd hook, the way a crash or an outside
//     kill does — the case where the pty reaching EOF is the only notice;
//   * a full-height screen with the input box pinned near the bottom, because
//     the server reads the last rows to tell a dialog from an input box;
//   * the HTTP hooks named in its own --settings argument;
//   * naming itself after its first prompt, as a metadata line in its
//     transcript — which is where the server reads the card's title from;
//   * a permission mode recorded in its transcript, which `--resume` without
//     --permission-mode picks back up. `[mode:<name>]` in a prompt changes it,
//     standing in for shift+tab or approving a plan.
//
// Each submitted prompt appends a line to main.rs so turn snapshots have
// something to capture, and the merge prompt is understood well enough to move
// the base branch for real.

import { execFileSync, execSync } from "node:child_process";
import { createServer } from "node:net";
import { createInterface } from "node:readline";
import {
  appendFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";

const ESC = "\u001b";
const PASTE_START = `${ESC}[200~`;
const PASTE_END = `${ESC}[201~`;
const ROWS = 40;
/** Width of the ruler line in the startup banner. See its use below. */
const RULER_COLS = 100;

const argv = process.argv.slice(2);
const flag = (name) => {
  const i = argv.indexOf(name);
  return i >= 0 ? argv[i + 1] : undefined;
};

// The opening task arrives as a positional argument after `--`, the way the
// real client takes one. Everything before it is flag/value pairs.
const separator = argv.indexOf("--");
const openingTask = separator >= 0 ? argv[separator + 1] : undefined;

const settings = JSON.parse(flag("--settings") ?? "{}");
const hookUrl = (event) => settings.hooks?.[event]?.[0]?.hooks?.[0]?.url;
const repo = flag("--add-dir");
const resuming = flag("--resume");
const sessionId = resuming ?? `fake-${process.pid}`;

// Sessions live on disk, as the real client's do, so that resuming one that was
// never written — or has since been pruned — fails the way the real one fails:
// a single line on stdout and a non-zero exit, with no TUI in between.
const SESSIONS = join(process.env.XDG_DATA_HOME ?? "/tmp", "fake-agent-sessions");
const transcriptPath = join(SESSIONS, `${sessionId}.jsonl`);

if (resuming && !existsSync(transcriptPath)) {
  process.stdout.write(`No conversation found with session ID: ${resuming}\n`);
  process.exit(1);
}
mkdirSync(SESSIONS, { recursive: true });
appendFileSync(transcriptPath, `${JSON.stringify({ session: sessionId })}\n`);

/** The mode this session was last in, as its transcript recorded it. */
function recordedMode() {
  return readFileSync(transcriptPath, "utf8")
    .split("\n")
    .filter(Boolean)
    .map((line) => JSON.parse(line).permissionMode)
    .filter(Boolean)
    .at(-1);
}

let permissionMode = flag("--permission-mode") ?? recordedMode() ?? "default";
const recordMode = () =>
  appendFileSync(transcriptPath, `${JSON.stringify({ type: "mode", permissionMode })}\n`);
recordMode();

// NB: in the system temp dir rather than under the card, because a unix socket
// path is capped near 100 bytes and the suite's data directories are long.
const socketPath = join(tmpdir(), `fake-agent-${process.pid}.sock`);

const out = (s) => process.stdout.write(s);
const transcript = [
  `fake-agent - ${sessionId}`,
  `cwd ${process.cwd()}`,
  `mode ${permissionMode}`,
  "",
];

// bypassPermissions shows a one-time consent dialog before anything else runs.
let modal = permissionMode === "bypassPermissions" ? "consent" : null;

// Set between answering a permission prompt and `f`. The turn is deliberately
// still open in that window: a card has to have stopped saying "needs you" while
// the agent is working, which the `idle` of a finished turn would otherwise hide.
let working = null;
/** The full text the composer holds. */
let buffer = "";
/** What the composer displays, which collapses for a long paste. */
let shown = "";
let turn = 0;

// The real client spends several seconds drawing itself before it has an input
// box. Nothing it is handed in that window goes anywhere, and the SessionStart
// hook has not run, so the server has nothing to mistake for readiness.
let booting = true;
setTimeout(() => {
  booting = false;
  render();
  // Only now: SessionStart fires once the client is up and past any dialog,
  // which is exactly what makes it worth anything as a signal to the server.
  ready();
}, Number(process.env.FAKE_AGENT_BOOT_MS ?? 4000));

/**
 * Opens the inbox socket and runs the SessionStart command hook, which is how
 * the server learns where to send messages — and, because this only happens
 * once no modal is left holding the keyboard, how it learns the session is up.
 */
function ready() {
  if (modal) return; // still the user's to answer; the task waits with it

  const server = createServer((connection) => {
    const lines = createInterface({ input: connection });
    lines.on("line", (line) => {
      let frame;
      try {
        frame = JSON.parse(line);
      } catch {
        return;
      }
      // The auth line opens the connection; the message follows it.
      if (frame.type === "user") submit(frame.message.content);
    });
  });

  // NB: everything below waits on the listen callback. Reporting the path
  // before the socket accepts leaves the server a live path to connect to and
  // nothing behind it, which is a flake rather than a failure.
  server.listen(socketPath, () => {
    const command = settings.hooks?.SessionStart?.[0]?.hooks?.[0]?.command;
    if (command) {
      try {
        execSync(command, {
          env: {
            ...process.env,
            CLAUDE_CODE_MESSAGING_SOCKET: socketPath,
            CLAUDE_CODE_MESSAGING_TOKEN: `token-${process.pid}`,
          },
        });
      } catch {
        // A server that has gone away is not the fake agent's problem.
      }
    }

    // The opening task was handed over on the command line and has been waiting
    // for the client to be able to take it.
    if (openingTask) submit(openingTask);
  });
}

function render() {
  out(`${ESC}[2J${ESC}[H`);
  out(transcript.slice(-20).join("\r\n"));

  // A modal replaces the input box with its own choices, marking the
  // highlighted one the way the real client does.
  //
  // NB: the consent dialog's options carry no numbers, because the real one's
  // do not — that is how it tells a dialog from an input box, and numbering
  // them here is what hid the bug where neither startup dialog was recognised.
  // The mid-turn permission prompt does number its options, so one of each is
  // on the screen the server reads.
  let block;
  if (booting) {
    block = [border(), "starting…"];
  } else if (modal === "consent") {
    block = [
      "WARNING: Bypass Permissions mode",
      "❯ No, exit",
      "  Yes, I accept",
      "Enter to confirm · Esc to cancel",
    ];
  } else if (modal === "permission") {
    block = ["Bash command needs approval", "❯ 1. Yes", "  2. No", "Enter to confirm"];
  } else {
    block = composer();
  }

  // The consent dialog is drawn from the *top* of an otherwise empty screen, as
  // the real one is — twenty rows above where the input box would be. Pinning it
  // to the bottom like everything else is what hid the startup dialogs from a
  // server that only read the last rows.
  out(modal === "consent" ? `${ESC}[2;1H` : `${ESC}[${ROWS - block.length};1H`);
  out(block.join("\r\n"));
}

const border = () => "─".repeat(20);

/**
 * The input box.
 *
 * A typed message keeps the prompt marker on its first line only and runs plain
 * from there, exactly as the real client draws it.
 */
function composer() {
  const [first = "", ...rest] = shown.split("\n");
  return [border(), `> ${first}`, ...rest.map((line) => `  ${line}`)];
}

async function hook(event, body) {
  const url = hookUrl(event);
  if (!url) return;
  try {
    await fetch(url, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        session_id: sessionId,
        transcript_path: transcriptPath,
        hook_event_name: event,
        cwd: process.cwd(),
        ...body,
      }),
    });
  } catch {
    // The server going away mid-run is not the fake agent's problem.
  }
}

const git = (cwd, ...args) =>
  execFileSync("git", ["-C", cwd, ...args], { encoding: "utf8" }).trim();

/** Applies the merge prompt: commit here, then fast-forward the base branch. */
function performMerge(prompt) {
  const branch = prompt.match(/Land it on `([^`]+)`/)?.[1];
  if (!branch || !repo) return "could not work out the base branch";

  git(process.cwd(), "add", "-A");
  try {
    git(
      process.cwd(),
      "-c",
      "user.email=fake@agent",
      "-c",
      "user.name=fake agent",
      "commit",
      "-qm",
      `work for ${sessionId}`,
    );
  } catch {
    // Nothing left to commit.
  }

  const sha = git(process.cwd(), "rev-parse", "HEAD");
  git(repo, "merge", "--ff-only", sha);
  return `merged ${sha} into ${branch}`;
}

async function submit(prompt) {
  buffer = "";
  shown = "";
  turn += 1;
  transcript.push(`> ${prompt.split("\n")[0]}`);
  render();

  // The real client titles an unnamed session off its first prompt, shortly
  // after submitting it, and only ever writes it to the transcript — never to a
  // hook payload. Derived from the prompt so the suite can still find the card
  // by the words it typed.
  if (turn === 1) {
    appendFileSync(
      transcriptPath,
      `${JSON.stringify({
        type: "ai-title",
        aiTitle: `${prompt.split("\n")[0]} (named)`,
        sessionId,
      })}\n`,
    );
  }

  const mode = prompt.match(/\[mode:(\w+)\]/)?.[1];
  if (mode) {
    permissionMode = mode;
    recordMode();
  }

  await hook("UserPromptSubmit", { prompt });

  // A marker in the prompt drives the permission path on demand.
  if (prompt.includes("[needs-permission]")) {
    modal = "permission";
    render();
    await hook("Notification", { notification_type: "permission_prompt" });
    return; // the turn resumes once the modal is answered
  }

  let summary;
  if (prompt.startsWith("The reviewer approved this work.")) {
    try {
      summary = performMerge(prompt);
    } catch (err) {
      summary = `merge failed: ${err.message}`;
    }
  } else {
    // Record the whole prompt so a test can prove what actually reached the agent.
    const detail = prompt.replace(/\s+/g, " ").trim().slice(0, 300);
    appendFileSync("main.rs", `// turn ${turn}: ${detail}\n`);

    // Also rewrite one word of an existing line. An appended line has no
    // counterpart to diff against, so without this there is no within-line
    // change for the review pane to highlight.
    const before = readFileSync("main.rs", "utf8");
    writeFileSync("main.rs", before.replace(/"(hi|turn-\d+)"/, `"turn-${turn}"`));

    // A second file, so the review pane has more than one to stack.
    appendFileSync("README.md", `\n- turn ${turn}\n`);

    summary = `applied turn ${turn}`;
  }

  transcript.push(`* ${summary}`);
  render();
  await hook("Stop", {
    last_assistant_message: summary,
    background_tasks: [],
    session_crons: [],
  });
}

function answerModal(key) {
  if (modal === "consent") {
    if (key === "2") {
      modal = null;
      transcript.push("* bypass permissions accepted");
      render();
      // The client has the keyboard back, so it can start and take the task it
      // was launched with.
      ready();
      return true;
    }
    if (key === "1" || key === "\r" || key === "\n") {
      // Enter takes the highlighted option, which is "No, exit". The server is
      // expected never to send a bare Enter into a modal.
      transcript.push("* declined bypass permissions, exiting");
      render();
      hook("SessionEnd", { reason: "declined" }).then(() => process.exit(0));
      return true;
    }
    return true; // everything else is swallowed by the modal
  }

  if (modal === "permission") {
    if (key !== "1" && key !== "2") return true;
    modal = null;
    const allowed = key === "1";
    transcript.push(allowed ? "* approved" : "* denied");
    if (allowed) appendFileSync("main.rs", `// turn ${turn}: approved\n`);
    // The composer is back, so the server can see the dialog has gone — but the
    // turn stays open until `f`. Ending it here instead would let the `idle` that
    // follows rescue a card still stuck on "needs you", and a timer would only
    // make the gap between the two a race.
    working = allowed ? "approved and applied" : "denied";
    render();
    return true;
  }

  if (working && key === "f") {
    const summary = working;
    working = null;
    transcript.push("* finished");
    render();
    hook("Stop", { last_assistant_message: summary, background_tasks: [] });
    return true;
  }

  // Dies where the real client would crash or be killed from outside: no
  // SessionEnd, no warning, just a pty that stops. The only thing that notices
  // is the server's pump reaching EOF.
  if (key === "k") {
    try {
      unlinkSync(socketPath);
    } catch {
      // Never opened.
    }
    process.exit(1);
  }

  return false;
}

// ---- input ------------------------------------------------------------------

let pasting = null; // accumulates while a bracketed paste is being received

process.stdin.setRawMode?.(true);
process.stdin.on("data", (chunk) => {
  let text = chunk.toString("utf8");

  while (text.length) {
    if (pasting !== null) {
      const end = text.indexOf(PASTE_END);
      if (end === -1) {
        pasting += text;
        return;
      }
      pasting += text.slice(0, end);
      text = text.slice(end + PASTE_END.length);

      // Nothing the server sends arrives this way any more — this is a person
      // pasting into the terminal pane. A client still drawing itself, or one
      // with a modal up, has nowhere to put it.
      if (!booting && !modal) {
        buffer = pasting;
        shown =
          pasting.length > 200
            ? `[Pasted text #1 +${pasting.split("\n").length} lines]`
            : pasting;
      }
      pasting = null;
      render();
      continue;
    }

    const start = text.indexOf(PASTE_START);
    if (start !== -1) {
      pasting = "";
      text = text.slice(start + PASTE_START.length);
      continue;
    }

    const key = text[0];
    text = text.slice(1);

    // Nothing is listening for keys until the client has drawn itself.
    if (booting) continue;
    if (answerModal(key)) continue;

    if (key === "\r" || key === "\n") {
      if (buffer.trim()) submit(buffer);
      continue;
    }

    if (key >= " ") {
      buffer += key;
      shown += key;
      render();
    }
  }
});

for (const signal of ["SIGTERM", "SIGINT", "SIGHUP"]) {
  process.on(signal, () => {
    try {
      unlinkSync(socketPath);
    } catch {
      // Never opened, or already gone.
    }
    hook("SessionEnd", { reason: signal }).finally(() => process.exit(0));
  });
}

// The real client scrolls a long transcript off the top of the screen before it
// starts repainting in place. `render` only ever clears and redraws, so without
// this the pty would have no scrollback at all and nothing would exercise the
// history the server replays to a connecting client.
//
// One of those lines is a ruler, wider than xterm's 80-column default and
// narrower than the pane the drawer gives it: a client that connected before it
// knew its own size replays it wrapped in two.
for (let i = 0; i < ROWS + 20; i++) {
  if (i === 5) out(`${"=".repeat(RULER_COLS)}\r\n`);
  out(`banner-${i}\r\n`);
}

// Keep the process alive on a pty even while stdin is quiet.
setInterval(() => {}, 1 << 30);
render();
