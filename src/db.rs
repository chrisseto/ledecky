use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::config;

const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/001_init.sql"),
    include_str!("../migrations/002_merge.sql"),
];

/// Single connection behind a mutex. This is a local, single-user app and every
/// query is a sub-millisecond hit against a WAL-mode file on disk; a pool would
/// buy nothing. Never hold the guard across an `.await`.
#[derive(Clone)]
pub struct Db(Arc<Mutex<Connection>>);

impl Db {
    pub fn open() -> Result<Self> {
        let path = config::db_path();
        std::fs::create_dir_all(path.parent().unwrap())
            .with_context(|| format!("creating {}", path.parent().unwrap().display()))?;

        let conn = Connection::open(&path).with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;

        migrate(&conn)?;
        Ok(Self(Arc::new(Mutex::new(conn))))
    }

    pub fn lock(&self) -> MutexGuard<'_, Connection> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn migrate(conn: &Connection) -> Result<()> {
    let applied: i64 =
        conn.query_row("SELECT * FROM pragma_user_version", [], |r| r.get(0))?;

    for (i, sql) in MIGRATIONS.iter().enumerate().skip(applied as usize) {
        conn.execute_batch(&format!("BEGIN; {sql}; PRAGMA user_version = {}; COMMIT;", i + 1))
            .with_context(|| format!("applying migration {}", i + 1))?;
    }
    Ok(())
}
