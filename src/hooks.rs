use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Write as _IoWrite};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use rand::rngs::SysRng;
use rand::TryRng;
use rocket::serde::Serialize;
use serde_json::{json, Map, Value};

/// Hook events we register over HTTP: the event, the URL segment it posts to,
/// and the matcher scoping it.
///
/// NB: `SessionStart` is not among them — an HTTP hook there silently never
/// fires. It is registered separately, as a `command` handler, by
/// `session_start_hook`; `session_id` is still read from whichever of these
/// lands first.
const EVENTS: &[(&str, &str, Option<&str>)] = &[
    ("UserPromptSubmit", "prompt", None),
    // NB: `Notification` rather than `PermissionRequest`, which sees tool
    // permissions only and answers from inside the permission flow, where a slow
    // reply stalls the turn. This one is fire-and-forget, and it also catches the
    // dialogs an MCP server puts up.
    //
    // The matcher earns its keep by leaving `idle_prompt` out: that fires a
    // minute into every idle card, which is not a card waiting on anybody.
    (
        "Notification",
        "needs-user",
        Some("permission_prompt|elicitation_dialog|elicitation_url_dialog|agent_needs_input"),
    ),
    ("Stop", "stop", None),
    ("SessionEnd", "end", None),
];

const HOOK_TIMEOUT_SECS: u32 = 10;

/// Subcommand a re-execution uses to report the session's inbox.
const SESSION_START: &str = "session-start";

/// First argument marking a re-execution as a hook rather than a server start.
const HOOK_ARG: &str = "hook";

/// How long the re-executed hook waits on the server. Short: it runs inside the
/// agent's `SessionStart`, and a hook that hangs holds the session open.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);

/// Per-process secret scoping hook callbacks to this server instance, so another
/// local process cannot forge turn boundaries.
pub struct HookAuth {
    token: String,
    port: AtomicU16,
    /// This binary, which the `SessionStart` hook re-executes. See `HOOK_ARG`.
    exe: String,
}

#[derive(Serialize)]
#[serde(crate = "rocket::serde")]
struct HttpHook {
    #[serde(rename = "type")]
    kind: &'static str,
    url: String,
    timeout: u32,
}

impl HookAuth {
    pub fn new() -> Self {
        let exe = std::env::current_exe()
            .expect("this executable's own path is unavailable")
            .to_string_lossy()
            .into_owned();

        Self {
            token: random_token(),
            port: AtomicU16::new(0),
            exe,
        }
    }

    /// Point callbacks at the port the server actually bound.
    ///
    /// NB: separate from `new` because a configured port of 0 asks for a free
    /// one, so the answer does not exist until the listener is up.
    pub fn bind(&self, port: u16) {
        self.port.store(port, Ordering::Relaxed);
    }

    pub fn matches(&self, token: &str) -> bool {
        // Not constant-time; an attacker would already need local access.
        self.token == token
    }

    pub fn url(&self, card_id: i64, event: &str) -> String {
        format!("{}/hooks/{}/{card_id}/{event}", self.origin(), self.token)
    }

    /// Where a session reports its inbox socket.
    ///
    /// NB: deliberately not under `/hooks`. Nothing Claude Code sends arrives
    /// here — it is posted by a re-execution of this binary — and a fourth
    /// segment there would collide with the `<event>` the real hooks use.
    pub fn inbox_url(&self, card_id: i64) -> String {
        format!("{}/inbox/{}/{card_id}", self.origin(), self.token)
    }

    fn origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.port.load(Ordering::Relaxed))
    }

    /// The `--settings` payload handed to `claude`.
    ///
    /// NB: this MERGES with the user's settings files rather than replacing
    /// them, so their own hooks keep running. It deliberately does not set
    /// `allowedHttpHookUrls` — defining that key at any level would switch the
    /// allowlist on globally and start blocking hooks that run fine today.
    pub fn settings(&self, card_id: i64) -> Value {
        let mut hooks: Map<String, Value> = EVENTS
            .iter()
            .map(|(event, path, matcher)| {
                let handler = HttpHook {
                    kind: "http",
                    url: self.url(card_id, path),
                    timeout: HOOK_TIMEOUT_SECS,
                };

                // NB: an absent `matcher` is what matches everything, so the key
                // is omitted rather than sent empty.
                let mut entry = Map::new();
                if let Some(matcher) = matcher {
                    entry.insert("matcher".to_owned(), json!(matcher));
                }
                entry.insert("hooks".to_owned(), json!([handler]));

                ((*event).to_owned(), json!([entry]))
            })
            .collect();

        hooks.insert("SessionStart".to_owned(), self.session_start_hook(card_id));

        // A message we send is not from one of the session's own children, so
        // without this a card running `bypassPermissions` would hold every
        // review behind an approval dialog instead of delivering it.
        json!({ "hooks": hooks, "crossSessionInbound": "accept" })
    }

    pub fn settings_json(&self, card_id: i64) -> String {
        self.settings(card_id).to_string()
    }

    /// Asks the session to report its inbox socket by re-executing this binary.
    ///
    /// `CLAUDE_CODE_MESSAGING_SOCKET` is exported to hooks as an environment
    /// variable, so only a `command` handler can see it — an HTTP POST carries
    /// nothing of the sort. Running ourselves rather than `curl` keeps the hook
    /// free of anything that has to be installed on the agent's path.
    ///
    /// That this is a command hook is also why it is worth anything as a signal:
    /// the event fires once the client is up and past the workspace-trust and
    /// `bypassPermissions` dialogs, which have no hook of their own, so its
    /// arrival is how `session::watch_startup` tells a session that is ready from
    /// one still blocked on somebody.
    fn session_start_hook(&self, card_id: i64) -> Value {
        let command = format!(
            "{} {HOOK_ARG} {SESSION_START} {}",
            shell_quote(&self.exe),
            shell_quote(&self.inbox_url(card_id))
        );

        json!([{
            "hooks": [{ "type": "command", "command": command, "timeout": HOOK_TIMEOUT_SECS }]
        }])
    }
}

/// Runs this binary's hook subcommand, if that is what the arguments ask for.
///
/// Returns `None` for an ordinary server start. `SessionStart` is the one event
/// Claude Code will not deliver over HTTP, so the settings point it at this
/// binary instead and the answer comes back in here — a second, short-lived
/// process that reports the session's inbox and exits.
pub fn dispatch(args: &[String]) -> Option<Result<()>> {
    match args {
        [arg, event, url] if arg == HOOK_ARG && event == SESSION_START => Some(report_inbox(url)),
        [arg, ..] if arg == HOOK_ARG => Some(Err(anyhow::anyhow!(
            "usage: {HOOK_ARG} {SESSION_START} <url>"
        ))),
        _ => None,
    }
}

/// Posts this session's inbox socket to the server.
fn report_inbox(url: &str) -> Result<()> {
    let socket = std::env::var("CLAUDE_CODE_MESSAGING_SOCKET").unwrap_or_default();
    if socket.is_empty() {
        bail!("CLAUDE_CODE_MESSAGING_SOCKET is unset; this agent predates cross-session messaging");
    }
    let token = std::env::var("CLAUDE_CODE_MESSAGING_TOKEN").unwrap_or_default();

    let body = json!({ "socket": socket, "token": token }).to_string();
    post(url, &body)
}

/// A one-shot HTTP POST, enough for a localhost callback to our own server.
///
/// NB: hand-rolled rather than pulled from a crate. This runs in a process
/// spawned once per session whose whole job is one request to a port we chose
/// ourselves, so a client that handles redirects, TLS and connection pools is
/// all cost.
fn post(url: &str, body: &str) -> Result<()> {
    let rest = url
        .strip_prefix("http://")
        .with_context(|| format!("{url} is not an http:// url"))?;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));

    let mut stream =
        TcpStream::connect(authority).with_context(|| format!("connecting to {authority}"))?;
    stream.set_read_timeout(Some(CLIENT_TIMEOUT))?;
    stream.set_write_timeout(Some(CLIENT_TIMEOUT))?;

    // NB: built as lines and joined rather than one continued literal. A
    // header that starts with whitespace is a folded line, not a header, and
    // the server answers 400 without saying why.
    let head = [
        format!("POST /{path} HTTP/1.1"),
        format!("Host: {authority}"),
        "Content-Type: application/json".to_owned(),
        format!("Content-Length: {}", body.len()),
        "Connection: close".to_owned(),
    ]
    .join("\r\n");
    let request = format!("{head}\r\n\r\n{body}");

    stream
        .write_all(request.as_bytes())
        .with_context(|| format!("posting to {url}"))?;

    let mut status = String::new();
    BufReader::new(&stream)
        .read_line(&mut status)
        .with_context(|| format!("reading the reply from {url}"))?;

    // NB: checked rather than assumed. A rejected token is the difference
    // between a card that can be sent a review and one that silently cannot,
    // and this is the only moment anything would notice.
    if !status.contains(" 200") {
        bail!("{url} answered {}", status.trim());
    }
    Ok(())
}

/// Wraps `s` in single quotes for `sh`, so a path with a space — or a quote — in
/// it cannot end the argument early.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// 128 bits from the OS CSPRNG, hex encoded.
fn random_token() -> String {
    let mut bytes = [0u8; 16];
    SysRng
        .try_fill_bytes(&mut bytes)
        .expect("the OS random number generator is unavailable");

    bytes
        .iter()
        .fold(String::with_capacity(32), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_hex_and_unpredictable() {
        let a = random_token();
        let b = random_token();

        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "two draws should not collide");
    }

    #[test]
    fn only_the_issuing_token_is_accepted() {
        let auth = HookAuth::new();
        let token = auth.url(1, "stop").split('/').nth(4).unwrap().to_owned();

        assert!(auth.matches(&token));
        assert!(!auth.matches("deadbeef"));
        assert!(!auth.matches(""));
    }

    fn settings_for(card_id: i64) -> (HookAuth, Value) {
        let auth = HookAuth::new();
        auth.bind(9999);
        let settings = auth.settings(card_id);
        (auth, settings)
    }

    #[test]
    fn settings_register_every_event_over_http() {
        let (auth, settings) = settings_for(42);
        let hooks = settings["hooks"].as_object().unwrap();

        // The HTTP events, plus the one command hook.
        assert_eq!(hooks.len(), EVENTS.len() + 1);

        for (event, path, matcher) in EVENTS {
            let entry = &hooks[*event][0];
            let handler = &entry["hooks"][0];
            assert_eq!(handler["type"], "http");
            assert_eq!(handler["timeout"], HOOK_TIMEOUT_SECS);
            assert_eq!(
                handler["url"].as_str().unwrap(),
                format!("http://127.0.0.1:9999/hooks/{}/42/{path}", auth.token)
            );

            match matcher {
                Some(matcher) => assert_eq!(entry["matcher"], *matcher),
                // An empty matcher is not the same as none of the key at all.
                None => assert!(entry.get("matcher").is_none(), "{event} grew a matcher"),
            }
        }
    }

    /// `Notification` covers more than a permission prompt, and one of the types
    /// it covers is useless here: `idle_prompt` fires a minute into every idle
    /// card. Admitting it would light up the whole board.
    #[test]
    fn the_notification_matcher_takes_dialogs_but_not_idleness() {
        let matcher = EVENTS
            .iter()
            .find(|(event, ..)| *event == "Notification")
            .and_then(|(.., matcher)| *matcher)
            .expect("Notification is what arms the dialog watcher");

        for kind in [
            "permission_prompt",
            "elicitation_dialog",
            "elicitation_url_dialog",
            "agent_needs_input",
        ] {
            assert!(
                matcher.contains(kind),
                "{kind} is a dialog holding the keyboard"
            );
        }
        assert!(!matcher.contains("idle_prompt"));
    }

    #[test]
    fn settings_do_not_touch_the_http_hook_allowlist() {
        let (_, settings) = settings_for(1);

        // Defining this key anywhere turns the allowlist on for every hook the
        // user has, including ones we know nothing about.
        assert!(settings.get("allowedHttpHookUrls").is_none());
        // Only the two keys we mean to set.
        let keys: Vec<&String> = settings.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["crossSessionInbound", "hooks"]);
    }

    /// A message from us is not an own-child message, and the inbound default
    /// holds one of those in a session that bypasses permission prompts. Without
    /// this key a `bypassPermissions` card would strand every review behind an
    /// approval dialog.
    #[test]
    fn settings_accept_inbound_messages() {
        let (_, settings) = settings_for(1);
        assert_eq!(settings["crossSessionInbound"], "accept");
    }

    /// Only a `command` handler sees `CLAUDE_CODE_MESSAGING_SOCKET`; it is an
    /// environment variable, not anything an HTTP payload carries. The command
    /// is this binary, so the hook needs nothing installed on the agent's path.
    #[test]
    fn session_start_re_executes_this_binary() {
        let (auth, settings) = settings_for(42);
        let handler = &settings["hooks"]["SessionStart"][0]["hooks"][0];

        assert_eq!(handler["type"], "command");
        let command = handler["command"].as_str().unwrap();
        assert!(command.starts_with(&shell_quote(&auth.exe)));
        assert!(command.ends_with(&format!("{SESSION_START} '{}'", auth.inbox_url(42))));
    }

    /// `dispatch` has to accept the arguments the settings emit, or the hook
    /// runs and the re-executed process refuses its own command line. Both sides
    /// are built from the same two constants, and this is what holds them there.
    #[test]
    fn dispatch_accepts_what_the_hook_command_passes() {
        let auth = HookAuth::new();
        auth.bind(9999);
        let url = auth.inbox_url(42);

        let command = auth.settings(42)["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(command.ends_with(&format!("{HOOK_ARG} {SESSION_START} {}", shell_quote(&url))));

        assert!(dispatch(&[HOOK_ARG.to_owned(), SESSION_START.to_owned(), url]).is_some());
    }

    #[test]
    fn an_ordinary_start_is_not_a_hook() {
        assert!(dispatch(&[]).is_none());
        assert!(dispatch(&["--port".to_owned(), "8770".to_owned()]).is_none());
    }

    /// An installation path with a space in it is not an excuse to run the wrong
    /// program.
    #[test]
    fn a_path_with_a_quote_in_it_cannot_end_the_argument() {
        assert_eq!(
            shell_quote("/srv/it's here/ledecky"),
            r"'/srv/it'\''s here/ledecky'"
        );
    }

    #[test]
    fn the_payload_is_valid_json_for_the_cli() {
        let json = HookAuth::new().settings_json(7);
        let parsed: Value = serde_json::from_str(&json).unwrap();
        assert!(parsed["hooks"]["Stop"][0]["hooks"][0]["url"]
            .as_str()
            .unwrap()
            .ends_with("/7/stop"));
    }
}
