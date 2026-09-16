use std::path::PathBuf;

/// Identifier for this app's data directory and git ref namespace.
/// Deliberately not a bare `kanban` — that collides with other tooling writing to `refs/`.
pub const APP_SLUG: &str = "kanban2";

/// Root for everything we persist: the database, worktrees, and terminal scrollback.
pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").expect("HOME is not set");
            PathBuf::from(home).join(".local/share")
        });
    base.join(APP_SLUG)
}

pub fn db_path() -> PathBuf {
    data_dir().join(format!("{APP_SLUG}.db"))
}

pub fn worktree_path(card_id: i64) -> PathBuf {
    data_dir().join("worktrees").join(card_id.to_string())
}

pub fn card_dir(card_id: i64) -> PathBuf {
    data_dir().join("cards").join(card_id.to_string())
}

/// `refs/{APP_SLUG}/<card>/` — where turn snapshots live in the *project* repo.
pub fn ref_namespace(card_id: i64) -> String {
    format!("refs/{APP_SLUG}/{card_id}")
}

pub fn base_ref(card_id: i64) -> String {
    format!("{}/base", ref_namespace(card_id))
}

pub fn turn_ref(card_id: i64, n: i64) -> String {
    format!("{}/turn-{n}", ref_namespace(card_id))
}
