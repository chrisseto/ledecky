use std::fmt::Write as _;

use rocket::serde::Serialize;
use sqlx::sqlite::SqliteRow;
use sqlx::{FromRow, Row};

use crate::db::{sql, Db};

/// Which side of the diff a comment is pinned to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Old,
    New,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Old => "old",
            Self::New => "new",
        }
    }

    /// How the side reads in a message to the agent.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Old => "before",
            Self::New => "after",
        }
    }

    pub fn parse(raw: &str) -> Self {
        if raw == "old" {
            Self::Old
        } else {
            Self::New
        }
    }
}

/// A review note on one line of a diff.
#[derive(Debug, Clone, Serialize)]
#[serde(crate = "rocket::serde")]
pub struct Comment {
    pub id: i64,
    pub turn_id: Option<i64>,
    pub file_path: String,
    pub line: i64,
    pub side: String,
    pub body: String,
    /// `draft` until submitted, then `submitted`.
    pub state: String,
    pub created_at: String,
}

impl Comment {
    const COLUMNS: &'static str = "id, turn_id, file_path, line, side, body, state, created_at";

    pub const DRAFT: &'static str = "draft";
    #[cfg_attr(not(test), allow(dead_code))]
    pub const SUBMITTED: &'static str = "submitted";

    pub fn is_draft(&self) -> bool {
        self.state == Self::DRAFT
    }

    /// The key the diff template looks a thread up by.
    pub fn anchor(&self) -> String {
        format!("{}#{}:{}", self.file_path, self.side, self.line)
    }

    /// The `SELECT` every read here is a `WHERE` on.
    fn select(clause: &str) -> String {
        format!("SELECT {} FROM comments {clause}", Self::COLUMNS)
    }

    /// The comments that belong on one range of the card's diff.
    ///
    /// `turn` is the turn the range ends at. `history` says that range is a
    /// snapshot of a turn already past rather than the card as it stands — the
    /// difference between a record to read back and somewhere feedback has yet
    /// to go, which is what decides whether comments already sent come with it.
    ///
    /// NB: this is why a batch leaves the screen the moment it is sent. Every
    /// working range ends at the live head and so asks for drafts alone; the
    /// comments are reachable again only by naming their turn in the picker.
    ///
    /// NB: `IS` rather than `=`, to compare null-safely — a card with no turns
    /// yet has no id to match against. A comment written in that window names
    /// no turn either, and belongs to the working range until sending pins it.
    ///
    /// Ordered by id, which is the order they were written: a line carrying
    /// more than one reads as the thread it was.
    pub async fn find_in_range(
        db: &Db,
        card_id: i64,
        turn: Option<i64>,
        history: bool,
    ) -> sqlx::Result<Vec<Self>> {
        let clause = match history {
            true => "WHERE card_id = ?1 AND turn_id IS ?2 ORDER BY id",
            false => {
                "WHERE card_id = ?1 AND state = 'draft'
                   AND (turn_id IS ?2 OR turn_id IS NULL)
                 ORDER BY id"
            }
        };

        sqlx::query_as(sql(Self::select(clause)))
            .bind(card_id)
            .bind(turn)
            .fetch_all(db.pool())
            .await
    }

    /// Ordered by id so the batch reaches the agent in the order it was written,
    /// which is the order the reviewer was thinking in.
    pub async fn drafts(db: &Db, card_id: i64) -> sqlx::Result<Vec<Self>> {
        sqlx::query_as(sql(Self::select(
            "WHERE card_id = ?1 AND state = 'draft' ORDER BY id",
        )))
        .bind(card_id)
        .fetch_all(db.pool())
        .await
    }

    /// Every draft on the card, including any the range on screen does not
    /// render — the batch goes to the agent whole, so the count has to say so.
    pub async fn draft_count(db: &Db, card_id: i64) -> sqlx::Result<i64> {
        sqlx::query_scalar("SELECT COUNT(*) FROM comments WHERE card_id = ?1 AND state = 'draft'")
            .bind(card_id)
            .fetch_one(db.pool())
            .await
    }

    pub async fn create(
        db: &Db,
        card_id: i64,
        turn_id: Option<i64>,
        file_path: &str,
        line: i64,
        side: Side,
        body: &str,
    ) -> sqlx::Result<i64> {
        let id = sqlx::query(
            "INSERT INTO comments (card_id, turn_id, file_path, line, side, body)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(card_id)
        .bind(turn_id)
        .bind(file_path)
        .bind(line)
        .bind(side.as_str())
        .bind(body)
        .execute(db.pool())
        .await?
        .last_insert_rowid();

        Ok(id)
    }

    /// Drafts can be withdrawn; anything already sent to the agent cannot.
    pub async fn delete_draft(db: &Db, card_id: i64, id: i64) {
        let _ =
            sqlx::query("DELETE FROM comments WHERE id = ?1 AND card_id = ?2 AND state = 'draft'")
                .bind(id)
                .bind(card_id)
                .execute(db.pool())
                .await;
    }

    /// Withdraws the whole batch at once.
    pub async fn delete_drafts(db: &Db, card_id: i64) {
        let _ = sqlx::query("DELETE FROM comments WHERE card_id = ?1 AND state = 'draft'")
            .bind(card_id)
            .execute(db.pool())
            .await;
    }

    /// Sends the card's drafts, pinning them to the turn they were given at.
    ///
    /// Sending is what turns a note into a record, so it is also where one
    /// written before the card had any turn finally gets one — up to then it
    /// answers to the working range and needs no id of its own.
    pub async fn mark_submitted(db: &Db, card_id: i64, turn: Option<i64>) {
        let _ = sqlx::query(
            "UPDATE comments
                SET state = 'submitted', turn_id = COALESCE(turn_id, ?2)
              WHERE card_id = ?1 AND state = 'draft'",
        )
        .bind(card_id)
        .bind(turn)
        .execute(db.pool())
        .await;
    }

    /// Files under `turn` the comments that were sent before the card had one.
    ///
    /// Sending is what pins a comment, and a review given while the agent's
    /// first turn was still running has nothing to be pinned to: the worktree
    /// was reviewable long before any turn recorded it. The turn that lands
    /// next is the record of the work it was written against, and without this
    /// it would be a row no range ever renders again — not a draft, so no
    /// working range wants it, and named by no turn, so no snapshot has it.
    pub async fn adopt_orphans(db: &Db, card_id: i64, turn: i64) {
        let _ = sqlx::query(
            "UPDATE comments
                SET turn_id = ?2
              WHERE card_id = ?1 AND turn_id IS NULL AND state = 'submitted'",
        )
        .bind(card_id)
        .bind(turn)
        .execute(db.pool())
        .await;
    }
}

impl<'r> FromRow<'r, SqliteRow> for Comment {
    fn from_row(row: &'r SqliteRow) -> sqlx::Result<Self> {
        Ok(Self {
            id: row.try_get("id")?,
            turn_id: row.try_get("turn_id")?,
            file_path: row.try_get("file_path")?,
            line: row.try_get("line")?,
            side: row.try_get("side")?,
            body: row.try_get("body")?,
            state: row.try_get("state")?,
            created_at: row.try_get("created_at")?,
        })
    }
}

/// Renders drafts as one message for the agent.
pub fn format_review(drafts: &[Comment], scope: &str) -> String {
    let mut out = format!("Code review on {scope}:\n");

    for comment in drafts {
        let _ = write!(
            out,
            "\n{}:{} ({})\n{}\n",
            comment.file_path,
            comment.line,
            Side::parse(&comment.side).describe(),
            comment.body.trim()
        );
    }

    out.push_str("\nAddress each comment, then commit.");
    out
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
                task: "work",
                base_branch: "main",
                permission_mode: "acceptEdits",
                model: None,
            },
        )
        .await
        .unwrap()
    }

    /// Every row on the card, whatever range it answers to. Unordered: what
    /// these assertions count does not depend on it.
    async fn all(db: &Db, card_id: i64) -> Vec<Comment> {
        sqlx::query_as(sql(Comment::select("WHERE card_id = ?1")))
            .bind(card_id)
            .fetch_all(db.pool())
            .await
            .unwrap()
    }

    /// `turn_id` is a foreign key, so a comment can only name a turn that is
    /// really there.
    async fn turn(db: &Db, card_id: i64, n: i64) -> i64 {
        sqlx::query(
            "INSERT INTO turns (card_id, n, ref_name, commit_sha, parent_sha)
             VALUES (?1, ?2, ?3, ?4, '')",
        )
        .bind(card_id)
        .bind(n)
        .bind(format!("refs/x/turn-{n}"))
        .bind(format!("sha{n}"))
        .execute(db.pool())
        .await
        .unwrap()
        .last_insert_rowid()
    }

    fn sample(file_path: &str, line: i64, side: &str, body: &str) -> Comment {
        Comment {
            id: 1,
            turn_id: None,
            file_path: file_path.into(),
            line,
            side: side.into(),
            body: body.into(),
            state: Comment::DRAFT.into(),
            created_at: String::new(),
        }
    }

    #[test]
    fn sides_round_trip_and_default_to_new() {
        assert_eq!(Side::parse("old"), Side::Old);
        assert_eq!(Side::parse("new"), Side::New);
        // An unexpected value anchors to the post-change side, which is what a
        // reviewer almost always means.
        assert_eq!(Side::parse("sideways"), Side::New);
    }

    #[test]
    fn the_anchor_matches_what_the_template_looks_up() {
        assert_eq!(
            sample("src/a.rs", 12, "new", "x").anchor(),
            "src/a.rs#new:12"
        );
        assert_eq!(sample("src/a.rs", 3, "old", "x").anchor(), "src/a.rs#old:3");
    }

    #[test]
    fn a_review_reads_as_one_message() {
        let drafts = vec![
            sample("src/a.rs", 12, "new", "  Hoist this.  "),
            sample("src/b.rs", 4, "old", "Why was this removed?"),
        ];

        let review = format_review(&drafts, "Turn 2");

        assert!(review.starts_with("Code review on Turn 2:\n"));
        assert!(review.contains("src/a.rs:12 (after)\nHoist this.\n"));
        assert!(review.contains("src/b.rs:4 (before)\nWhy was this removed?\n"));
        assert!(review.ends_with("Address each comment, then commit."));
    }

    #[test]
    fn an_empty_review_still_reads_sensibly() {
        let review = format_review(&[], "All changes");
        assert_eq!(
            review,
            "Code review on All changes:\n\nAddress each comment, then commit."
        );
    }

    #[tokio::test]
    async fn a_comment_shows_only_on_the_range_that_ends_where_it_was_written() {
        let db = memory_db().await;
        let card_id = card(&db).await;
        let (first, second) = (turn(&db, card_id, 1).await, turn(&db, card_id, 2).await);

        Comment::create(&db, card_id, Some(first), "a.rs", 1, Side::New, "on 1")
            .await
            .unwrap();
        Comment::create(&db, card_id, Some(second), "a.rs", 2, Side::New, "on 2")
            .await
            .unwrap();
        Comment::mark_submitted(&db, card_id, Some(second)).await;
        Comment::create(&db, card_id, Some(second), "a.rs", 3, Side::New, "fresh")
            .await
            .unwrap();

        let bodies = async |turn, history| {
            Comment::find_in_range(&db, card_id, turn, history)
                .await
                .unwrap()
                .into_iter()
                .map(|c| c.body)
                .collect::<Vec<_>>()
        };

        // The card as it stands wants feedback still to give, not feedback given.
        assert_eq!(bodies(Some(second), false).await, ["fresh"]);
        // Reading turn 2 back is reading its record, which is both.
        assert_eq!(bodies(Some(second), true).await, ["on 2", "fresh"]);
        // And turn 1 keeps its own, wherever turn 2 has got to.
        assert_eq!(bodies(Some(first), true).await, ["on 1"]);
        assert!(bodies(Some(first), false).await.is_empty());
    }

    #[tokio::test]
    async fn a_comment_written_before_the_first_turn_waits_on_the_working_range() {
        let db = memory_db().await;
        let card_id = card(&db).await;

        // A dirty worktree is reviewable long before a turn records it, so there
        // is no turn for this to name yet.
        Comment::create(&db, card_id, None, "a.rs", 1, Side::New, "early")
            .await
            .unwrap();
        let bodies = async |turn, history| {
            Comment::find_in_range(&db, card_id, turn, history)
                .await
                .unwrap()
                .into_iter()
                .map(|c| c.body)
                .collect::<Vec<_>>()
        };
        assert_eq!(bodies(None, false).await, ["early"]);

        // A turn lands under it; it is still the card as it stands, so it stays.
        let first = turn(&db, card_id, 1).await;
        assert_eq!(bodies(Some(first), false).await, ["early"]);

        // Sending is what pins it, so it can be read back afterwards.
        Comment::mark_submitted(&db, card_id, Some(first)).await;
        assert!(bodies(Some(first), false).await.is_empty());
        assert_eq!(bodies(Some(first), true).await, ["early"]);
    }

    #[tokio::test]
    async fn a_review_sent_before_the_first_turn_is_filed_under_it() {
        let db = memory_db().await;
        let card_id = card(&db).await;

        // The agent is on its first turn and has already dirtied the worktree,
        // so there is something to review and nothing yet to pin it to.
        Comment::create(&db, card_id, None, "a.rs", 1, Side::New, "early")
            .await
            .unwrap();
        Comment::mark_submitted(&db, card_id, None).await;

        let first = turn(&db, card_id, 1).await;
        Comment::adopt_orphans(&db, card_id, first).await;

        let bodies = async |turn, history| {
            Comment::find_in_range(&db, card_id, turn, history)
                .await
                .unwrap()
                .into_iter()
                .map(|c| c.body)
                .collect::<Vec<_>>()
        };

        // Without the turn it names, this row would be on no range at all.
        assert_eq!(bodies(Some(first), true).await, ["early"]);
        assert!(bodies(Some(first), false).await.is_empty());
    }

    #[tokio::test]
    async fn drafts_are_separated_from_submitted_comments() {
        let db = memory_db().await;
        let card_id = card(&db).await;

        Comment::create(&db, card_id, None, "a.rs", 1, Side::New, "first")
            .await
            .unwrap();
        Comment::create(&db, card_id, None, "a.rs", 2, Side::New, "second")
            .await
            .unwrap();
        assert_eq!(Comment::drafts(&db, card_id).await.unwrap().len(), 2);

        Comment::mark_submitted(&db, card_id, None).await;
        assert!(Comment::drafts(&db, card_id).await.unwrap().is_empty());

        // Sending keeps the rows; which range still renders them is
        // `find_in_range`'s business.
        let sent = all(&db, card_id).await;
        assert_eq!(sent.len(), 2);
        assert!(sent.iter().all(|c| c.state == Comment::SUBMITTED));
    }

    #[tokio::test]
    async fn only_drafts_can_be_withdrawn() {
        let db = memory_db().await;
        let card_id = card(&db).await;

        let draft = Comment::create(&db, card_id, None, "a.rs", 1, Side::New, "oops")
            .await
            .unwrap();
        Comment::delete_draft(&db, card_id, draft).await;
        assert!(all(&db, card_id).await.is_empty());

        let sent = Comment::create(&db, card_id, None, "a.rs", 1, Side::New, "sent")
            .await
            .unwrap();
        Comment::mark_submitted(&db, card_id, None).await;
        Comment::delete_draft(&db, card_id, sent).await;

        // Already delivered to the agent, so removing it would rewrite history.
        assert_eq!(all(&db, card_id).await.len(), 1);
    }
}
