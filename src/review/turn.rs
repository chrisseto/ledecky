use std::path::Path;

use rocket::serde::Serialize;
use rusqlite::{Connection, Row};

use crate::config::Settings;
use crate::git;

/// A snapshot of the worktree at the end of one agent turn.
///
/// The agent may or may not commit its own work, so the server commits the
/// working tree itself; these refs are what make the diff scopes possible.
#[derive(Debug, Clone, Serialize)]
#[serde(crate = "rocket::serde")]
pub struct Turn {
    pub id: i64,
    pub n: i64,
    pub commit_sha: String,
    pub parent_sha: String,
    pub last_assistant_message: Option<String>,
    pub created_at: String,
}

impl Turn {
    const COLUMNS: &'static str =
        "id, n, commit_sha, parent_sha, last_assistant_message, created_at";

    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            n: row.get("n")?,
            commit_sha: row.get("commit_sha")?,
            parent_sha: row.get("parent_sha")?,
            last_assistant_message: row.get("last_assistant_message")?,
            created_at: row.get("created_at")?,
        })
    }

    pub fn for_card(conn: &Connection, card_id: i64) -> Vec<Self> {
        conn.prepare(&format!(
            "SELECT {} FROM turns WHERE card_id = ?1 ORDER BY n",
            Self::COLUMNS
        ))
        .and_then(|mut stmt| {
            stmt.query_map([card_id], Self::from_row)
                .map(|rows| rows.filter_map(Result::ok).collect())
        })
        .unwrap_or_default()
    }

    pub fn latest(conn: &Connection, card_id: i64) -> Option<Self> {
        conn.query_row(
            &format!(
                "SELECT {} FROM turns WHERE card_id = ?1 ORDER BY n DESC LIMIT 1",
                Self::COLUMNS
            ),
            [card_id],
            Self::from_row,
        )
        .ok()
    }

    pub fn latest_id(conn: &Connection, card_id: i64) -> Option<i64> {
        conn.query_row(
            "SELECT id FROM turns WHERE card_id = ?1 ORDER BY n DESC LIMIT 1",
            [card_id],
            |r| r.get(0),
        )
        .ok()
    }

    fn next_number(conn: &Connection, card_id: i64) -> i64 {
        conn.query_row(
            "SELECT COALESCE(MAX(n), 0) + 1 FROM turns WHERE card_id = ?1",
            [card_id],
            |r| r.get(0),
        )
        .unwrap_or(1)
    }

    fn record(
        conn: &Connection,
        settings: &Settings,
        card_id: i64,
        n: i64,
        commit_sha: &str,
        parent_sha: &str,
        message: &str,
    ) {
        let _ = conn.execute(
            "INSERT INTO turns
                 (card_id, n, ref_name, commit_sha, parent_sha, last_assistant_message)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                card_id,
                n,
                settings.turn_ref(card_id, n),
                commit_sha,
                parent_sha,
                message
            ],
        );
    }

    /// Snapshots `worktree` as the card's next turn.
    ///
    /// Returns `None` when nothing changed on disk, which is how a chat-only
    /// turn avoids piling up an empty ref.
    pub fn snapshot(
        conn: &Connection,
        settings: &Settings,
        card_id: i64,
        repo: &Path,
        worktree: &Path,
        message: &str,
    ) -> anyhow::Result<Option<Self>> {
        let n = Self::next_number(conn, card_id);
        let parent = Self::latest(conn, card_id)
            .map(|turn| turn.commit_sha)
            .unwrap_or_else(|| settings.base_ref(card_id));
        let parent_sha = git::run(repo, &["rev-parse", &parent])?;

        let Some(sha) = git::snapshot_turn(settings, repo, worktree, card_id, n, &parent_sha)?
        else {
            return Ok(None);
        };

        Self::record(conn, settings, card_id, n, &sha, &parent_sha, message);
        Ok(Self::latest(conn, card_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tests::memory_db;
    use crate::project::{Card, NewCard, Project};

    fn card(conn: &Connection) -> i64 {
        let project = Project::upsert(conn, Path::new("/srv/repo")).unwrap();
        Card::create(
            conn,
            NewCard {
                project_id: project,
                title: "work",
                description: "",
                base_branch: "main",
                permission_mode: "acceptEdits",
                model: None,
            },
        )
        .unwrap()
    }

    fn settings() -> Settings {
        use rocket::figment::providers::Serialized;
        Settings::from(
            &rocket::figment::Figment::new()
                .merge(Serialized::default("app_slug", "ledecky"))
                .merge(Serialized::default("data_dir", "/tmp/ledecky-turn-test")),
        )
        .unwrap()
    }

    #[test]
    fn turns_number_from_one_and_climb() {
        let db = memory_db();
        let conn = db.lock();
        let card_id = card(&conn);
        let settings = settings();

        assert_eq!(Turn::next_number(&conn, card_id), 1);
        Turn::record(&conn, &settings, card_id, 1, "sha1", "base", "first");
        assert_eq!(Turn::next_number(&conn, card_id), 2);
        Turn::record(&conn, &settings, card_id, 2, "sha2", "sha1", "second");
        assert_eq!(Turn::next_number(&conn, card_id), 3);
    }

    #[test]
    fn the_ref_name_follows_the_configured_slug() {
        let db = memory_db();
        let conn = db.lock();
        let card_id = card(&conn);

        use rocket::figment::providers::Serialized;
        let settings = Settings::from(
            &rocket::figment::Figment::new()
                .merge(Serialized::default("app_slug", "planner"))
                .merge(Serialized::default("data_dir", "/tmp/x")),
        )
        .unwrap();

        Turn::record(&conn, &settings, card_id, 1, "sha1", "base", "");

        let ref_name: String = conn
            .query_row("SELECT ref_name FROM turns WHERE card_id = ?1", [card_id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(ref_name, format!("refs/planner/{card_id}/turn-1"));
    }

    #[test]
    fn latest_is_the_highest_numbered_turn() {
        let db = memory_db();
        let conn = db.lock();
        let card_id = card(&conn);
        let settings = settings();

        assert!(Turn::latest(&conn, card_id).is_none());

        Turn::record(&conn, &settings, card_id, 1, "sha1", "base", "first");
        Turn::record(&conn, &settings, card_id, 2, "sha2", "sha1", "second");

        let latest = Turn::latest(&conn, card_id).unwrap();
        assert_eq!(latest.n, 2);
        assert_eq!(latest.commit_sha, "sha2");
        assert_eq!(latest.last_assistant_message.as_deref(), Some("second"));

        assert_eq!(Turn::for_card(&conn, card_id).len(), 2);
        assert_eq!(Turn::latest_id(&conn, card_id), Some(latest.id));
    }

    #[test]
    fn turns_are_scoped_to_their_card() {
        let db = memory_db();
        let conn = db.lock();
        let settings = settings();

        let a = card(&conn);
        let b = Card::create(
            &conn,
            NewCard {
                project_id: Project::upsert(&conn, Path::new("/srv/repo")).unwrap(),
                title: "other",
                description: "",
                base_branch: "main",
                permission_mode: "acceptEdits",
                model: None,
            },
        )
        .unwrap();

        Turn::record(&conn, &settings, a, 1, "sha-a", "base", "");

        assert_eq!(Turn::for_card(&conn, a).len(), 1);
        assert!(Turn::for_card(&conn, b).is_empty());
        assert_eq!(Turn::next_number(&conn, b), 1);
    }
}
