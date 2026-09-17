use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{Context, Result};
use bytes::Bytes;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};
use tokio::sync::broadcast;

use crate::config::Settings;
use crate::project::Card;

const DEFAULT_ROWS: u16 = 40;
const DEFAULT_COLS: u16 = 120;
const SCROLLBACK: usize = 5000;

/// Paste delivery: up to PASTE_ATTEMPTS sends, each polled PASTE_CHECKS times.
const PASTE_POLL: std::time::Duration = std::time::Duration::from_millis(150);
const PASTE_CHECKS: u32 = 8;
const PASTE_ATTEMPTS: u32 = 4;

/// Rows at the bottom of the screen treated as the input box.
const COMPOSER_ROWS: usize = 15;

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
                std::thread::sleep(PASTE_POLL);
                continue;
            }

            // A paste written while the TUI is mid-redraw is simply dropped, so
            // re-send rather than waiting longer on one that never arrived. The
            // in-box check before each retry keeps it from landing twice.
            if !self.holds(&needle) {
                self.write_input(&payload);
            }

            for _ in 0..PASTE_CHECKS {
                std::thread::sleep(PASTE_POLL);
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

    /// The lines of the input box and of any dialog over it.
    ///
    /// NB: only the last few rows are searched, and only lines carrying the
    /// TUI's prompt marker. The transcript above echoes earlier messages, and
    /// the box's own border can carry a *queued* message — either would match a
    /// whole-screen search and fire the submit key at nothing.
    fn prompt_lines(&self) -> Vec<String> {
        let screen = self.screen.lock().unwrap();
        let contents = screen.screen().contents();

        let lines: Vec<&str> = contents.lines().collect();
        lines[lines.len().saturating_sub(COMPOSER_ROWS)..]
            .iter()
            .filter(|line| is_prompt_line(line))
            .map(|line| (*line).to_owned())
            .collect()
    }

    /// Whether there is an input box to paste into at all.
    ///
    /// A client that has not drawn one yet is still starting up, and a dialog
    /// over it leaves only its own highlighted options behind.
    pub fn is_composing(&self) -> bool {
        self.prompt_lines().iter().any(|line| !is_menu_option(line))
    }

    /// Whether a dialog is holding the keyboard, which is the user's to answer.
    pub fn is_blocked(&self) -> bool {
        self.prompt_lines().iter().any(|line| is_menu_option(line))
    }

    /// True once `needle`, or the placeholder the TUI collapses a long paste to,
    /// is in the input box.
    fn holds(&self, needle: &str) -> bool {
        self.prompt_lines()
            .iter()
            .any(|line| line.contains(needle) || line.contains("Pasted text"))
    }

    /// Waits for the input box to let go of the text, which is the only proof
    /// the submit key did anything.
    fn cleared(&self, needle: &str) -> bool {
        for _ in 0..PASTE_CHECKS {
            std::thread::sleep(PASTE_POLL);
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
        cmd.arg("--name");
        cmd.arg(&card.title);
        cmd.cwd(worktree);
        // Match what xterm.js renders; the inherited TERM may be anything.
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
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
            screen: screen.clone(),
            output: output.clone(),
            pending_prompt: Mutex::new(card.opening_prompt()),
        });

        // portable-pty hands back a blocking reader, so it gets its own thread.
        std::thread::spawn(move || pump(reader, screen, output));

        self.0.write().unwrap().insert(card.id, agent.clone());
        Ok(agent)
    }
}

fn pump(
    mut reader: Box<dyn Read + Send>,
    screen: Arc<Mutex<vt100::Parser>>,
    output: broadcast::Sender<Bytes>,
) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let chunk = Bytes::copy_from_slice(&buf[..n]);
                screen.lock().unwrap().process(&chunk);
                // No subscribers is the normal case when nobody has the card open.
                let _ = output.send(chunk);
            }
        }
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
    use super::{is_menu_option, is_prompt_line, paste_needle};

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
}
