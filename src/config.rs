use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rocket::figment::Figment;
use rocket::serde::Deserialize;

/// Application settings, read from the same figment Rocket uses.
///
/// Keys live under `[default]` in `Rocket.toml` alongside Rocket's own, and can
/// be overridden per-run with `LEDECKY_*` environment variables. Every field
/// carries its own default, so `Rocket.toml` documents rather than supplies
/// them and a figment with none of these keys still deserializes.
#[derive(Debug, Clone, Deserialize)]
#[serde(crate = "rocket::serde", default)]
pub struct Settings {
    /// Names this app's data directory and its git ref namespace.
    pub app_slug: String,

    /// Root for the database, worktrees and per-card scratch space.
    ///
    /// `None` until `from` resolves it, because its default is
    /// `$XDG_DATA_HOME/<app_slug>` and so cannot be written without knowing the
    /// slug. Read it through `data_dir()`.
    data_dir: Option<PathBuf>,

    /// Executable spawned for an agent. Overridable so the end-to-end suite can
    /// substitute a scripted stand-in.
    pub agent_bin: String,

    /// Extra environment for that executable.
    ///
    /// The agent would otherwise inherit only this server's environment, which
    /// leaves no way to configure it per deployment — or, for the end-to-end
    /// suite, to tell its stand-in how long to pretend to start up for.
    pub agent_env: HashMap<String, String>,

    /// How long to let a burst of worktree writes settle, in milliseconds,
    /// before announcing that the diff moved. Saving one file touches it
    /// several times and a build touches thousands.
    pub watch_debounce: u64,

    /// How long, in milliseconds, a staged worktree head stays good for without
    /// the watcher saying otherwise. A backstop, not the mechanism.
    pub head_ttl: u64,

    /// How long the TUI needs, in milliseconds, before it will accept pasted
    /// input.
    pub ready_delay: u64,

    /// How long to wait, in milliseconds, for any hook to come back before
    /// concluding our hook URLs are not reaching us — which is what the card's
    /// "misconfigured" state reports, most often an `allowedHttpHookUrls`
    /// allowlist that does not name us.
    pub hook_grace: u64,

    /// How long, in milliseconds, a resumed session has to prove itself one way
    /// or the other.
    pub resume_timeout: u64,

    /// How long, in milliseconds, a requested dialog has to paint before the
    /// watcher gives up on it.
    pub dialog_grace: u64,

    /// How often, in milliseconds, to re-check the input box while landing a
    /// paste.
    pub paste_poll: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            app_slug: "ledecky".to_owned(),
            data_dir: None,
            agent_bin: "claude".to_owned(),
            agent_env: HashMap::new(),
            watch_debounce: 250,
            head_ttl: 30_000,
            ready_delay: 2500,
            hook_grace: 15_000,
            resume_timeout: 5000,
            dialog_grace: 5000,
            paste_poll: 150,
        }
    }
}

/// The waits the agent plumbing makes in real time.
///
/// Bundled and `Copy` because `deliver_opening_prompt` runs on a detached
/// thread and `Settings` is neither. Every one of these is overridable for the
/// same reason every other wait here is: so the end-to-end suite does not have
/// to wait in real time.
#[derive(Debug, Clone, Copy)]
pub struct Timings {
    pub ready_delay: Duration,
    pub hook_grace: Duration,
    pub resume_timeout: Duration,
    pub dialog_grace: Duration,
    pub paste_poll: Duration,
}

impl Settings {
    /// Reads the settings, filling in the one default that depends on another.
    pub fn from(figment: &Figment) -> Result<Self, rocket::figment::Error> {
        let mut settings: Self = figment.extract()?;
        settings
            .data_dir
            .get_or_insert_with(|| default_data_dir(&settings.app_slug));
        Ok(settings)
    }

    /// Root for the database, worktrees and per-card scratch space.
    pub fn data_dir(&self) -> &Path {
        self.data_dir
            .as_deref()
            .expect("`Settings::from` resolves the data directory")
    }

    /// The real-time waits, as durations.
    pub fn timings(&self) -> Timings {
        Timings {
            ready_delay: Duration::from_millis(self.ready_delay),
            hook_grace: Duration::from_millis(self.hook_grace),
            resume_timeout: Duration::from_millis(self.resume_timeout),
            dialog_grace: Duration::from_millis(self.dialog_grace),
            paste_poll: Duration::from_millis(self.paste_poll),
        }
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir().join(format!("{}.db", self.app_slug))
    }

    pub fn worktree_path(&self, card_id: i64) -> PathBuf {
        self.data_dir().join("worktrees").join(card_id.to_string())
    }

    pub fn worktrees_dir(&self) -> PathBuf {
        self.data_dir().join("worktrees")
    }

    pub fn card_dir(&self, card_id: i64) -> PathBuf {
        self.data_dir().join("cards").join(card_id.to_string())
    }

    /// `refs/<app_slug>/<card>/base` — where a card's diffs start from.
    pub fn base_ref(&self, card_id: i64) -> String {
        format!("refs/{}/{card_id}/base", self.app_slug)
    }

    /// `refs/<app_slug>/<card>/turn-<n>` — one snapshot per finished turn.
    pub fn turn_ref(&self, card_id: i64, n: i64) -> String {
        format!("refs/{}/{card_id}/turn-{n}", self.app_slug)
    }

    /// `refs/<app_slug>/<card>/working` — the live worktree's tree.
    ///
    /// Unlike the turn refs this one is not history: it moves on every read and
    /// goes away with the worktree.
    pub fn working_ref(&self, card_id: i64) -> String {
        format!("refs/{}/{card_id}/working", self.app_slug)
    }
}

fn default_data_dir(app_slug: &str) -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").expect("HOME is not set");
            PathBuf::from(home).join(".local/share")
        });
    base.join(app_slug)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rocket::figment::providers::Serialized;

    fn settings(values: &[(&str, &str)]) -> Settings {
        let mut figment = Figment::new();
        for (key, value) in values {
            figment = figment.merge(Serialized::default(key, value.to_string()));
        }
        Settings::from(&figment).unwrap()
    }

    #[test]
    fn defaults_come_from_the_slug() {
        let s = settings(&[("data_dir", "/srv/board")]);
        assert_eq!(s.app_slug, "ledecky");
        assert_eq!(s.agent_bin, "claude");
        assert_eq!(s.watch_debounce, 250);
        assert!(s.agent_env.is_empty());
        assert_eq!(s.db_path(), PathBuf::from("/srv/board/ledecky.db"));

        // The defaults are the constants these replaced, so an unconfigured
        // server behaves exactly as it did before they were overridable.
        let t = s.timings();
        assert_eq!(t.ready_delay, Duration::from_millis(2500));
        assert_eq!(t.hook_grace, Duration::from_secs(15));
        assert_eq!(t.resume_timeout, Duration::from_secs(5));
        assert_eq!(t.dialog_grace, Duration::from_secs(5));
        assert_eq!(t.paste_poll, Duration::from_millis(150));
    }

    #[test]
    fn the_slug_names_paths_and_refs() {
        let s = settings(&[("app_slug", "planner"), ("data_dir", "/srv/board")]);

        assert_eq!(s.db_path(), PathBuf::from("/srv/board/planner.db"));
        assert_eq!(s.worktree_path(7), PathBuf::from("/srv/board/worktrees/7"));
        assert_eq!(s.card_dir(7), PathBuf::from("/srv/board/cards/7"));
        assert_eq!(s.base_ref(7), "refs/planner/7/base");
        assert_eq!(s.turn_ref(7, 3), "refs/planner/7/turn-3");
        assert_eq!(s.working_ref(7), "refs/planner/7/working");
    }

    /// One key carries the whole map, which is what lets the end-to-end suite
    /// configure its stand-in agent without a second channel.
    ///
    /// NB: asserted through a real map rather than the string figment's `Env`
    /// provider would parse — reproducing that would mean writing to the
    /// process environment, which these tests deliberately leave alone.
    #[test]
    fn the_agent_environment_comes_from_one_key() {
        let env = HashMap::from([("FAKE_AGENT_BOOT_MS".to_owned(), "250".to_owned())]);
        let figment = Figment::new().merge(Serialized::default("agent_env", env));
        let s = Settings::from(&figment).unwrap();

        assert_eq!(
            s.agent_env.get("FAKE_AGENT_BOOT_MS").map(String::as_str),
            Some("250")
        );
    }

    #[test]
    fn the_data_dir_follows_xdg_when_unset() {
        // Only the derivation is asserted; the process env is left alone.
        assert_eq!(default_data_dir("planner").file_name().unwrap(), "planner",);
    }
}
