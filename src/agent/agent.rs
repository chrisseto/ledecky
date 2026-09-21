use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::Bytes;
use pty_process::{Command, OwnedReadPty, OwnedWritePty, Size};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{broadcast, watch, Notify};

use crate::agent::messaging::Inbox;
use crate::config::Timings;

const DEFAULT_ROWS: u16 = 40;
const DEFAULT_COLS: u16 = 120;
const SCROLLBACK: usize = 5000;

/// Rows at the bottom of the screen treated as the input box.
const COMPOSER_ROWS: usize = 15;

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
    /// Where to send this session messages, once its `SessionStart` hook has
    /// said. `None` until then, which is also how the manager tells a session
    /// that is up from one still held by a dialog.
    inbox: Mutex<Option<Inbox>>,
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
            inbox: Mutex::new(None),
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
        let mut writer = self.writer.lock().await;
        let _ = writer.write_all(bytes).await;
        let _ = writer.flush().await;
    }

    /// Records where this session takes messages.
    pub fn set_inbox(&self, inbox: Inbox) {
        *self.inbox.lock().unwrap() = Some(inbox);
    }

    /// Where this session takes messages, if it has reported in yet.
    pub fn inbox(&self) -> Option<Inbox> {
        self.inbox.lock().unwrap().clone()
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
        composer_block, history_bytes, is_dialog, is_menu_option, is_prompt_line, settle, Dialog,
        COMPOSER_ROWS,
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

    #[test]
    fn an_echo_of_an_earlier_turn_is_left_above_the_anchor() {
        let pasted = &[
            "  ⏺ an earlier turn that also said Investigate",
            "────────────────────────────────────────────",
            "❯ Investigate the unpoly fragment swapping.",
            "────────────────────────────────────────────",
        ];
        assert!(!composer_block(pasted)
            .iter()
            .any(|l| l.contains("an earlier turn")));
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
}
