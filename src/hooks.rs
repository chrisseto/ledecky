use std::fmt::Write as _;
use std::sync::atomic::{AtomicU16, Ordering};

use rand::rngs::SysRng;
use rand::TryRng;
use rocket::serde::Serialize;
use serde_json::{json, Map, Value};

/// Hook events we register, paired with the URL segment each posts to.
///
/// NB: `SessionStart` is absent on purpose — it only accepts `command` and
/// `mcp_tool` handlers, so an HTTP hook there silently never fires. The opening
/// prompt goes out on a timer instead, and `session_id` is read from whichever
/// of these lands first.
const EVENTS: &[(&str, &str)] = &[
    ("UserPromptSubmit", "prompt"),
    ("PermissionRequest", "permission"),
    ("Stop", "stop"),
    ("SessionEnd", "end"),
];

const HOOK_TIMEOUT_SECS: u32 = 10;

/// Per-process secret scoping hook callbacks to this server instance, so another
/// local process cannot forge turn boundaries.
pub struct HookAuth {
    token: String,
    port: AtomicU16,
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
        Self {
            token: random_token(),
            port: AtomicU16::new(0),
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
        format!(
            "http://127.0.0.1:{}/hooks/{}/{card_id}/{event}",
            self.port.load(Ordering::Relaxed),
            self.token
        )
    }

    /// The `--settings` payload handed to `claude`.
    ///
    /// NB: this MERGES with the user's settings files rather than replacing
    /// them, so their own hooks keep running. It deliberately does not set
    /// `allowedHttpHookUrls` — defining that key at any level would switch the
    /// allowlist on globally and start blocking hooks that run fine today.
    pub fn settings(&self, card_id: i64) -> Value {
        let hooks: Map<String, Value> = EVENTS
            .iter()
            .map(|(event, path)| {
                let handler = HttpHook {
                    kind: "http",
                    url: self.url(card_id, path),
                    timeout: HOOK_TIMEOUT_SECS,
                };
                ((*event).to_owned(), json!([{ "hooks": [handler] }]))
            })
            .collect();

        json!({ "hooks": hooks })
    }

    pub fn settings_json(&self, card_id: i64) -> String {
        self.settings(card_id).to_string()
    }
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

    #[test]
    fn settings_register_every_event_over_http() {
        let auth = HookAuth::new();
        auth.bind(9999);
        let settings = auth.settings(42);
        let hooks = settings["hooks"].as_object().unwrap();

        assert_eq!(hooks.len(), EVENTS.len());
        // SessionStart cannot take an HTTP handler, so registering it would be a
        // hook that never fires.
        assert!(!hooks.contains_key("SessionStart"));

        for (event, path) in EVENTS {
            let handler = &hooks[*event][0]["hooks"][0];
            assert_eq!(handler["type"], "http");
            assert_eq!(handler["timeout"], HOOK_TIMEOUT_SECS);
            assert_eq!(
                handler["url"].as_str().unwrap(),
                format!("http://127.0.0.1:9999/hooks/{}/42/{path}", auth.token)
            );
        }
    }

    #[test]
    fn settings_do_not_touch_the_http_hook_allowlist() {
        let settings = HookAuth::new().settings(1);

        // Defining this key anywhere turns the allowlist on for every hook the
        // user has, including ones we know nothing about.
        assert!(settings.get("allowedHttpHookUrls").is_none());
        assert_eq!(settings.as_object().unwrap().len(), 1);
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
