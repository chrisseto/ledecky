use std::collections::HashSet;

use rusqlite::Connection;

/// Files the reviewer has ticked off on a card.
///
/// Reviewing a card spans many renders — a comment, a scope change, a new turn —
/// so which files have been read has to outlive the fragment it was ticked on.
pub struct Viewed;

impl Viewed {
    pub fn for_card(conn: &Connection, card_id: i64) -> HashSet<String> {
        conn.prepare("SELECT file_path FROM review_viewed WHERE card_id = ?1")
            .and_then(|mut stmt| {
                stmt.query_map([card_id], |row| row.get::<_, String>(0))
                    .map(|rows| rows.filter_map(Result::ok).collect())
            })
            .unwrap_or_default()
    }

    /// Ticks the file, or unticks it if it was already ticked.
    pub fn toggle(conn: &Connection, card_id: i64, file_path: &str) {
        let removed = conn
            .execute(
                "DELETE FROM review_viewed WHERE card_id = ?1 AND file_path = ?2",
                rusqlite::params![card_id, file_path],
            )
            .unwrap_or(0);

        if removed == 0 {
            let _ = conn.execute(
                "INSERT INTO review_viewed (card_id, file_path) VALUES (?1, ?2)",
                rusqlite::params![card_id, file_path],
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tests::memory_db;
    use crate::project::{Card, NewCard, Project};

    fn card(conn: &Connection) -> i64 {
        let project = Project::upsert(conn, std::path::Path::new("/srv/repo")).unwrap();
        Card::create(
            conn,
            NewCard {
                project_id: project,
                title: "review me",
                description: "",
                base_branch: "main",
                permission_mode: "acceptEdits",
                model: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn a_file_toggles_on_and_back_off() {
        let db = memory_db();
        let conn = db.lock();
        let card = card(&conn);

        Viewed::toggle(&conn, card, "src/main.rs");
        assert!(Viewed::for_card(&conn, card).contains("src/main.rs"));

        Viewed::toggle(&conn, card, "src/main.rs");
        assert!(Viewed::for_card(&conn, card).is_empty());
    }

    #[test]
    fn cards_do_not_share_what_has_been_read() {
        let db = memory_db();
        let conn = db.lock();
        let (one, two) = (card(&conn), card(&conn));

        Viewed::toggle(&conn, one, "src/main.rs");

        assert_eq!(Viewed::for_card(&conn, one).len(), 1);
        assert!(Viewed::for_card(&conn, two).is_empty());
    }
}
