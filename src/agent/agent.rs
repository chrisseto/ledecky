use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::Bytes;
use pty_process::{Command, OwnedReadPty, OwnedWritePty, Size};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, watch, Notify};
use tokio::time::{timeout_at, Instant as Deadline};

use crate::config::Timings;

const DEFAULT_ROWS: u16 = 40;
const DEFAULT_COLS: u16 = 120;
const SCROLLBACK: usize = 5000;

/// Rows at the bottom of the screen treated as the input box.
const COMPOSER_ROWS: usize = 15;

/// Moves the cursor to the end of whatever the input box holds.
///
/// NB: `CSI F`, which is what the client answers to; `SS3 F` and the `CSI n ~`
/// forms go unread. Measured rather than assumed, because the wrong one is
/// indistinguishable from a box that had nothing to move past.
const END: &[u8] = b"\x1b[F";

/// How many times a message is written before giving up on it.
///
/// NB: two, where the screen-reading version needed four. Each attempt now
/// waits `Timings::submit_grace` on the session's own hook rather than polling
/// the screen, so an attempt costs real time and a dropped paste is the only
/// thing a second one buys.
const PASTE_ATTEMPTS: u32 = 2;

/// What the pty pump has seen.
///
/// Implemented by the manager; nothing in here knows or cares who is listening.
/// The pump holds a `Weak` to one of these, which is what keeps this module from
/// needing a database, an event bus, or a way back into the registry.
pub trait Watcher: Send + Sync {
    /// A dialog has left the screen. Nothing else reports this — no hook fires
    /// when a permission prompt is answered — so the redraw is the only signal.
    fn dialog_cleared(&self, card_id: i64);

    /// The pty reached EOF. The child is gone, or has stopped writing to a
    /// terminal nobody else holds open.
    fn exited(&self, card_id: i64);
}

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

/// What became of a message handed to a running session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Paste {
    /// The session's own hook came back carrying it.
    Submitted,
    /// There was no input box: a dialog holds the keyboard, or the client never
    /// finished drawing itself.
    NoBox,
    /// The box already held something, which is not ours to send.
    Occupied,
    /// Written, but nothing came back to say the session took it.
    Unconfirmed,
    /// Submitted, along with something the box was already holding.
    Merged,
    /// Something else was submitted instead: the box held a draft the emptiness
    /// check did not see, and the submit key sent that.
    Displaced,
}

/// What the session acknowledged submitting, against what was asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Submission {
    /// The prompt was ours and nothing else.
    Ours,
    /// Ours went, with something the box was already holding.
    Merged,
    /// Something else went, and ours did not.
    Foreign,
    /// Nothing was acknowledged.
    Nothing,
}

/// One live `claude` process attached to a pty.
pub struct Agent {
    /// Recorded so a later server run can sweep this up if we die without
    /// getting the chance to.
    ///
    /// NB: a pid rather than the `Child`. The reaper holds the child across
    /// its `wait`, which needs it exclusively for the child's whole life.
    pub pid: Option<i64>,
    writer: tokio::sync::Mutex<OwnedWritePty>,
    /// Asks the reaper to kill the child.
    kill: Arc<Notify>,
    /// Flips once the reaper has collected the child.
    exited: watch::Receiver<bool>,
    screen: Arc<Mutex<vt100::Parser>>,
    output: broadcast::Sender<Bytes>,
    /// What the pump believes about a dialog, and when it started believing it.
    dialog: Mutex<(Dialog, Instant)>,
    /// The last prompt this session reported submitting, whitespace squashed.
    /// Sent by the `UserPromptSubmit` hook; awaited by [`Agent::paste`], which
    /// is how a message is known to have arrived.
    submitted: watch::Sender<Option<String>>,
    /// Whether this session's `SessionStart` hook has reported in. `false`
    /// until it does, which is how the manager tells a session that is up from
    /// one still held by a dialog.
    ready: watch::Sender<bool>,
    /// The real-time waits this agent makes, from `Settings`.
    timings: Timings,
}

impl Agent {
    /// Opens a pty, attaches `cmd` to it, and hands back the agent together with
    /// the reader its pump will own.
    ///
    /// NB: takes a prepared command rather than a card. Turning a card into a
    /// command line is the manager's job; this side knows about a process and a
    /// terminal and nothing else.
    ///
    /// Must be called from within the runtime: the pty registers with its
    /// reactor, and the child's reaper is a task on it.
    pub(crate) fn attach(cmd: Command, timings: Timings) -> Result<(Arc<Self>, OwnedReadPty)> {
        let (pty, pts) = pty_process::open().context("opening a pty")?;
        pty.resize(Size::new(DEFAULT_ROWS, DEFAULT_COLS))
            .context("sizing the pty")?;

        // NB: `kill_on_drop` covers a runtime going down before the reaper has
        // had a chance to act on a kill.
        let mut child = cmd
            .kill_on_drop(true)
            .spawn(pts)
            .context("spawning claude")?;

        let pid = child.id().map(i64::from);
        let (reader, writer) = pty.into_split();
        let (exited_tx, exited) = watch::channel(false);
        let kill = Arc::new(Notify::new());

        let (output, _) = broadcast::channel(1024);
        let screen = Arc::new(Mutex::new(vt100::Parser::new(
            DEFAULT_ROWS,
            DEFAULT_COLS,
            SCROLLBACK,
        )));

        let agent = Arc::new(Agent {
            pid,
            writer: tokio::sync::Mutex::new(writer),
            kill: kill.clone(),
            exited,
            screen,
            output,
            dialog: Mutex::new((Dialog::None, Instant::now())),
            submitted: watch::Sender::new(None),
            ready: watch::Sender::new(false),
            timings,
        });

        // The only owner of the child, so the only thing that reaps it. Without
        // the `wait` an exited agent lingers as a zombie for as long as this
        // server runs.
        tokio::spawn(async move {
            tokio::select! {
                _ = child.wait() => {}
                _ = kill.notified() => {
                    let _ = child.kill().await;
                }
            }
            let _ = exited_tx.send(true);
        });

        Ok((agent, reader))
    }

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

    pub async fn write_input(&self, bytes: &[u8]) {
        self.write_run(&[bytes]).await;
    }

    /// Writes `parts` as one uninterrupted run.
    ///
    /// NB: one acquisition for the lot. The websocket writes through the same
    /// lock, so a keystroke could otherwise land between a paste and the submit
    /// key behind it — appended to the composer and sent with the message, or an
    /// Enter of the user's own submitting the paste early and leaving ours to
    /// fire at an empty box. The client reads its input in order, so a run that
    /// cannot be split cannot be interleaved.
    async fn write_run(&self, parts: &[&[u8]]) {
        let mut writer = self.writer.lock().await;
        for part in parts {
            let _ = writer.write_all(part).await;
        }
        let _ = writer.flush().await;
    }

    /// Records that this session's `SessionStart` hook has reported in.
    ///
    /// NB: `send_replace`, not `send`. A `watch` sender with nothing subscribed
    /// to it counts as closed and `send` drops the value on the floor — and this
    /// almost always lands before anything is waiting to hear it.
    pub fn mark_ready(&self) {
        let _ = self.ready.send_replace(true);
    }

    /// Whether this session has reported in yet.
    pub fn is_ready(&self) -> bool {
        *self.ready.borrow()
    }

    /// Sends `text` to the session as one message.
    ///
    /// Claude Code's TUI reads a bare newline as submit, so the text goes in as
    /// a bracketed paste followed by `\r`. What makes that safe is the two
    /// checks around it, neither of which reads the message itself: the input
    /// box has to be *empty* first, so the submit key cannot send somebody's
    /// half-typed line along with this or instead of it; and the session's own
    /// `UserPromptSubmit` hook has to come back carrying the text, which is what
    /// says it arrived.
    ///
    /// NB: nothing looks for the message on screen, deliberately. A multi-line
    /// paste is collapsed to `[Pasted text #1 +N lines]`, which says nothing
    /// about *whose* paste it is — so a check that waited to see this one in the
    /// box answered to anybody's, and sent theirs.
    pub async fn paste(&self, text: &str) -> Paste {
        let text = text.trim_end();
        if text.is_empty() {
            return Paste::Submitted;
        }

        // Hook-derived, and stronger than anything on screen: the `SessionStart`
        // ping lands only once the client is up and past the workspace-trust and
        // `bypassPermissions` dialogs — the two the submit key would answer "No,
        // exit", and the two drawn in a shape no screen check is sure to know.
        if !self.await_ready().await {
            return Paste::NoBox;
        }
        match self.await_composer().await {
            Composer::Empty => {}
            Composer::Occupied => return Paste::Occupied,
            Composer::Dialog | Composer::Missing => return Paste::NoBox,
        }

        // NB: subscribed before the slot is cleared, and cleared before the
        // first write. The same batch sent twice is the same string, so a
        // leftover would confirm the second send on the strength of the first.
        let wanted = squash(text);
        let mut acks = self.submitted.subscribe();
        self.submitted.send_replace(None);

        let mut payload = Vec::with_capacity(text.len() + 16);
        payload.extend_from_slice(b"\x1b[200~");
        payload.extend_from_slice(text.as_bytes());
        payload.extend_from_slice(b"\x1b[201~");

        for _ in 0..PASTE_ATTEMPTS {
            // END first, and all three in one run. A box the cursor check read
            // as empty can still hold a draft somebody left with the cursor
            // parked at its start, and this puts the paste after their text
            // rather than in front of it. On an empty box it does nothing —
            // measured against the client, which honours `CSI F` and redraws
            // nothing for it.
            self.write_run(&[END, &payload, b"\r"]).await;

            match self.await_submitted(&mut acks, &wanted).await {
                Submission::Ours => return Paste::Submitted,
                Submission::Merged => return Paste::Merged,
                Submission::Foreign => return Paste::Displaced,
                Submission::Nothing => {}
            }

            // Nothing was submitted, and the box says which half went missing.
            // Empty: the paste was dropped mid-redraw and the submit key landed
            // on nothing, so the loop can send the whole thing again. Holding
            // something: this text is sitting in it and only the key went
            // astray, so send that rather than a second copy of the message.
            if self.composer() != Composer::Empty {
                self.write_run(&[b"\r"]).await;
                return match self.await_submitted(&mut acks, &wanted).await {
                    Submission::Ours => Paste::Submitted,
                    Submission::Merged => Paste::Merged,
                    Submission::Foreign => Paste::Displaced,
                    Submission::Nothing => Paste::Unconfirmed,
                };
            }
        }
        Paste::Unconfirmed
    }

    /// Records a prompt the session says it submitted.
    ///
    /// NB: `send_replace` for the same reason as [`Agent::mark_ready`] — most
    /// prompts are the user's and nothing is waiting on them.
    pub fn record_prompt(&self, prompt: &str) {
        let _ = self.submitted.send_replace(Some(squash(prompt)));
    }

    /// Waits for the session to report submitting a prompt, and says whether it
    /// was the one asked for.
    ///
    /// A prompt that is *not* ours is worth more than silence: it says the box
    /// held something the emptiness check missed and the submit key has just
    /// sent it. Nothing can take that back, but reporting it beats reporting a
    /// message that simply never arrived.
    ///
    /// NB: `contains` on squashed whitespace rather than equality. What comes
    /// back is what the composer held, and the client is free to fold a paste's
    /// newlines on the way in; nothing here should turn that into a lost message.
    async fn await_submitted(
        &self,
        acks: &mut watch::Receiver<Option<String>>,
        wanted: &str,
    ) -> Submission {
        let until = Deadline::now() + self.timings.submit_grace;

        loop {
            // Cloned out: a `watch::Ref` is a lock, and this waits.
            if let Some(seen) = acks.borrow_and_update().clone() {
                return match seen.as_str() {
                    // NB: `contains` and then a length test, rather than
                    // equality alone. Equality is what says the box held
                    // nothing else; falling back to `contains` means a client
                    // that pads or decorates the prompt reports a merge rather
                    // than a message that never arrived.
                    s if s == wanted => Submission::Ours,
                    s if s.contains(wanted) => Submission::Merged,
                    _ => Submission::Foreign,
                };
            }

            // The clearing `paste` does before its first write lands here as a
            // change of its own, which is why this loops rather than taking the
            // first wake as an answer.
            if timeout_at(until, acks.changed()).await.is_err() {
                return Submission::Nothing;
            }
        }
    }

    /// What the terminal is currently offering.
    fn composer(&self) -> Composer {
        composer_state(self.screen.lock().unwrap().screen())
    }

    /// Waits for the session's `SessionStart` ping, and says whether it came.
    ///
    /// The same budget `settled` gives a starting session, for the same reason:
    /// a resumed card is often still drawing itself when a send is asked for.
    async fn await_ready(&self) -> bool {
        let until = Deadline::now() + self.timings.startup_timeout;
        let mut pings = self.ready.subscribe();

        loop {
            if *pings.borrow_and_update() {
                return true;
            }
            if timeout_at(until, pings.changed()).await.is_err() {
                return false;
            }
        }
    }

    /// Waits for the client to draw its input box, and says what it drew.
    ///
    /// Only `Missing` is worth waiting on, and `startup_timeout` is the budget —
    /// the same one `settled` gives a starting session, because that is the same
    /// question. A dialog will not turn into a box by being waited on, and a box
    /// with a draft in it is the user's to clear.
    ///
    /// NB: woken by the pty's own output rather than a tick. The screen can only
    /// change when bytes arrive, and the pump feeds the parser *before* it fans a
    /// chunk out here — so a chunk means the screen has already moved, and a
    /// quiet terminal costs nothing to wait on.
    async fn await_composer(&self) -> Composer {
        let until = Deadline::now() + self.timings.startup_timeout;
        let mut output = self.output.subscribe();

        loop {
            let state = self.composer();
            if state != Composer::Missing {
                return state;
            }

            match timeout_at(until, output.recv()).await {
                // Dropped chunks are no loss: the screen is read, not replayed.
                Ok(Ok(_)) | Ok(Err(RecvError::Lagged(_))) => {}
                // Out of time, or the pty is finished. One last look either way.
                Err(_) | Ok(Err(RecvError::Closed)) => return self.composer(),
            }
        }
    }

    /// Whether a dialog is holding the keyboard, which is the user's to answer.
    pub fn is_blocked(&self) -> bool {
        let screen = self.screen.lock().unwrap();
        let contents = screen.screen().contents();
        is_dialog(&contents.lines().collect::<Vec<&str>>())
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

    pub async fn resize(&self, rows: u16, cols: u16) {
        let _ = self.writer.lock().await.resize(Size::new(rows, cols));
        self.screen
            .lock()
            .unwrap()
            .screen_mut()
            .set_size(rows, cols);
    }

    /// Kills the agent. Returns before it is gone; the reaper collects it.
    pub fn kill(&self) {
        // NB: `notify_one` keeps a permit, so a kill that lands before the
        // reaper first polls is not lost.
        self.kill.notify_one();
    }

    pub fn is_running(&self) -> bool {
        !*self.exited.borrow()
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

/// Feeds pty output into the screen and out to the websocket, watches for a
/// dialog leaving that screen, and reports the pty closing.
///
/// The dialog watch lives here because there is no hook for a permission being
/// answered: the redraw that takes the dialog away is the only signal, and this
/// is the only place it is seen. The same is true of the child going away.
///
/// NB: both are reported through a `Weak`, not called directly. That is what
/// keeps this module clear of the database and the event bus, and it makes a
/// cycle between the pump and the registry that owns this agent impossible
/// rather than merely absent.
pub(crate) async fn pump(
    mut reader: OwnedReadPty,
    agent: Arc<Agent>,
    watcher: Weak<dyn Watcher>,
    card_id: i64,
) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let chunk = Bytes::copy_from_slice(&buf[..n]);
                agent.screen.lock().unwrap().process(&chunk);

                if agent.watch_dialog() {
                    if let Some(watcher) = watcher.upgrade() {
                        watcher.dialog_cleared(card_id);
                    }
                }

                // No subscribers is the normal case when nobody has the card open.
                let _ = agent.output.send(chunk);
            }
        }
    }

    // EOF. A failed upgrade means the server is going down, which is the one
    // case where nobody needs telling.
    if let Some(watcher) = watcher.upgrade() {
        watcher.exited(card_id);
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
    match composer_anchor(lines) {
        Some(at) => &lines[at..],
        None => &[],
    }
}

/// Which row the input box's marker is on, if it is on screen.
fn composer_anchor(lines: &[&str]) -> Option<usize> {
    let window = lines.len().saturating_sub(COMPOSER_ROWS);
    lines
        .iter()
        .rposition(|line| is_prompt_line(line))
        .filter(|at| *at >= window)
}

/// Whether a line is one the TUI takes input on: its input box, or a dialog's
/// highlighted option. `❯` is what the current client draws; `>` is what older
/// ones — and the test stand-in — use.
fn is_prompt_line(line: &str) -> bool {
    matches!(line.trim_start().chars().next(), Some('❯' | '>'))
}

/// Whether a dialog is holding the keyboard, given the whole screen.
///
/// Two tests, because the dialogs that matter are drawn two different ways.
///
/// A mid-turn dialog — a tool permission, an MCP elicitation — replaces the
/// input box at the bottom of the screen, so it is found where the input box
/// would be, and it numbers its choices.
///
/// The two that hold a session at startup do neither. The workspace-trust
/// prompt and the `bypassPermissions` consent are drawn from the top of an
/// otherwise empty screen, nowhere near the last rows, and neither numbers
/// anything:
///
/// ```text
/// ❯ No, exit
///   Yes, I trust this folder
///
///   Enter to confirm · Esc to cancel
/// ```
///
/// So they are found by their footer instead, anywhere on screen. NB: matched
/// at the start of a line rather than anywhere in one, or an agent quoting the
/// words back into its transcript would read as a dialog. Every new worktree is
/// an unfamiliar directory, so the trust prompt is not an edge case — it is what
/// greets every card on its first start.
fn is_dialog(lines: &[&str]) -> bool {
    if lines.iter().any(|line| {
        let line = line.trim_start();
        line.starts_with("Enter to confirm") || line.starts_with("Esc to cancel")
    }) {
        return true;
    }

    composer_block(lines)
        .first()
        .is_some_and(|line| is_menu_option(line))
}

/// Collapses every run of whitespace to one space, so a prompt can be compared
/// against what was sent without caring how the client folded its newlines.
fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<&str>>().join(" ")
}

/// What the terminal is offering, as one answer rather than three questions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Composer {
    /// An input box with nothing in it: the only state a message can go into
    /// and be the only thing that goes.
    Empty,
    /// An input box holding something, which is not ours to send.
    Occupied,
    /// A dialog standing where the box would be. The user's to answer.
    Dialog,
    /// No box drawn yet.
    Missing,
}

/// Reads the screen once and says which of those it is.
///
/// Emptiness is asked of the *cursor*, not of the text. The cursor is the
/// insertion point, so it sits past anything already typed: text on the marker's
/// own row moves it along that row, and a draft beginning with a blank line
/// moves it off the row altogether. Reading the text instead meant deciding
/// whether each line below the marker was a message's continuation or the box's
/// own bottom border and mode footer — and the client draws a greyed `Try "…"`
/// hint into an empty box, which is text but is nobody's message. The text
/// cannot answer the other question either: a long paste collapses to
/// `[Pasted text #1 +N lines]`, a placeholder naming nobody, so a check that
/// waited to see *this* message appear answered to anybody's and sent theirs.
///
/// NB: the dialog test comes first and keeps its own answer, though the cursor
/// would refuse one anyway — the client parks it off the highlighted row. It is
/// what lets a refusal say the box is somebody's to answer rather than that it
/// has something in it, and it is the only live signal that a dialog is still up.
fn composer_state(screen: &vt100::Screen) -> Composer {
    let contents = screen.contents();
    let lines: Vec<&str> = contents.lines().collect();

    if is_dialog(&lines) {
        return Composer::Dialog;
    }
    let Some(at) = composer_anchor(&lines) else {
        return Composer::Missing;
    };

    let (row, col) = screen.cursor_position();
    if usize::from(row) == at && col == input_column(lines[at]) {
        Composer::Empty
    } else {
        Composer::Occupied
    }
}

/// The column a message's first character would occupy: past the marker and the
/// single space the client draws after it.
///
/// NB: one blank, not every blank that follows. Counting a padded row's
/// trailing spaces would put the insertion point off the end of the text, which
/// reads an empty box as one with something in it. And *any* blank: the client
/// separates the marker from the text with a non-breaking space, so matching
/// `' '` alone put the column one short of the cursor and read every empty box
/// as occupied.
fn input_column(line: &str) -> u16 {
    let mut col = 0;
    let mut chars = line.chars().peekable();

    while chars.peek().is_some_and(|c| c.is_whitespace()) {
        chars.next();
        col += 1;
    }
    if chars.next_if(|c| matches!(c, '❯' | '>')).is_none() {
        return col;
    }
    col += 1;
    if chars.next_if(|c| c.is_whitespace()).is_some() {
        col += 1;
    }
    col
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

#[cfg(test)]
mod tests {
    use super::{
        composer_block, composer_state, history_bytes, input_column, is_dialog, is_menu_option,
        is_prompt_line, settle, squash, Composer, Dialog, COMPOSER_ROWS,
    };
    use std::time::Duration;

    fn dialog(lines: &[&str]) -> bool {
        is_dialog(lines)
    }

    /// The same, but through a real screen: the rows are painted where the
    /// client paints them and read back the way `Agent::is_blocked` reads them.
    /// Anchoring at the bottom of the screen is exactly what a plain slice of
    /// lines cannot catch, so the startup dialogs need this.
    fn dialog_on_screen(rows: &[&str]) -> bool {
        let mut parser = vt100::Parser::new(40, 120, 0);
        parser.process(b"\x1b[2J\x1b[H");
        for row in rows {
            parser.process(format!("{row}\r\n").as_bytes());
        }

        let contents = parser.screen().contents();
        is_dialog(&contents.lines().collect::<Vec<&str>>())
    }

    /// `composer_state` over a real screen, with the cursor left wherever the
    /// rows happened to end — which is where a client that has just drawn them
    /// leaves it.
    fn state_on_screen(rows: &[&str]) -> Composer {
        let mut parser = vt100::Parser::new(40, 120, 0);
        parser.process(b"\x1b[2J\x1b[H");
        for row in rows {
            parser.process(format!("{row}\r\n").as_bytes());
        }
        composer_state(parser.screen())
    }

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

        // The live view must be back, or the dialog watcher reads the wrong rows.
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

    /// An empty input box, as the client draws it: the marker inside a bordered
    /// box with the session name on the border.
    const COMPOSING: &[&str] = &[
        "  The board flashes whenever the poll returns",
        "  ⏺ Worked for 1s",
        "─────────────────────────────────── spike-a ─",
        "❯ ",
        "────────────────────────────────────────────",
        "  -- INSERT -- ⏸ plan mode on (shift+tab to cycle)",
    ];

    /// The workspace-trust prompt, verbatim from claude 2.1.272. Its options
    /// carry no numbers, which is the case the old check missed.
    const TRUST: &[&str] = &[
        "  Quick safety check: Is this a project you created or one you trust?",
        "  Security guide",
        "❯ No, exit",
        "  Yes, I trust this folder",
        "",
        "  Enter to confirm · Esc to cancel",
    ];

    /// The `bypassPermissions` consent, also unnumbered.
    const CONSENT: &[&str] = &[
        "  WARNING: Claude Code running in Bypass Permissions mode",
        "  https://code.claude.com/docs/en/security",
        "❯ No, exit",
        "  Yes, I accept",
        "",
        "  Enter to confirm · Esc to cancel",
    ];

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

    /// Paints `rows` from the top of a real screen and leaves the cursor at
    /// `(row, col)`, the way the client leaves it on its insertion point.
    fn screen_with_cursor(rows: &[&str], row: u16, col: u16) -> vt100::Parser {
        let mut parser = vt100::Parser::new(40, 120, 0);
        parser.process(b"\x1b[2J\x1b[H");
        for line in rows {
            parser.process(format!("{line}\r\n").as_bytes());
        }
        // 1-based, as the escape is.
        parser.process(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());
        parser
    }

    /// Where the marker sits on a real screen, given the rows above it.
    fn marker_row(rows: &[&str]) -> u16 {
        rows.iter()
            .rposition(|line| is_prompt_line(line))
            .expect("a fixture with an input box") as u16
    }

    /// Whatever the box holds goes to the agent with the next submit key, so an
    /// empty one is the only state a message can be written into and be the only
    /// thing sent. The cursor is what says so: it is the insertion point, so it
    /// sits past anything already there.
    #[test]
    fn an_empty_box_is_one_whose_cursor_is_at_the_start() {
        let at = marker_row(COMPOSING);
        let parser = screen_with_cursor(COMPOSING, at, 2);
        assert_eq!(composer_state(parser.screen()), Composer::Empty);

        // The same box, cursor moved along it: something has been typed.
        let parser = screen_with_cursor(COMPOSING, at, 9);
        assert_eq!(composer_state(parser.screen()), Composer::Occupied);

        // And a box the client has just emptied by submitting what was in it.
        let at = marker_row(SUBMITTED);
        let parser = screen_with_cursor(SUBMITTED, at, 2);
        assert_eq!(composer_state(parser.screen()), Composer::Empty);
    }

    /// The case the text check could not see: the marker's own row is blank and
    /// the draft is on the row below, where a bottom border and a mode footer
    /// also live. The cursor is on that row, which settles it without having to
    /// tell a continuation line from the client's furniture.
    #[test]
    fn a_draft_below_the_marker_is_not_an_empty_box() {
        let rows = &[
            "────────────────────────────────────────────",
            "❯ ",
            "  second line only",
            "────────────────────────────────────────────",
        ];
        let parser = screen_with_cursor(rows, 2, 18);
        assert_eq!(composer_state(parser.screen()), Composer::Occupied);
        // And the same rows with the cursor back on the marker: empty.
        let parser = screen_with_cursor(rows, 1, 2);
        assert_eq!(composer_state(parser.screen()), Composer::Empty);
    }

    /// The hint the client draws into an empty box is text, and is nobody's
    /// message. Reading the row would call this occupied and refuse every send.
    #[test]
    fn the_clients_own_hint_is_still_an_empty_box() {
        let rows = &[
            "────────────────────────────────────────────",
            "❯ Try \"write a test for _drawer_card.html\"",
            "────────────────────────────────────────────",
        ];
        let parser = screen_with_cursor(rows, 1, 2);
        assert_eq!(composer_state(parser.screen()), Composer::Empty);
    }

    /// A screen with no box at all is not an empty one.
    #[test]
    fn a_client_still_drawing_itself_has_no_empty_box() {
        let rows = &["  Loading…", "  ─────────"];
        let parser = screen_with_cursor(rows, 0, 0);
        assert_eq!(composer_state(parser.screen()), Composer::Missing);
    }

    #[test]
    fn the_input_column_is_past_the_marker_and_one_blank() {
        // Verbatim from claude 2.1.276: the blank after the marker is U+00A0,
        // and taking it for an ordinary space is not optional — matching `' '`
        // alone left the column one short of where the client puts the cursor,
        // so every empty box read as one with something in it.
        assert_eq!(input_column("❯\u{a0}"), 2);
        assert_eq!(input_column("❯\u{a0}hello world this is a draft"), 2);

        assert_eq!(input_column("❯ "), 2);
        assert_eq!(input_column("> "), 2);
        // Indented markers count their indent; a row of padding does not.
        assert_eq!(input_column("  ❯ "), 4);
        assert_eq!(input_column("❯     "), 2);
    }

    #[test]
    fn an_echo_of_an_earlier_turn_is_left_above_the_anchor() {
        assert!(!composer_block(PASTED)
            .iter()
            .any(|l| l.contains("an earlier turn")));
    }

    /// A prompt comes back as the composer held it, and the client is free to
    /// fold a paste's newlines on the way in. Comparing on squashed whitespace
    /// is what keeps that from reading as a message that never arrived.
    #[test]
    fn a_prompt_matches_however_its_newlines_were_folded() {
        let sent = "Code review on main:\n\nmain.rs:3 (after)\nSay hello instead.";
        assert_eq!(
            squash(sent),
            "Code review on main: main.rs:3 (after) Say hello instead."
        );
        assert!(squash(&sent.replace('\n', "\r")).contains(&squash(sent)));
        assert!(squash(&format!("{sent}\n\nAddress each comment.")).contains(&squash(sent)));
    }

    #[test]
    fn a_client_with_no_input_box_yet_has_no_composer() {
        let booting = &["  Loading…", "  ─────────", "  starting up"];
        assert!(composer_block(booting).is_empty());
    }

    #[test]
    fn an_input_box_is_not_a_dialog() {
        assert!(!dialog(COMPOSING));
    }

    /// Both dialogs that strand a card at startup, and neither numbers its
    /// options. Reading them as an input box is what left the card in
    /// `starting` with nobody told there was anything to answer.
    #[test]
    fn the_unnumbered_startup_dialogs_are_dialogs() {
        assert!(dialog(TRUST));
        assert!(dialog(CONSENT));
    }

    /// The case a slice of lines hides: the client draws these from the *top* of
    /// an otherwise empty 40-row screen, so on the real screen they sit twenty
    /// rows above where the input box would be. Looking only at the last rows
    /// found nothing, and a card meeting the trust prompt — which is every card
    /// on its first start, the worktree being a directory nobody has seen
    /// before — went to `misconfigured` instead of `needs you`.
    #[test]
    fn a_startup_dialog_is_found_though_it_is_nowhere_near_the_input_box() {
        assert!(dialog_on_screen(TRUST));
        assert!(dialog_on_screen(CONSENT));
    }

    #[test]
    fn an_input_box_on_a_real_screen_is_still_not_a_dialog() {
        assert!(!dialog_on_screen(COMPOSING));
    }

    /// A client that has painted nothing yet is starting up, not waiting.
    #[test]
    fn a_blank_screen_is_not_a_dialog() {
        assert!(!dialog_on_screen(&["", "  Loading…", ""]));
    }

    /// Nothing may be pasted into a dialog: it discards the text, and the
    /// submit key behind it answers the dialog instead — which on both startup
    /// prompts means "No, exit". Neither numbers its options, so a gate built on
    /// `is_menu_option` alone read them as an input box; this one asks
    /// `is_dialog`.
    #[test]
    fn a_dialog_is_not_something_to_paste_into() {
        assert_eq!(state_on_screen(TRUST), Composer::Dialog);
        assert_eq!(state_on_screen(CONSENT), Composer::Dialog);
        assert_eq!(
            state_on_screen(&["  Bash command needs approval", "❯ 1. Yes", "  2. No"]),
            Composer::Dialog
        );
        // And a screen with nothing drawn on it is neither.
        assert_eq!(state_on_screen(&["  Loading…"]), Composer::Missing);
    }

    #[test]
    fn a_numbered_dialog_is_still_a_dialog() {
        let numbered = &["  Bash command needs approval", "❯ 1. Yes", "  2. No"];
        assert!(dialog(numbered));
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

    /// A message quoting the footer is transcript, not a dialog: the anchor
    /// leaves it above the block.
    #[test]
    fn the_words_in_a_message_above_the_box_do_not_make_a_dialog() {
        let quoting = &[
            "  ⏺ It said Enter to confirm · Esc to cancel, so I pressed Enter.",
            "────────────────────────────────────────────",
            "❯ ",
            "────────────────────────────────────────────",
        ];
        assert!(!dialog(quoting));
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

    /// A client connecting late has to get the input modes back and not only the
    /// pixels: a pane handed a screen without them takes a paste unbracketed and
    /// drops every click, on a session that was working before the tab closed.
    #[test]
    fn a_snapshot_replays_bracketed_paste_and_the_mouse_protocol() {
        let mut parser = vt100::Parser::new(10, 40, 100);
        parser.process(b"\x1b[?2004h\x1b[?1000h");

        let out = parser.screen().state_formatted();
        assert!(out.windows(8).any(|w| w == b"\x1b[?2004h"));
        assert!(out.windows(8).any(|w| w == b"\x1b[?1000h"));
    }

    // ---- frames recorded from the real client ------------------------------

    /// Paints a recorded frame back onto a screen.
    ///
    /// NB: each row is positioned rather than newline-separated. A newline on
    /// the last row scrolls the screen, and every row then sits one above where
    /// the cursor the fixture recorded is put.
    fn replay(fixture: &str) -> vt100::Parser {
        let (head, rows) = fixture
            .split_once("\n--\n")
            .expect("a cursor line, then rows");
        let mut fields = head.split_whitespace();
        assert_eq!(fields.next(), Some("cursor"));
        let row: u16 = fields.next().unwrap().parse().unwrap();
        let col: u16 = fields.next().unwrap().parse().unwrap();

        let mut parser = vt100::Parser::new(40, 120, 0);
        parser.process(b"\x1b[2J\x1b[H");
        for (i, line) in rows.lines().enumerate() {
            parser.process(format!("\x1b[{};1H{line}", i + 1).as_bytes());
        }
        parser.process(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());
        parser
    }

    /// Every frame, and what the heuristics are meant to make of it.
    ///
    /// These are screens the real client drew, which is the whole point: the
    /// marker is followed by a non-breaking space and an empty box carries a
    /// greyed hint, and a fixture somebody typed had neither. A failure here is
    /// the client having moved — re-record with
    /// `cargo test -- --ignored record_frames` and read the diff before
    /// trusting it.
    const FRAMES: &[(&str, &str, Composer)] = &[
        (
            "empty-box",
            include_str!("frames/empty-box.txt"),
            Composer::Empty,
        ),
        // The same box after ctrl+u, which the client honours.
        (
            "cleared",
            include_str!("frames/cleared.txt"),
            Composer::Empty,
        ),
        (
            "draft",
            include_str!("frames/draft.txt"),
            Composer::Occupied,
        ),
        (
            "draft-cursor-at-end",
            include_str!("frames/draft-cursor-at-end.txt"),
            Composer::Occupied,
        ),
        (
            "trust-dialog",
            include_str!("frames/trust-dialog.txt"),
            Composer::Dialog,
        ),
    ];

    #[test]
    fn recorded_frames_read_as_they_should() {
        for (name, fixture, want) in FRAMES {
            let parser = replay(fixture);
            assert_eq!(composer_state(parser.screen()), *want, "{name}");
        }
    }

    /// The one frame that is read wrong, recorded so the gap is a fact rather
    /// than a remark.
    ///
    /// `CSI H` puts the cursor back to the start of a draft, and the insertion
    /// point is then exactly where an empty box's would be. Nothing on the
    /// screen separates the two. What covers it is downstream: `END` goes in
    /// ahead of the paste so the message lands after the draft rather than in
    /// front of it, and the acknowledgement carries both, which reports as
    /// `Paste::Merged` instead of a clean send.
    #[test]
    fn a_draft_with_its_cursor_at_the_start_reads_as_empty() {
        let parser = replay(include_str!("frames/draft-cursor-at-start.txt"));
        assert_eq!(composer_state(parser.screen()), Composer::Empty);
    }

    /// Rewrites `FRAMES` from a live client. Ignored: it needs `claude` on the
    /// path, a directory the client already trusts (this repository), and about
    /// half a minute.
    ///
    /// The trust prompt is not among what it records — a trusted directory will
    /// not draw one. That fixture came from a fresh `mkdtemp`, and re-recording
    /// it means doing the same by hand.
    #[test]
    #[ignore = "drives the real client"]
    fn record_frames() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let root = env!("CARGO_MANIFEST_DIR");
            let mut cmd = Command::new("claude");
            cmd = cmd.arg("--permission-mode").arg("plan");
            cmd = cmd.current_dir(root);
            cmd = cmd.env("TERM", "xterm-256color");
            // The same markers `AgentManager::command` strips. Leaving any of
            // them makes the client think it is a nested agent, and it draws a
            // warning row about it that the recording would then carry.
            for marker in [
                "CLAUDECODE",
                "CLAUDE_CODE_CHILD_SESSION",
                "CLAUDE_CODE_ENTRYPOINT",
                "CLAUDE_CODE_SSE_PORT",
                "CLAUDE_SESSION_ID",
            ] {
                cmd = cmd.env_remove(marker);
            }

            let (agent, reader) = Agent::attach(cmd, brisk()).unwrap();
            let watcher: Arc<dyn Watcher> = Arc::new(Quiet);
            tokio::spawn(pump(
                reader,
                Arc::clone(&agent),
                Arc::downgrade(&watcher),
                1,
            ));

            // The client spends seconds drawing itself.
            for _ in 0..150 {
                if agent.composer() != Composer::Missing {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }

            let draft = "hello world this is a draft";
            for (name, keys) in [
                ("empty-box", None),
                ("draft", Some(format!("\x1b[200~{draft}\x1b[201~"))),
                ("draft-cursor-at-start", Some("\x1b[H".to_owned())),
                ("draft-cursor-at-end", Some("\x1b[F".to_owned())),
                ("cleared", Some("\x15".to_owned())),
            ] {
                if let Some(keys) = keys {
                    agent.write_input(keys.as_bytes()).await;
                }
                tokio::time::sleep(Duration::from_millis(2500)).await;
                record(root, name, agent.screen.lock().unwrap().screen());
            }
            agent.kill();
        });
    }

    /// Writes one frame, leaving out rows that say what account this was.
    fn record(root: &str, name: &str, screen: &vt100::Screen) {
        let noise = [
            "Your login expires",
            "Claude Max",
            "Claude Pro",
            "gh auth login",
        ];
        let contents = screen.contents();
        let mut rows: Vec<&str> = contents
            .lines()
            .map(|line| {
                if noise.iter().any(|n| line.contains(n)) {
                    ""
                } else {
                    line
                }
            })
            .collect();
        while rows.last().is_some_and(|r| r.trim().is_empty()) {
            rows.pop();
        }

        let (row, col) = screen.cursor_position();
        let body = format!("cursor {row} {col}\n--\n{}\n", rows.join("\n"));
        std::fs::write(format!("{root}/src/agent/frames/{name}.txt"), body).unwrap();
        println!("recorded {name}: cursor=({row},{col})");
    }

    // ---- the stand-in, against the same heuristics -------------------------

    /// Attaches the end-to-end suite's own stand-in, with nothing to submit and
    /// no hooks to call.
    ///
    /// The point is that one stand-in answers to the recordings above. It is
    /// what every end-to-end assertion about delivery runs against, so a screen
    /// it draws that the heuristics read differently from the real client's is a
    /// suite that passes while the product is broken — which is how the
    /// non-breaking space after the marker got in.
    async fn stand_in() -> (Arc<Agent>, Arc<dyn Watcher>) {
        let dir = std::env::temp_dir().join(format!("ledecky-stand-in-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = format!("{}/tests/fake-agent.mjs", env!("CARGO_MANIFEST_DIR"));

        let mut cmd = Command::new("node");
        cmd = cmd.arg(&script);
        // An empty settings object leaves every hook URL undefined, so the
        // stand-in's `hook()` is a no-op and nothing has to be listening.
        cmd = cmd.arg("--settings").arg("{}");
        cmd = cmd.current_dir(&dir);
        cmd = cmd.env("TERM", "xterm-256color");
        cmd = cmd.env("XDG_DATA_HOME", &dir);
        cmd = cmd.env("FAKE_AGENT_BOOT_MS", "150");

        let (agent, reader) = Agent::attach(cmd, brisk()).unwrap();
        let watcher: Arc<dyn Watcher> = Arc::new(Quiet);
        tokio::spawn(pump(
            reader,
            Arc::clone(&agent),
            Arc::downgrade(&watcher),
            1,
        ));
        (agent, watcher)
    }

    /// Waits for the stand-in's screen to read as `want`.
    async fn settles_to(agent: &Arc<Agent>, want: Composer) -> Composer {
        for _ in 0..200 {
            let state = agent.composer();
            if state == want {
                return state;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        agent.composer()
    }

    #[tokio::test]
    async fn the_stand_in_draws_what_the_recordings_do() {
        let (agent, _w) = stand_in().await;

        // Boots with nothing in its box, like `frames/empty-box.txt`.
        assert_eq!(settles_to(&agent, Composer::Empty).await, Composer::Empty);

        // Holds a draft the same way `frames/draft.txt` does.
        agent
            .write_input(b"\x1b[200~hello world this is a draft\x1b[201~")
            .await;
        assert_eq!(
            settles_to(&agent, Composer::Occupied).await,
            Composer::Occupied
        );

        // And puts a dialog where the box was, like `frames/trust-dialog.txt`.
        // The marker is the stand-in's way of being asked for one.
        agent.write_input(b"\r").await;
        agent
            .write_input(b"\x1b[200~[needs-permission] run it\x1b[201~")
            .await;
        agent.write_input(b"\r").await;
        assert_eq!(settles_to(&agent, Composer::Dialog).await, Composer::Dialog);

        agent.kill();
    }

    // ---- the paste itself, against a real pty -------------------------------

    use super::{pump, Agent, Paste, Watcher};
    use crate::config::Timings;
    use pty_process::Command;
    use std::sync::Arc;

    /// A watcher that wants nothing. `pump` reports a cleared dialog and an EOF
    /// through it, and neither is what these tests are about.
    struct Quiet;
    impl Watcher for Quiet {
        fn dialog_cleared(&self, _: i64) {}
        fn exited(&self, _: i64) {}
    }

    /// Timings small enough that a refusal costs milliseconds, with the submit
    /// grace well clear of the poll so a confirmation cannot be raced.
    fn brisk() -> Timings {
        Timings {
            hook_grace: Duration::from_millis(50),
            startup_timeout: Duration::from_millis(60),
            dialog_grace: Duration::from_millis(50),
            submit_grace: Duration::from_millis(400),
        }
    }

    /// A full screen with `rows` at the bottom and the cursor left at
    /// `(cursor_row, cursor_col)` — counted within `rows`, as the client leaves
    /// it on its insertion point.
    fn frame(rows: &[&str], cursor_row: usize, cursor_col: u16) -> Vec<u8> {
        let top = 40 - rows.len();
        let mut out = b"\x1b[2J\x1b[H".to_vec();
        for _ in 0..top {
            out.extend_from_slice(b"\r\n");
        }
        // NB: no newline after the last row. A fortieth one on a forty-row
        // screen scrolls it, and every row — the input box included — would sit
        // one above where the cursor is then put.
        for (i, row) in rows.iter().enumerate() {
            out.extend_from_slice(row.as_bytes());
            if i + 1 < rows.len() {
                out.extend_from_slice(b"\r\n");
            }
        }
        // 1-based, as the escape is.
        let (row, col) = (top + cursor_row + 1, cursor_col + 1);
        out.extend_from_slice(format!("\x1b[{row};{col}H").as_bytes());
        out
    }

    /// An agent attached to a stand-in that paints `screen` and then sits still.
    ///
    /// `sleep` rather than anything interactive: what `paste` reacts to is the
    /// screen and `record_prompt`, both of which a test drives, so the child only
    /// has to hold the pty open and leave the frame alone. The watcher comes back
    /// with it because `pump` holds only a `Weak`.
    async fn attached(screen: Vec<u8>) -> (Arc<Agent>, Arc<dyn Watcher>) {
        // NB: a counter, not the thread id. That formats as `ThreadId(2)`, and
        // the parentheses end the shell's word.
        static NTH: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let nth = NTH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("ledecky-frame-{}-{nth}.bin", std::process::id()));
        std::fs::write(&path, &screen).unwrap();

        let mut cmd = Command::new("sh");
        cmd = cmd.arg("-c");
        cmd = cmd.arg(format!("cat {}; sleep 120", path.display()));
        let (agent, reader) = Agent::attach(cmd, brisk()).unwrap();

        let watcher: Arc<dyn Watcher> = Arc::new(Quiet);
        tokio::spawn(pump(
            reader,
            Arc::clone(&agent),
            Arc::downgrade(&watcher),
            1,
        ));

        // The frame has to be on the screen before anything asks about it.
        for _ in 0..200 {
            if !agent
                .screen
                .lock()
                .unwrap()
                .screen()
                .contents()
                .trim()
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        (agent, watcher)
    }

    const MESSAGE: &str = "Code review on main:\n\nmain.rs:3 (after)\nSay hello instead.";

    /// An empty box: a border, the marker, a border, the mode footer.
    fn empty_box() -> Vec<u8> {
        frame(
            &["────────────", "❯ ", "────────────", "  -- INSERT --"],
            1,
            2,
        )
    }

    /// Answers as the client's `UserPromptSubmit` hook would, for as long as the
    /// send is still running.
    ///
    /// NB: repeatedly. `paste` clears the slot before its first write, so a
    /// single answer timed against it would be a race; answering until it
    /// settles cannot be.
    fn acknowledge(agent: &Arc<Agent>, prompt: &'static str) -> tokio::task::JoinHandle<()> {
        let agent = Arc::clone(agent);
        tokio::spawn(async move {
            loop {
                agent.record_prompt(prompt);
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
    }

    #[tokio::test]
    async fn a_message_is_delivered_when_the_session_acknowledges_it() {
        let (agent, _w) = attached(empty_box()).await;
        agent.mark_ready();

        let answering = acknowledge(&agent, MESSAGE);
        assert_eq!(agent.paste(MESSAGE).await, Paste::Submitted);
        answering.abort();
    }

    /// Nothing says the message landed, so nothing may report it as sent.
    #[tokio::test]
    async fn a_message_nothing_acknowledges_is_unconfirmed() {
        let (agent, _w) = attached(empty_box()).await;
        agent.mark_ready();

        assert_eq!(agent.paste(MESSAGE).await, Paste::Unconfirmed);
    }

    /// A prompt came back, and it was not this one: the box held a draft the
    /// cursor did not give away and the submit key has just sent it. Worth
    /// saying so rather than reporting a message that never arrived.
    #[tokio::test]
    async fn a_prompt_that_is_not_ours_reads_as_displaced() {
        let (agent, _w) = attached(empty_box()).await;
        agent.mark_ready();

        let answering = acknowledge(&agent, "whatever was already in the box");
        assert_eq!(agent.paste(MESSAGE).await, Paste::Displaced);
        answering.abort();
    }

    /// The cursor is along the marker's row, so something is already typed.
    /// Writing now would send it with the message, and the box cannot say what
    /// it is — a collapsed paste names nobody.
    #[tokio::test]
    async fn a_box_with_something_in_it_is_refused() {
        let held = frame(
            &[
                "────────────",
                "❯ [Pasted text #1 +11 lines]",
                "────────────",
            ],
            1,
            28,
        );
        let (agent, _w) = attached(held).await;
        agent.mark_ready();

        assert_eq!(agent.paste(MESSAGE).await, Paste::Occupied);
    }

    /// The workspace-trust prompt, which numbers nothing and defaults to "No,
    /// exit" — so the submit key behind a paste would kill the session.
    #[tokio::test]
    async fn a_startup_dialog_is_refused() {
        let dialog = frame(
            &[
                "  Quick safety check: Is this a project you created or one you trust?",
                "❯ No, exit",
                "  Yes, I trust this folder",
                "",
                "  Enter to confirm · Esc to cancel",
            ],
            1,
            0,
        );
        let (agent, _w) = attached(dialog).await;
        agent.mark_ready();

        assert_eq!(agent.paste(MESSAGE).await, Paste::NoBox);
    }

    /// A session that has not reported in is either still drawing itself or held
    /// by a dialog drawn in a shape no screen check is sure to know. The ping is
    /// hook-derived and cannot be fooled by either.
    #[tokio::test]
    async fn a_session_that_has_not_reported_in_is_refused() {
        let (agent, _w) = attached(empty_box()).await;
        // Deliberately not `mark_ready`.
        assert_eq!(agent.paste(MESSAGE).await, Paste::NoBox);
    }
}
