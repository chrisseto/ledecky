use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::Bytes;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};
use tokio::sync::broadcast;

use crate::agent::session;
use crate::config::{Settings, Timings};
use crate::db::Db;
use crate::project::Card;

const DEFAULT_ROWS: u16 = 40;
const DEFAULT_COLS: u16 = 120;
const SCROLLBACK: usize = 5000;

/// Paste delivery: up to PASTE_ATTEMPTS sends, each polled PASTE_CHECKS times.
// NB: the paste poll interval and the dialog grace live in `Settings`, so the
// end-to-end suite does not have to wait in real time.
const PASTE_CHECKS: u32 = 8;
const PASTE_ATTEMPTS: u32 = 4;

/// Rows at the bottom of the screen treated as the input box.
const COMPOSER_ROWS: usize = 15;

// The dialog grace is `Timings::dialog_grace`: our own hook returns no
// decision, but the settings merge with the user's (`hooks.rs`) — one of theirs
// can answer the request, and then nothing is ever drawn and there is nothing
// to wait for.

/// What the watcher believes about a dialog holding the keyboard.
///
/// `Expected` and `OnScreen` are deliberately different: the `Notification` hook
/// can reach us before the client paints anything, so treating the two alike
/// would clear the card before there was a dialog to answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dialog {
    None,
    Expected,
    OnScreen,
}

/// One live `claude` process attached to a pty.
pub struct Agent {
    /// Recorded so a later server run can sweep this up if we die without
    /// getting the chance to.
    pub pid: Option<i64>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    screen: Arc<Mutex<vt100::Parser>>,
    output: broadcast::Sender<Bytes>,
    /// The opening task, held until the session is actually up. Flushed by the
    /// `SessionStart` hook, or by a timer if hooks never reach us.
    pending_prompt: Mutex<Option<String>>,
    /// What the pump believes about a dialog, and when it started believing it.
    dialog: Mutex<(Dialog, Instant)>,
    /// The real-time waits this agent makes, from `Settings`.
    timings: Timings,
}

impl Agent {
    pub fn subscribe(&self) -> broadcast::Receiver<Bytes> {
        self.output.subscribe()
    }

    /// Escape codes that repaint the current screen from scratch. Replaying a
    /// truncated raw byte log would corrupt the display; this cannot.
    pub fn snapshot(&self) -> Vec<u8> {
        self.screen.lock().unwrap().screen().state_formatted()
    }

    /// The scrollback sitting above the current screen, oldest row first, as
    /// ordinary output lines. Sent to a client ahead of `snapshot()` so it has
    /// history to scroll back into; the repaint lands on top of it.
    pub fn history(&self, client_rows: Option<u16>) -> Vec<u8> {
        history_bytes(self.screen.lock().unwrap().screen_mut(), client_rows)
    }

    pub fn write_input(&self, bytes: &[u8]) {
        let mut writer = self.writer.lock().unwrap();
        let _ = writer.write_all(bytes);
        let _ = writer.flush();
    }

    /// Sends `text` as one message and returns whether it was actually submitted.
    ///
    /// Claude Code's TUI treats a bare newline as submit, so the text goes in as
    /// a bracketed paste. Nothing is sent until the input box is on screen, and
    /// the submit key is not sent until the paste is visibly in it: a client
    /// that is still starting up *queues* what it is sent, out of the box and
    /// out of reach, and a dialog — the workspace-trust prompt, the
    /// bypass-permissions consent — swallows it, where a blind Enter would
    /// answer the dialog instead.
    #[must_use]
    pub fn inject(&self, text: &str) -> bool {
        let text = text.trim_end();
        if text.is_empty() {
            return true;
        }

        let needle = paste_needle(text);

        let mut payload = Vec::with_capacity(text.len() + 16);
        payload.extend_from_slice(b"\x1b[200~");
        payload.extend_from_slice(text.as_bytes());
        payload.extend_from_slice(b"\x1b[201~");

        for _ in 0..PASTE_ATTEMPTS {
            if !self.is_composing() {
                std::thread::sleep(self.timings.paste_poll);
                continue;
            }

            // A paste written while the TUI is mid-redraw is simply dropped, so
            // re-send rather than waiting longer on one that never arrived. The
            // in-box check before each retry keeps it from landing twice.
            if !self.holds(&needle) {
                self.write_input(&payload);
            }

            for _ in 0..PASTE_CHECKS {
                std::thread::sleep(self.timings.paste_poll);
                if !self.holds(&needle) {
                    continue;
                }

                self.write_input(b"\r");
                if self.cleared(&needle) {
                    return true;
                }
                break; // the key went nowhere; the text is still sitting there
            }
        }
        false
    }

    /// The input box, or the dialog standing in for it.
    fn composer(&self) -> Vec<String> {
        let screen = self.screen.lock().unwrap();
        let contents = screen.screen().contents();

        let lines: Vec<&str> = contents.lines().collect();
        composer_block(&lines)
            .iter()
            .map(|line| (*line).to_owned())
            .collect()
    }

    /// Whether there is an input box to paste into at all.
    ///
    /// A client that has not drawn one yet is still starting up, and a dialog
    /// over it leaves only its own highlighted options behind.
    pub fn is_composing(&self) -> bool {
        self.composer()
            .first()
            .is_some_and(|line| !is_menu_option(line))
    }

    /// Whether a dialog is holding the keyboard, which is the user's to answer.
    pub fn is_blocked(&self) -> bool {
        self.composer()
            .first()
            .is_some_and(|line| is_menu_option(line))
    }

    /// Arms the watcher: a dialog has been asked for.
    ///
    /// NB: it looks at the screen rather than trusting the order of events. The
    /// client usually paints before the hook reaches us, and `Expected` can only
    /// promote itself when a chunk arrives *while* the dialog is up — nothing
    /// guarantees another redraw once it has been drawn.
    pub fn expect_dialog(&self) {
        let state = if self.is_blocked() {
            Dialog::OnScreen
        } else {
            Dialog::Expected
        };
        *self.dialog.lock().unwrap() = (state, Instant::now());
    }

    /// The same, for a caller that has already looked.
    pub fn saw_dialog(&self) {
        *self.dialog.lock().unwrap() = (Dialog::OnScreen, Instant::now());
    }

    /// Advances the watcher over one chunk of output, returning whether the card
    /// has stopped waiting on the user.
    ///
    /// NB: the `Dialog::None` check is the whole point of the latch — reading the
    /// screen renders it to a `String`, and this runs per chunk. The screen lock
    /// is deliberately not taken while `dialog` is held; `expect_dialog` takes
    /// them the other way round.
    fn watch_dialog(&self) -> bool {
        if self.dialog.lock().unwrap().0 == Dialog::None {
            return false;
        }

        let blocked = self.is_blocked();

        let mut dialog = self.dialog.lock().unwrap();
        let (next, resumed) = settle(
            dialog.0,
            blocked,
            dialog.1.elapsed(),
            self.timings.dialog_grace,
        );
        dialog.0 = next;
        resumed
    }

    /// True once `needle`, or the placeholder the TUI collapses a long paste to,
    /// is in the input box.
    fn holds(&self, needle: &str) -> bool {
        self.composer()
            .iter()
            .any(|line| line.contains(needle) || line.contains("Pasted text"))
    }

    /// Waits for the input box to let go of the text, which is the only proof
    /// the submit key did anything.
    fn cleared(&self, needle: &str) -> bool {
        for _ in 0..PASTE_CHECKS {
            std::thread::sleep(self.timings.paste_poll);
            if !self.holds(needle) {
                return true;
            }
        }
        false
    }

    /// Sends the opening task, keeping it queued if it could not be delivered.
    /// Returns false while the terminal is still busy with something else.
    pub fn flush_pending_prompt(&self) -> bool {
        let Some(prompt) = self.pending_prompt.lock().unwrap().take() else {
            return true; // already sent
        };

        if self.inject(&prompt) {
            return true;
        }

        *self.pending_prompt.lock().unwrap() = Some(prompt);
        false
    }

    pub fn resize(&self, rows: u16, cols: u16) {
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let _ = self.master.lock().unwrap().resize(size);
        self.screen
            .lock()
            .unwrap()
            .screen_mut()
            .set_size(rows, cols);
    }

    /// Kills the agent and reaps it. Without the `wait` the process lingers as a
    /// zombie for as long as this server runs, because nothing else calls it once
    /// the agent has been dropped from the registry.
    pub fn kill(&self) {
        let mut child = self.child.lock().unwrap();
        let _ = child.kill();
        let _ = child.wait();
    }

    pub fn is_running(&self) -> bool {
        matches!(self.child.lock().unwrap().try_wait(), Ok(None))
    }
}

#[derive(Clone, Default)]
pub struct Agents(Arc<RwLock<HashMap<i64, Arc<Agent>>>>);

impl Agents {
    pub fn get(&self, card_id: i64) -> Option<Arc<Agent>> {
        self.0.read().unwrap().get(&card_id).cloned()
    }

    pub fn remove(&self, card_id: i64) -> Option<Arc<Agent>> {
        self.0.write().unwrap().remove(&card_id)
    }

    pub fn shutdown(&self) {
        for agent in self.0.write().unwrap().drain().map(|(_, a)| a) {
            agent.kill();
        }
    }

    /// Spawns `claude` in `worktree` and registers it under `card.id`.
    ///
    /// `repo` is the project's main checkout, added as a second allowed directory
    /// so the agent can land its work on the base branch — which lives there, not
    /// in the worktree.
    pub fn spawn(
        &self,
        db: &Db,
        settings: &Settings,
        card: &Card,
        worktree: &Path,
        repo: &Path,
        hook_settings: &str,
    ) -> Result<Arc<Agent>> {
        if let Some(existing) = self.get(card.id) {
            if existing.is_running() {
                return Ok(existing);
            }
            self.remove(card.id);
        }

        let pty = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows: DEFAULT_ROWS,
                cols: DEFAULT_COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("opening a pty")?;

        let mut cmd = CommandBuilder::new(&settings.agent_bin);
        cmd.arg("--permission-mode");
        cmd.arg(&card.permission_mode);
        // Restarting a card picks the conversation back up rather than starting
        // over with no memory of the work already done.
        if let Some(session_id) = &card.session_id {
            cmd.arg("--resume");
            cmd.arg(session_id);
        }
        if let Some(model) = &card.model {
            cmd.arg("--model");
            cmd.arg(model);
        }
        cmd.arg("--settings");
        cmd.arg(hook_settings);
        cmd.arg("--add-dir");
        cmd.arg(repo);
        // NB: no `--name`. Naming the session suppresses the name it would
        // give itself, which is the one the card takes.
        cmd.cwd(worktree);
        // Match what xterm.js renders; the inherited TERM may be anything.
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        // Applied last so a deployment can override the above if it must.
        for (key, value) in &settings.agent_env {
            cmd.env(key, value);
        }
        // If this server was itself launched from a Claude Code session, these
        // markers make the child think it is a nested agent and disable session
        // persistence. Auth vars are deliberately left alone.
        for marker in [
            "CLAUDECODE",
            "CLAUDE_CODE_CHILD_SESSION",
            "CLAUDE_CODE_ENTRYPOINT",
            "CLAUDE_CODE_SSE_PORT",
            "CLAUDE_SESSION_ID",
        ] {
            cmd.env_remove(marker);
        }

        let child = pty.slave.spawn_command(cmd).context("spawning claude")?;
        drop(pty.slave);

        let pid = child.process_id().map(i64::from);
        let reader = pty.master.try_clone_reader()?;
        let writer = pty.master.take_writer()?;

        let (output, _) = broadcast::channel(1024);
        let screen = Arc::new(Mutex::new(vt100::Parser::new(
            DEFAULT_ROWS,
            DEFAULT_COLS,
            SCROLLBACK,
        )));

        let agent = Arc::new(Agent {
            pid,
            master: Mutex::new(pty.master),
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            screen,
            output,
            pending_prompt: Mutex::new(card.opening_prompt()),
            dialog: Mutex::new((Dialog::None, Instant::now())),
            timings: settings.timings(),
        });

        self.0.write().unwrap().insert(card.id, agent.clone());

        // portable-pty hands back a blocking reader, so it gets its own thread.
        // Its handle on the agent is not a cycle: the reader hits EOF when the
        // pty closes, and the loop drops it on the way out.
        let pumped = agent.clone();
        let db = db.clone();
        let card_id = card.id;
        std::thread::spawn(move || pump(reader, pumped, db, card_id));

        Ok(agent)
    }
}

/// Renders `screen`'s scrollback as plain output lines, oldest first, followed
/// by enough blank rows to scroll the last of them off a `client_rows`-tall
/// screen. Leaves `screen` back on the live view.
fn history_bytes(screen: &mut vt100::Screen, client_rows: Option<u16>) -> Vec<u8> {
    // A full-screen app has no scrollback of its own, and the normal screen's
    // belongs to whatever was running before it took over.
    if screen.alternate_screen() {
        return Vec::new();
    }

    let (rows, cols) = screen.size();
    let rows = usize::from(rows);

    // NB: `set_scrollback` clamps to what actually exists, so overshooting and
    // reading the offset back is how many rows of history there are.
    screen.set_scrollback(usize::MAX);
    let total = screen.scrollback();

    let mut out = Vec::new();
    let mut emitted = 0;
    while emitted < total {
        // The window top sits `offset` rows above the screen, so only its first
        // `offset` rows are still history rather than the live screen.
        let offset = total - emitted;
        let take = offset.min(rows);

        screen.set_scrollback(offset);
        for row in screen.rows_formatted(0, cols).take(take) {
            out.extend_from_slice(&row);
            out.extend_from_slice(b"\r\n");
        }
        emitted += take;
    }

    // NB: the last screenful of history is still *on* the client's screen here,
    // and the repaint that follows erases in place rather than scrolling.
    // Without pushing it off first those rows never reach the client's
    // scrollback, leaving a hole right above the live screen. The count is the
    // client's own height — the pty's would come up short for a taller one.
    if total > 0 {
        for _ in 0..client_rows.map_or(rows, usize::from) {
            out.extend_from_slice(b"\r\n");
        }
    }

    screen.set_scrollback(0);
    out
}

/// Feeds pty output into the screen and out to the websocket, and watches for a
/// dialog leaving that screen.
///
/// The watcher lives here because there is no hook for a permission being
/// answered: the redraw that takes the dialog away is the only signal, and this
/// is the only place it is seen.
fn pump(mut reader: Box<dyn Read + Send>, agent: Arc<Agent>, db: Db, card_id: i64) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let chunk = Bytes::copy_from_slice(&buf[..n]);
                agent.screen.lock().unwrap().process(&chunk);

                if agent.watch_dialog() {
                    session::resume_after_dialog(&db, card_id);
                }

                // No subscribers is the normal case when nobody has the card open.
                let _ = agent.output.send(chunk);
            }
        }
    }
}

/// One step of the dialog watcher: the next belief, and whether the card has
/// stopped waiting on the user.
///
/// Split out from `Agent` so the transitions can be tested without a pty.
fn settle(state: Dialog, blocked: bool, waited: Duration, grace: Duration) -> (Dialog, bool) {
    match (state, blocked) {
        (Dialog::None, _) => (Dialog::None, false),
        (_, true) => (Dialog::OnScreen, false),
        (Dialog::OnScreen, false) => (Dialog::None, true),
        // Nothing was ever drawn. Either the client is slow, or another hook
        // answered the request and no dialog is coming at all.
        (Dialog::Expected, false) if waited >= grace => (Dialog::None, true),
        (Dialog::Expected, false) => (Dialog::Expected, false),
    }
}

/// The prompt marker nearest the bottom of the screen, and everything under it.
/// Empty while the client has yet to draw an input box.
///
/// NB: anchored at the marker rather than filtered to the lines carrying one.
/// The client marks a message's *first* line only, so the words worth looking
/// for usually sit on a continuation line — anchoring takes those in. It still
/// leaves out the transcript above, which echoes what was just submitted and
/// would otherwise read as a message still sitting unsent in the box.
fn composer_block<'a>(lines: &'a [&'a str]) -> &'a [&'a str] {
    let window = lines.len().saturating_sub(COMPOSER_ROWS);
    match lines.iter().rposition(|line| is_prompt_line(line)) {
        Some(at) if at >= window => &lines[at..],
        _ => &[],
    }
}

/// Whether a line is one the TUI takes input on: its input box, or a dialog's
/// highlighted option. `❯` is what the current client draws; `>` is what older
/// ones — and the test stand-in — use.
fn is_prompt_line(line: &str) -> bool {
    matches!(line.trim_start().chars().next(), Some('❯' | '>'))
}

/// Whether that line is a numbered choice rather than the input box.
fn is_menu_option(line: &str) -> bool {
    let rest = line
        .trim_start()
        .trim_start_matches(['❯', '>'])
        .trim_start();
    let number = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();

    number > 0 && rest[number..].starts_with('.')
}

/// Picks a substring of a pasted message to look for on screen.
///
/// A long word survives the input box's line wrapping, which a fixed-length
/// prefix would not.
fn paste_needle(text: &str) -> String {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 6 && w.len() <= 20)
        .max_by_key(|w| w.len())
        .map(str::to_owned)
        .unwrap_or_else(|| text.chars().take(8).collect())
}

#[cfg(test)]
mod tests {
    use super::{
        composer_block, history_bytes, is_menu_option, is_prompt_line, paste_needle, settle,
        Dialog, COMPOSER_ROWS,
    };
    use std::time::Duration;

    /// The walk steps through the scrollback a screenful at a time, and the
    /// last step is a partial one whenever the history is not a multiple of the
    /// screen height — the case that silently drops or repeats rows if the
    /// window arithmetic is off.
    #[test]
    fn history_replays_every_scrolled_row_once_and_in_order() {
        let mut parser = vt100::Parser::new(10, 40, 100);
        for i in 0..37 {
            parser.process(format!("line-{i}\r\n").as_bytes());
        }

        // 37 lines plus the blank row the last newline left the cursor on,
        // against 10 visible rows: 28 have scrolled off.
        let out = history_bytes(parser.screen_mut(), None);
        let text = String::from_utf8(out).unwrap();
        let seen: Vec<&str> = text
            .lines()
            .filter(|l| l.contains("line-"))
            .map(|l| l.trim_end())
            .collect();

        let expected: Vec<String> = (0..28).map(|i| format!("line-{i}")).collect();
        assert_eq!(seen, expected);

        // The live view must be back, or `inject` reads the wrong rows.
        assert_eq!(parser.screen().scrollback(), 0);
    }

    /// The tail of the history is still on screen when the replay ends, so it
    /// only reaches the client's scrollback if the repaint is pushed down past
    /// it first.
    #[test]
    fn history_scrolls_its_last_screenful_off_before_the_repaint() {
        let mut parser = vt100::Parser::new(10, 40, 100);
        for i in 0..37 {
            parser.process(format!("line-{i}\r\n").as_bytes());
        }

        let out = history_bytes(parser.screen_mut(), None);
        // The final terminator leaves one empty field of its own, so drop it
        // before counting the blank rows that follow the last history line.
        let text = String::from_utf8(out).unwrap();
        let mut fields: Vec<&str> = text.split("\r\n").collect();
        fields.pop();
        let blanks = fields
            .iter()
            .rev()
            .take_while(|f| f.trim().is_empty())
            .count();
        assert_eq!(blanks, 10);
    }

    /// Nothing to replay under a full-screen app: the scrollback on the other
    /// side of the switch is not its history.
    #[test]
    fn history_is_empty_on_the_alternate_screen() {
        let mut parser = vt100::Parser::new(10, 40, 100);
        for i in 0..37 {
            parser.process(format!("line-{i}\r\n").as_bytes());
        }
        parser.process(b"\x1b[?1049h");

        assert!(history_bytes(parser.screen_mut(), None).is_empty());
    }

    /// A client taller than the pty needs a deeper flush than the pty's own
    /// height, or the newest history is still on its screen when the repaint
    /// erases it.
    #[test]
    fn history_flushes_down_by_the_clients_height_not_the_ptys() {
        let mut parser = vt100::Parser::new(10, 40, 100);
        for i in 0..37 {
            parser.process(format!("line-{i}\r\n").as_bytes());
        }

        let out = history_bytes(parser.screen_mut(), Some(30));
        let text = String::from_utf8(out).unwrap();
        let mut fields: Vec<&str> = text.split("\r\n").collect();
        fields.pop();
        let blanks = fields
            .iter()
            .rev()
            .take_while(|f| f.trim().is_empty())
            .count();
        assert_eq!(blanks, 30);
    }

    /// The composer holding a pasted message, as the client actually draws it:
    /// the marker on the first line only, the rest plain.
    const PASTED: &[&str] = &[
        "  ⏺ an earlier turn that also said Investigate",
        "────────────────────────────────────────────",
        "❯ The board flashes whenever the poll returns",
        "",
        "  Investigate the unpoly fragment swapping.",
        "────────────────────────────────────────────",
        "  -- INSERT -- ⏸ plan mode on (shift+tab to cycle)",
    ];

    /// The same message a moment later: submitted, so it has moved up into the
    /// transcript and the box is empty again.
    const SUBMITTED: &[&str] = &[
        "  The board flashes whenever the poll returns",
        "  Investigate the unpoly fragment swapping.",
        "  ⏺ Worked for 1s",
        "────────────────────────────────────────────",
        "❯ ",
        "────────────────────────────────────────────",
        "  -- INSERT -- ⏸ plan mode on (shift+tab to cycle)",
    ];

    fn holds(lines: &[&str], needle: &str) -> bool {
        composer_block(lines).iter().any(|l| l.contains(needle))
    }

    #[test]
    fn a_pasted_message_is_found_on_its_continuation_line() {
        // `paste_needle` picks the longest word, which lands below the marker.
        assert_eq!(paste_needle("The board flashes whenever the poll returns\n\nInvestigate the unpoly fragment swapping."), "Investigate");
        assert!(holds(PASTED, "Investigate"));
    }

    #[test]
    fn the_transcript_above_the_box_is_not_the_box() {
        // The words are still on screen, but they have been sent. Reading them
        // as unsent is what re-pastes a message that already went in.
        assert!(!holds(SUBMITTED, "Investigate"));
        assert!(composer_block(SUBMITTED)
            .first()
            .is_some_and(|l| l.trim() == "❯"));
    }

    #[test]
    fn an_echo_of_an_earlier_turn_is_left_above_the_anchor() {
        assert!(!composer_block(PASTED)
            .iter()
            .any(|l| l.contains("an earlier turn")));
    }

    #[test]
    fn a_client_with_no_input_box_yet_has_no_composer() {
        let booting = &["  Loading…", "  ─────────", "  starting up"];
        assert!(composer_block(booting).is_empty());
    }

    #[test]
    fn a_dialog_is_the_composer_while_it_is_up() {
        let dialog = &[
            "  Do you trust the files in this folder?",
            "❯ 1. Yes, I trust this folder",
            "  2. No, exit",
            "  Enter to confirm · Esc to cancel",
        ];
        let block = composer_block(dialog);
        assert!(block.first().is_some_and(|l| is_menu_option(l)));
    }

    #[test]
    fn a_marker_scrolled_out_of_the_window_is_not_reached_for() {
        let mut lines = vec!["❯ far above"];
        lines.extend(std::iter::repeat_n("  filler", COMPOSER_ROWS + 2));
        assert!(composer_block(&lines).is_empty());
    }

    #[test]
    fn the_input_box_is_a_prompt_line() {
        assert!(is_prompt_line("❯ "));
        assert!(is_prompt_line("❯ half a typed message"));
        assert!(is_prompt_line("> older clients and the stand-in"));
    }

    #[test]
    fn the_rest_of_the_screen_is_not() {
        // The border a queued message is drawn on — the line that used to be
        // mistaken for the input box, submitting nothing.
        assert!(!is_prompt_line(
            "──────────────── Reply with the word ok ──"
        ));
        assert!(!is_prompt_line("  -- INSERT -- accept edits on"));
        assert!(!is_prompt_line(""));
    }

    #[test]
    fn a_numbered_choice_belongs_to_a_dialog() {
        assert!(is_menu_option("❯ 1. Yes, I trust this folder"));
        assert!(is_menu_option("  2. No, exit"));
        assert!(!is_menu_option("❯ "));
        assert!(!is_menu_option("❯ 1 is not a choice without its dot"));
    }

    #[test]
    fn needle_prefers_a_long_word() {
        assert_eq!(paste_needle("Add a --verbose flag to main.rs"), "verbose");
    }

    #[test]
    fn needle_falls_back_to_a_prefix() {
        assert_eq!(paste_needle("go go go"), "go go go");
        assert_eq!(paste_needle(""), "");
    }

    // ---- the dialog watcher -------------------------------------------------

    const SOON: Duration = Duration::from_millis(200);
    /// The grace the transitions are asserted against; `Settings` supplies the
    /// real one.
    const GRACE: Duration = Duration::from_secs(5);

    #[test]
    fn an_unarmed_watcher_never_fires() {
        // Output from an ordinary turn must not be read as a dialog going away.
        assert_eq!(
            settle(Dialog::None, false, SOON, GRACE),
            (Dialog::None, false)
        );
        assert_eq!(
            settle(Dialog::None, true, SOON, GRACE),
            (Dialog::None, false)
        );
    }

    #[test]
    fn a_requested_dialog_is_held_until_it_paints() {
        // The hook fires first. Clearing here would take the card off "needs
        // you" before there was anything on screen to answer.
        assert_eq!(
            settle(Dialog::Expected, false, SOON, GRACE),
            (Dialog::Expected, false)
        );
        assert_eq!(
            settle(Dialog::Expected, true, SOON, GRACE),
            (Dialog::OnScreen, false)
        );
    }

    #[test]
    fn a_dialog_leaving_the_screen_resumes_the_card() {
        assert_eq!(
            settle(Dialog::OnScreen, true, SOON, GRACE),
            (Dialog::OnScreen, false)
        );
        assert_eq!(
            settle(Dialog::OnScreen, false, SOON, GRACE),
            (Dialog::None, true)
        );
    }

    #[test]
    fn a_dialog_that_never_paints_is_given_up_on() {
        // A notification the client never draws a dialog for — the user's own
        // `PermissionRequest` hook answered it first, say — leaves nothing to
        // wait on, so the card must not be stranded on "needs you".
        assert_eq!(
            settle(Dialog::Expected, false, GRACE, GRACE),
            (Dialog::None, true)
        );
    }
}
