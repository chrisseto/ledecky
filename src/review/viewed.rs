use std::collections::HashSet;

use crate::db::Db;

/// Files the reviewer has ticked off on a card.
///
/// Reviewing a card spans many renders — a comment, a scope change, a new turn —
/// so which files have been read has to outlive the fragment it was ticked on.
pub struct Viewed;

impl Viewed {
    pub async fn for_card(db: &Db, card_id: i64) -> HashSet<String> {
        let rows: Vec<String> =
            sqlx::query_scalar("SELECT file_path FROM review_viewed WHERE card_id = ?1")
                .bind(card_id)
                .fetch_all(db.pool())
                .await
                .unwrap_or_default();

        rows.into_iter().collect()
    }

    /// Ticks the file, or unticks it if it was already ticked.
    pub async fn toggle(db: &Db, card_id: i64, file_path: &str) {
        // NB: in a transaction. Whether to insert is decided by what the delete
        // found, so two clicks landing together would otherwise both delete
        // nothing and both insert.
        let Ok(mut tx) = db.pool().begin().await else {
            return;
        };

        let removed =
            sqlx::query("DELETE FROM review_viewed WHERE card_id = ?1 AND file_path = ?2")
                .bind(card_id)
                .bind(file_path)
                .execute(&mut *tx)
                .await
                .map(|done| done.rows_affected())
                .unwrap_or(0);

        if removed == 0 {
            let _ = sqlx::query("INSERT INTO review_viewed (card_id, file_path) VALUES (?1, ?2)")
                .bind(card_id)
                .bind(file_path)
                .execute(&mut *tx)
                .await;
        }

        let _ = tx.commit().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tests::memory_db;
    use crate::project::{Card, NewCard, Project};

    async fn card(db: &Db) -> i64 {
        let project = Project::upsert(db, std::path::Path::new("/srv/repo"))
            .await
            .unwrap();
        Card::create(
            db,
            NewCard {
                project_id: project,
                task: "review me",
                base_branch: "main",
                permission_mode: "acceptEdits",
                model: None,
            },
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn a_file_toggles_on_and_back_off() {
        let db = memory_db().await;
        let card = card(&db).await;

        Viewed::toggle(&db, card, "src/main.rs").await;
        assert!(Viewed::for_card(&db, card).await.contains("src/main.rs"));

        Viewed::toggle(&db, card, "src/main.rs").await;
        assert!(Viewed::for_card(&db, card).await.is_empty());
    }

    #[tokio::test]
    async fn cards_do_not_share_what_has_been_read() {
        let db = memory_db().await;
        let (one, two) = (card(&db).await, card(&db).await);

        Viewed::toggle(&db, one, "src/main.rs").await;

        assert_eq!(Viewed::for_card(&db, one).await.len(), 1);
        assert!(Viewed::for_card(&db, two).await.is_empty());
    }
}
