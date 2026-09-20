use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{AssertSqlSafe, SqlitePool};

use crate::config::Settings;

const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/001_init.sql"),
    include_str!("../migrations/002_merge.sql"),
    include_str!("../migrations/003_agent_pid.sql"),
    include_str!("../migrations/004_review_viewed.sql"),
    include_str!("../migrations/005_session_titles.sql"),
    include_str!("../migrations/006_awaiting_user.sql"),
];

/// One connection, behind a pool.
///
/// This is a local, single-user app and every query is a sub-millisecond hit on
/// a WAL-mode file, so a pool of one is all it needs. Sized deliberately rather
/// than by default: it is the cheapest way to keep two writers off each other,
/// and WAL's concurrent readers buy nothing when there is one reader.
///
/// NB: a sequence that reads and then writes what it read needs a transaction
/// of its own — one connection is not one statement, and every `.await` hands
/// it back.
#[derive(Clone)]
pub struct Db(SqlitePool);

impl Db {
    pub async fn open(settings: &Settings) -> Result<Self> {
        let path = settings.db_path();
        let dir = path.parent().expect("the database path has a parent");
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);

        Self::connect(options)
            .await
            .with_context(|| format!("opening {}", path.display()))
    }

    async fn connect(options: SqliteConnectOptions) -> Result<Self> {
        let options = options
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));

        // NB: the connection is kept rather than recycled. An idle timeout would
        // be harmless for a file, but the tests open `sqlite::memory:`, where
        // dropping the last connection drops the database with it.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(options)
            .await?;

        migrate(&pool).await?;
        Ok(Self(pool))
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.0
    }
}

/// Marks a built-up query string as safe to run.
///
/// NB: sqlx refuses a non-literal query unless the caller says it has looked.
/// Every one here is a literal with a `COLUMNS` const interpolated into it —
/// a list of column names written in this crate, never anything a request
/// carries. Bound values all go through `bind`. This is that audit, said once
/// rather than at each call site.
pub fn sql(query: String) -> AssertSqlSafe<String> {
    AssertSqlSafe(query)
}

async fn migrate(pool: &SqlitePool) -> Result<()> {
    let applied: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await?;

    for (i, script) in MIGRATIONS.iter().enumerate().skip(applied as usize) {
        let version = i + 1;
        let mut tx = pool.begin().await?;

        // NB: `raw_sql`, because a migration is a script rather than a
        // statement. The prepared-statement path takes one at a time.
        sqlx::raw_sql(AssertSqlSafe(*script))
            .execute(&mut *tx)
            .await
            .with_context(|| format!("applying migration {version}"))?;
        sqlx::raw_sql(sql(format!("PRAGMA user_version = {version}")))
            .execute(&mut *tx)
            .await
            .with_context(|| format!("recording migration {version}"))?;

        tx.commit().await?;
    }
    Ok(())
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A migrated, throwaway database for tests.
    ///
    /// NB: `sqlite::memory:` gives each *connection* a database of its own, so
    /// this only holds together because the pool is capped at one — the same
    /// cap production runs with.
    pub async fn memory_db() -> Db {
        Db::connect(SqliteConnectOptions::new().in_memory(true))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn migrations_apply_once_and_are_idempotent() {
        let db = memory_db().await;

        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(version as usize, MIGRATIONS.len());

        // Re-running skips everything already applied.
        migrate(db.pool()).await.unwrap();
        let again: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(again, version);
    }

    #[tokio::test]
    async fn every_table_the_app_uses_exists() {
        let db = memory_db().await;

        for table in [
            "projects",
            "cards",
            "turns",
            "comments",
            "events",
            "review_viewed",
        ] {
            let count: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            )
            .bind(table)
            .fetch_one(db.pool())
            .await
            .unwrap();
            assert_eq!(count, 1, "{table} is missing");
        }
    }

    #[tokio::test]
    async fn deleting_a_project_takes_its_cards_with_it() {
        let db = memory_db().await;

        let project = crate::project::Project::upsert(&db, std::path::Path::new("/srv/repo"))
            .await
            .unwrap();
        crate::project::Card::create(
            &db,
            crate::project::NewCard {
                project_id: project,
                task: "work",
                base_branch: "main",
                permission_mode: "acceptEdits",
                model: None,
            },
        )
        .await
        .unwrap();

        sqlx::query("DELETE FROM projects WHERE id = ?1")
            .bind(project)
            .execute(db.pool())
            .await
            .unwrap();

        assert!(crate::project::Card::for_project(&db, project)
            .await
            .is_empty());
    }
}
