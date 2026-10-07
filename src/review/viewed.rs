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

    /// Says whether a file has been read.
    ///
    /// NB: sets rather than flips, so the caller says which way it went. The
    /// browser folds the file itself and reports the state it landed on, and an
    /// update that re-asserts what is already stored has to be harmless —
    /// nothing here can tell a reader's click from a redraw replaying one.
    pub async fn set(db: &Db, card_id: i64, file_path: &str, viewed: bool) {
        let query = match viewed {
            true => sqlx::query(
                "INSERT INTO review_viewed (card_id, file_path) VALUES (?1, ?2)
                 ON CONFLICT (card_id, file_path) DO NOTHING",
            ),
            false => sqlx::query("DELETE FROM review_viewed WHERE card_id = ?1 AND file_path = ?2"),
        };

        let _ = query.bind(card_id).bind(file_path).execute(db.pool()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tests::memory_db;
    use crate::project::{Card, NewCard, Project};
    use crate::vcs::VCS;

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
                vcs: VCS::Git,
            },
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn a_file_ticks_on_and_back_off() {
        let db = memory_db().await;
        let card = card(&db).await;

        Viewed::set(&db, card, "src/main.rs", true).await;
        assert!(Viewed::for_card(&db, card).await.contains("src/main.rs"));

        Viewed::set(&db, card, "src/main.rs", false).await;
        assert!(Viewed::for_card(&db, card).await.is_empty());
    }

    /// A redraw can report a state the store already holds; saying it twice has
    /// to mean the same as saying it once.
    #[tokio::test]
    async fn setting_the_same_state_twice_changes_nothing() {
        let db = memory_db().await;
        let card = card(&db).await;

        Viewed::set(&db, card, "src/main.rs", true).await;
        Viewed::set(&db, card, "src/main.rs", true).await;
        assert_eq!(Viewed::for_card(&db, card).await.len(), 1);

        Viewed::set(&db, card, "src/main.rs", false).await;
        Viewed::set(&db, card, "src/main.rs", false).await;
        assert!(Viewed::for_card(&db, card).await.is_empty());
    }

    #[tokio::test]
    async fn cards_do_not_share_what_has_been_read() {
        let db = memory_db().await;
        let (one, two) = (card(&db).await, card(&db).await);

        Viewed::set(&db, one, "src/main.rs", true).await;

        assert_eq!(Viewed::for_card(&db, one).await.len(), 1);
        assert!(Viewed::for_card(&db, two).await.is_empty());
    }
}
