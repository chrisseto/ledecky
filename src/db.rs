use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::config::Settings;

const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/001_init.sql"),
    include_str!("../migrations/002_merge.sql"),
    include_str!("../migrations/003_agent_pid.sql"),
    include_str!("../migrations/004_review_viewed.sql"),
    include_str!("../migrations/005_session_titles.sql"),
    include_str!("../migrations/006_awaiting_user.sql"),
];

/// A single connection behind a mutex.
///
/// This is a local, single-user app and every query is a sub-millisecond hit on
/// a WAL-mode file, so a pool would buy nothing. Never hold the guard across an
/// `.await`.
#[derive(Clone)]
pub struct Db(Arc<Mutex<Connection>>);

impl Db {
    pub fn open(settings: &Settings) -> Result<Self> {
        let path = settings.db_path();
        let dir = path.parent().expect("the database path has a parent");
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

        let conn =
            Connection::open(&path).with_context(|| format!("opening {}", path.display()))?;
        Self::prepare(conn)
    }

    fn prepare(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;

        migrate(&conn)?;
        Ok(Self(Arc::new(Mutex::new(conn))))
    }

    pub fn lock(&self) -> MutexGuard<'_, Connection> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn migrate(conn: &Connection) -> Result<()> {
    let applied: i64 = conn.query_row("SELECT * FROM pragma_user_version", [], |r| r.get(0))?;

    for (i, sql) in MIGRATIONS.iter().enumerate().skip(applied as usize) {
        let version = i + 1;
        conn.execute_batch(&format!(
            "BEGIN; {sql}; PRAGMA user_version = {version}; COMMIT;"
        ))
        .with_context(|| format!("applying migration {version}"))?;
    }
    Ok(())
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A migrated, throwaway database for tests.
    pub fn memory_db() -> Db {
        Db::prepare(Connection::open_in_memory().unwrap()).unwrap()
    }

    #[test]
    fn migrations_apply_once_and_are_idempotent() {
        let db = memory_db();
        let conn = db.lock();

        let version: i64 = conn
            .query_row("SELECT * FROM pragma_user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version as usize, MIGRATIONS.len());

        // Re-running skips everything already applied.
        migrate(&conn).unwrap();
        let again: i64 = conn
            .query_row("SELECT * FROM pragma_user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(again, version);
    }

    #[test]
    fn every_table_the_app_uses_exists() {
        let db = memory_db();
        let conn = db.lock();

        for table in [
            "projects",
            "cards",
            "turns",
            "comments",
            "events",
            "review_viewed",
        ] {
            let count: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "{table} is missing");
        }
    }

    #[test]
    fn deleting_a_project_takes_its_cards_with_it() {
        let db = memory_db();
        let conn = db.lock();

        let project =
            crate::project::Project::upsert(&conn, std::path::Path::new("/srv/repo")).unwrap();
        crate::project::Card::create(
            &conn,
            crate::project::NewCard {
                project_id: project,
                task: "work",
                base_branch: "main",
                permission_mode: "acceptEdits",
                model: None,
            },
        )
        .unwrap();

        conn.execute("DELETE FROM projects WHERE id = ?1", [project])
            .unwrap();

        assert!(crate::project::Card::for_project(&conn, project).is_empty());
    }
}
