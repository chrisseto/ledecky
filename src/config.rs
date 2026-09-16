use std::path::PathBuf;

use rocket::figment::Figment;
use rocket::serde::Deserialize;

/// Application settings, read from the same figment Rocket uses.
///
/// Keys live under `[default]` in `Rocket.toml` alongside Rocket's own, and can
/// be overridden per-run with `KANBAN2_*` environment variables.
#[derive(Debug, Clone, Deserialize)]
#[serde(crate = "rocket::serde")]
pub struct Settings {
    /// Names this app's data directory and its git ref namespace. Distinct from
    /// a bare `kanban` so the refs cannot collide with other tooling.
    pub app_slug: String,

    /// Root for the database, worktrees and per-card scratch space. Defaults to
    /// `$XDG_DATA_HOME/<app_slug>`.
    pub data_dir: PathBuf,

    /// Executable spawned for an agent. Overridable so the end-to-end suite can
    /// substitute a scripted stand-in.
    pub agent_bin: String,
}

impl Settings {
    /// Reads the settings, filling in the defaults that depend on `app_slug`.
    pub fn from(figment: &Figment) -> Result<Self, rocket::figment::Error> {
        let app_slug: String = figment.extract_inner("app_slug").unwrap_or_else(|_| "kanban2".into());

        let data_dir = figment
            .extract_inner::<PathBuf>("data_dir")
            .unwrap_or_else(|_| default_data_dir(&app_slug));

        let agent_bin = figment
            .extract_inner("agent_bin")
            .unwrap_or_else(|_| "claude".to_owned());

        Ok(Self {
            app_slug,
            data_dir,
            agent_bin,
        })
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join(format!("{}.db", self.app_slug))
    }

    pub fn worktree_path(&self, card_id: i64) -> PathBuf {
        self.data_dir.join("worktrees").join(card_id.to_string())
    }

    pub fn worktrees_dir(&self) -> PathBuf {
        self.data_dir.join("worktrees")
    }

    pub fn card_dir(&self, card_id: i64) -> PathBuf {
        self.data_dir.join("cards").join(card_id.to_string())
    }

    /// `refs/<app_slug>/<card>/base` — where a card's diffs start from.
    pub fn base_ref(&self, card_id: i64) -> String {
        format!("refs/{}/{card_id}/base", self.app_slug)
    }

    /// `refs/<app_slug>/<card>/turn-<n>` — one snapshot per finished turn.
    pub fn turn_ref(&self, card_id: i64, n: i64) -> String {
        format!("refs/{}/{card_id}/turn-{n}", self.app_slug)
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
        assert_eq!(s.app_slug, "kanban2");
        assert_eq!(s.agent_bin, "claude");
        assert_eq!(s.db_path(), PathBuf::from("/srv/board/kanban2.db"));
    }

    #[test]
    fn the_slug_names_paths_and_refs() {
        let s = settings(&[("app_slug", "planner"), ("data_dir", "/srv/board")]);

        assert_eq!(s.db_path(), PathBuf::from("/srv/board/planner.db"));
        assert_eq!(s.worktree_path(7), PathBuf::from("/srv/board/worktrees/7"));
        assert_eq!(s.card_dir(7), PathBuf::from("/srv/board/cards/7"));
        assert_eq!(s.base_ref(7), "refs/planner/7/base");
        assert_eq!(s.turn_ref(7, 3), "refs/planner/7/turn-3");
    }

    #[test]
    fn the_data_dir_follows_xdg_when_unset() {
        // Only the derivation is asserted; the process env is left alone.
        assert_eq!(
            default_data_dir("planner").file_name().unwrap(),
            "planner",
        );
    }
}
