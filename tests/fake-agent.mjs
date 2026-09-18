#!/usr/bin/env node
//
// A scripted stand-in for `claude`, used by the end-to-end suite.
//
// It imitates only the parts of the real TUI that ledecky actually couples to:
//
//   * a full-height screen with the input box pinned near the bottom, because
//     the server looks for its pasted text in the last rows of the terminal;
//   * a startup window with no input box at all, where anything sent is queued
//     on the box's border rather than typed — text on screen that no Enter will
//     submit, which the server must not mistake for a delivered prompt;
//   * a composer that marks only the first line of a pasted message, leaving
//     the rest plain — the server's needle usually lands on one of those;
//   * bracketed-paste handling, collapsing long pastes to "Pasted text" exactly
//     as the real client does;
//   * a modal that swallows pastes and treats a bare Enter as "exit", which is
//     how the bypass-permissions consent dialog behaves — blind-Entering into
//     one used to kill agents outright;
//   * the HTTP hooks named in its own --settings argument;
//   * naming itself after its first prompt, as a metadata line in its
//     transcript — which is where the server reads the card's title from.
//
// Each submitted prompt appends a line to main.rs so turn snapshots have
// something to capture, and the merge prompt is understood well enough to move
// the base branch for real.

import { execFileSync } from "node:child_process";
import { appendFileSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

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

const settings = JSON.parse(flag("--settings") ?? "{}");
const hookUrl = (event) => settings.hooks?.[event]?.[0]?.hooks?.[0]?.url;
const repo = flag("--add-dir");
const permissionMode = flag("--permission-mode") ?? "default";
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
/** Text sent before there was anywhere to put it. */
let queued = "";
let turn = 0;

// The real client spends several seconds drawing itself before it has an input
// box, and anything sent in that window is queued on the box's border instead of
// typed into it — where a bare Enter will not submit it either.
let booting = true;
setTimeout(() => {
  booting = false;
  render();
}, Number(process.env.FAKE_AGENT_BOOT_MS ?? 4000));

function render() {
  out(`${ESC}[2J${ESC}[H`);
  out(transcript.slice(-20).join("\r\n"));

  // A modal replaces the input box with its own choices, marking the
  // highlighted one the way the real client does — that marker is how the
  // server tells a dialog holding the keyboard from a client that has not drawn
  // a box yet.
  let block;
  if (booting) {
    block = [border(), "starting…"];
  } else if (modal === "consent") {
    block = ["WARNING: Bypass Permissions mode", "❯ 1. No, exit", "  2. Yes, I accept", "Enter to confirm"];
  } else if (modal === "permission") {
    block = ["Bash command needs approval", "❯ 1. Yes", "  2. No", "Enter to confirm"];
  } else {
    block = composer();
  }

  // Pin it to the bottom; the server only searches the last rows.
  out(`${ESC}[${ROWS - block.length};1H`);
  out(block.join("\r\n"));
}

const border = () => `${"─".repeat(20)} ${queued} ──`;

/**
 * The input box.
 *
 * A pasted message keeps the prompt marker on its first line only and runs
 * plain from there, exactly as the real client draws it — so the word the
 * server looks for is usually *not* on the marked line.
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

      // A client still drawing itself queues the paste out of the input box; a
      // modal owns the keyboard and drops it. Both are the real client's
      // behaviour, and neither puts the text anywhere Enter can submit it.
      if (booting) {
        queued = pasting.replace(/\n/g, " ").slice(0, 60);
      } else if (!modal) {
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
