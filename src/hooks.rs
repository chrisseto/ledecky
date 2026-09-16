use std::fmt::Write as _;

/// Per-process secret that scopes hook callbacks to this server instance, so a
/// stray local process cannot forge turn boundaries.
pub struct HookAuth {
    token: String,
    port: u16,
}

impl HookAuth {
    pub fn new(port: u16) -> Self {
        Self {
            token: random_token(),
            port,
        }
    }

    pub fn matches(&self, token: &str) -> bool {
        // Not constant-time; the attacker would already need local access.
        self.token == token
    }

    fn url(&self, card_id: i64, event: &str) -> String {
        format!(
            "http://127.0.0.1:{}/hooks/{}/{card_id}/{event}",
            self.port, self.token
        )
    }

    /// The `--settings` payload handed to `claude`.
    ///
    /// NB: this MERGES with the user's settings files rather than replacing
    /// them, so their own hooks keep running. It deliberately does not set
    /// `allowedHttpHookUrls` — defining that key at any level would switch the
    /// allowlist on globally and start blocking hooks that run fine today.
    pub fn settings_json(&self, card_id: i64) -> String {
        // NB: `SessionStart` is absent on purpose — it only accepts `command` and
        // `mcp_tool` hooks, so an HTTP handler there silently never fires. The
        // opening prompt is sent on a timer instead, and `session_id` is picked
        // up from whichever of these lands first.
        const EVENTS: &[(&str, &str)] = &[
            ("UserPromptSubmit", "prompt"),
            ("PermissionRequest", "permission"),
            ("Stop", "stop"),
            ("SessionEnd", "end"),
        ];

        let mut hooks = String::from("{\"hooks\":{");
        for (i, (event, path)) in EVENTS.iter().enumerate() {
            if i > 0 {
                hooks.push(',');
            }
            let _ = write!(
                hooks,
                "\"{event}\":[{{\"hooks\":[{{\"type\":\"http\",\"url\":\"{}\",\"timeout\":10}}]}}]",
                self.url(card_id, path)
            );
        }
        hooks.push_str("}}");
        hooks
    }
}

fn random_token() -> String {
    let mut bytes = [0u8; 16];
    // /dev/urandom is always present on the platforms we run on.
    std::io::Read::read_exact(
        &mut std::fs::File::open("/dev/urandom").expect("opening /dev/urandom"),
        &mut bytes,
    )
    .expect("reading /dev/urandom");

    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}
