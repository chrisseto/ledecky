use std::fmt::Write as _;

use rocket::serde::Serialize;
use rusqlite::{Connection, Row};

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

    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            turn_id: row.get("turn_id")?,
            file_path: row.get("file_path")?,
            line: row.get("line")?,
            side: row.get("side")?,
            body: row.get("body")?,
            state: row.get("state")?,
            created_at: row.get("created_at")?,
        })
    }

    pub fn is_draft(&self) -> bool {
        self.state == Self::DRAFT
    }

    /// The key the diff template looks a thread up by.
    pub fn anchor(&self) -> String {
        format!("{}#{}:{}", self.file_path, self.side, self.line)
    }

    fn query(
        conn: &Connection,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> rusqlite::Result<Vec<Self>> {
        conn.prepare(&format!("SELECT {} FROM comments {sql}", Self::COLUMNS))?
            .query_map(params, Self::from_row)?
            .collect()
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
    pub fn find_in_range(
        conn: &Connection,
        card_id: i64,
        turn: Option<i64>,
        history: bool,
    ) -> rusqlite::Result<Vec<Self>> {
        match history {
            true => Self::query(
                conn,
                "WHERE card_id = ?1 AND turn_id IS ?2 ORDER BY id",
                rusqlite::params![card_id, turn],
            ),
            false => Self::query(
                conn,
                "WHERE card_id = ?1 AND state = 'draft'
                   AND (turn_id IS ?2 OR turn_id IS NULL)
                 ORDER BY id",
                rusqlite::params![card_id, turn],
            ),
        }
    }

    /// Ordered by id so the batch reaches the agent in the order it was written,
    /// which is the order the reviewer was thinking in.
    pub fn drafts(conn: &Connection, card_id: i64) -> rusqlite::Result<Vec<Self>> {
        Self::query(
            conn,
            "WHERE card_id = ?1 AND state = 'draft' ORDER BY id",
            [card_id],
        )
    }

    /// Every draft on the card, including any the range on screen does not
    /// render — the batch goes to the agent whole, so the count has to say so.
    pub fn draft_count(conn: &Connection, card_id: i64) -> rusqlite::Result<i64> {
        conn.query_row(
            "SELECT COUNT(*) FROM comments WHERE card_id = ?1 AND state = 'draft'",
            [card_id],
            |r| r.get(0),
        )
    }

    pub fn create(
        conn: &Connection,
        card_id: i64,
        turn_id: Option<i64>,
        file_path: &str,
        line: i64,
        side: Side,
        body: &str,
    ) -> rusqlite::Result<i64> {
        conn.execute(
            "INSERT INTO comments (card_id, turn_id, file_path, line, side, body)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![card_id, turn_id, file_path, line, side.as_str(), body],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Drafts can be withdrawn; anything already sent to the agent cannot.
    pub fn delete_draft(conn: &Connection, card_id: i64, id: i64) {
        let _ = conn.execute(
            "DELETE FROM comments WHERE id = ?1 AND card_id = ?2 AND state = 'draft'",
            rusqlite::params![id, card_id],
        );
    }

    /// Withdraws the whole batch at once.
    pub fn delete_drafts(conn: &Connection, card_id: i64) {
        let _ = conn.execute(
            "DELETE FROM comments WHERE card_id = ?1 AND state = 'draft'",
            [card_id],
        );
    }

    /// Sends the card's drafts, pinning them to the turn they were given at.
    ///
    /// Sending is what turns a note into a record, so it is also where one
    /// written before the card had any turn finally gets one — up to then it
    /// answers to the working range and needs no id of its own.
    pub fn mark_submitted(conn: &Connection, card_id: i64, turn: Option<i64>) {
        let _ = conn.execute(
            "UPDATE comments
                SET state = 'submitted', turn_id = COALESCE(turn_id, ?2)
              WHERE card_id = ?1 AND state = 'draft'",
            rusqlite::params![card_id, turn],
        );
    }

    /// Files under `turn` the comments that were sent before the card had one.
    ///
    /// Sending is what pins a comment, and a review given while the agent's
    /// first turn was still running has nothing to be pinned to: the worktree
    /// was reviewable long before any turn recorded it. The turn that lands
    /// next is the record of the work it was written against, and without this
    /// it would be a row no range ever renders again — not a draft, so no
    /// working range wants it, and named by no turn, so no snapshot has it.
    pub fn adopt_orphans(conn: &Connection, card_id: i64, turn: i64) {
        let _ = conn.execute(
            "UPDATE comments
                SET turn_id = ?2
              WHERE card_id = ?1 AND turn_id IS NULL AND state = 'submitted'",
            rusqlite::params![card_id, turn],
        );
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

    fn card(conn: &Connection) -> i64 {
        let project = Project::upsert(conn, std::path::Path::new("/srv/repo")).unwrap();
        Card::create(
            conn,
            NewCard {
                project_id: project,
                task: "work",
                base_branch: "main",
                permission_mode: "acceptEdits",
                model: None,
            },
        )
        .unwrap()
    }

    /// Every row on the card, whatever range it answers to. Unordered: what
    /// these assertions count does not depend on it.
    fn all(conn: &Connection, card_id: i64) -> Vec<Comment> {
        Comment::query(conn, "WHERE card_id = ?1", [card_id]).unwrap()
    }

    /// `turn_id` is a foreign key, so a comment can only name a turn that is
    /// really there.
    fn turn(conn: &Connection, card_id: i64, n: i64) -> i64 {
        conn.execute(
            "INSERT INTO turns (card_id, n, ref_name, commit_sha, parent_sha)
             VALUES (?1, ?2, ?3, ?4, '')",
            rusqlite::params![card_id, n, format!("refs/x/turn-{n}"), format!("sha{n}")],
        )
        .unwrap();
        conn.last_insert_rowid()
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

    #[test]
    fn a_comment_shows_only_on_the_range_that_ends_where_it_was_written() {
        let db = memory_db();
        let conn = db.lock();
        let card_id = card(&conn);
        let (first, second) = (turn(&conn, card_id, 1), turn(&conn, card_id, 2));

        Comment::create(&conn, card_id, Some(first), "a.rs", 1, Side::New, "on 1").unwrap();
        Comment::create(&conn, card_id, Some(second), "a.rs", 2, Side::New, "on 2").unwrap();
        Comment::mark_submitted(&conn, card_id, Some(second));
        Comment::create(&conn, card_id, Some(second), "a.rs", 3, Side::New, "fresh").unwrap();

        let bodies = |turn, history| {
            Comment::find_in_range(&conn, card_id, turn, history)
                .unwrap()
                .into_iter()
                .map(|c| c.body)
                .collect::<Vec<_>>()
        };

        // The card as it stands wants feedback still to give, not feedback given.
        assert_eq!(bodies(Some(second), false), ["fresh"]);
        // Reading turn 2 back is reading its record, which is both.
        assert_eq!(bodies(Some(second), true), ["on 2", "fresh"]);
        // And turn 1 keeps its own, wherever turn 2 has got to.
        assert_eq!(bodies(Some(first), true), ["on 1"]);
        assert!(bodies(Some(first), false).is_empty());
    }

    #[test]
    fn a_comment_written_before_the_first_turn_waits_on_the_working_range() {
        let db = memory_db();
        let conn = db.lock();
        let card_id = card(&conn);

        // A dirty worktree is reviewable long before a turn records it, so there
        // is no turn for this to name yet.
        Comment::create(&conn, card_id, None, "a.rs", 1, Side::New, "early").unwrap();
        let bodies = |turn, history| {
            Comment::find_in_range(&conn, card_id, turn, history)
                .unwrap()
                .into_iter()
                .map(|c| c.body)
                .collect::<Vec<_>>()
        };
        assert_eq!(bodies(None, false), ["early"]);

        // A turn lands under it; it is still the card as it stands, so it stays.
        let first = turn(&conn, card_id, 1);
        assert_eq!(bodies(Some(first), false), ["early"]);

        // Sending is what pins it, so it can be read back afterwards.
        Comment::mark_submitted(&conn, card_id, Some(first));
        assert!(bodies(Some(first), false).is_empty());
        assert_eq!(bodies(Some(first), true), ["early"]);
    }

    #[test]
    fn a_review_sent_before_the_first_turn_is_filed_under_it() {
        let db = memory_db();
        let conn = db.lock();
        let card_id = card(&conn);

        // The agent is on its first turn and has already dirtied the worktree,
        // so there is something to review and nothing yet to pin it to.
        Comment::create(&conn, card_id, None, "a.rs", 1, Side::New, "early").unwrap();
        Comment::mark_submitted(&conn, card_id, None);

        let first = turn(&conn, card_id, 1);
        Comment::adopt_orphans(&conn, card_id, first);

        let bodies = |turn, history| {
            Comment::find_in_range(&conn, card_id, turn, history)
                .unwrap()
                .into_iter()
                .map(|c| c.body)
                .collect::<Vec<_>>()
        };

        // Without the turn it names, this row would be on no range at all.
        assert_eq!(bodies(Some(first), true), ["early"]);
        assert!(bodies(Some(first), false).is_empty());
    }

    #[test]
    fn drafts_are_separated_from_submitted_comments() {
        let db = memory_db();
        let conn = db.lock();
        let card_id = card(&conn);

        Comment::create(&conn, card_id, None, "a.rs", 1, Side::New, "first").unwrap();
        Comment::create(&conn, card_id, None, "a.rs", 2, Side::New, "second").unwrap();
        assert_eq!(Comment::drafts(&conn, card_id).unwrap().len(), 2);

        Comment::mark_submitted(&conn, card_id, None);
        assert!(Comment::drafts(&conn, card_id).unwrap().is_empty());

        // Sending keeps the rows; which range still renders them is
        // `find_in_range`'s business.
        let sent = all(&conn, card_id);
        assert_eq!(sent.len(), 2);
        assert!(sent.iter().all(|c| c.state == Comment::SUBMITTED));
    }

    #[test]
    fn only_drafts_can_be_withdrawn() {
        let db = memory_db();
        let conn = db.lock();
        let card_id = card(&conn);

        let draft = Comment::create(&conn, card_id, None, "a.rs", 1, Side::New, "oops").unwrap();
        Comment::delete_draft(&conn, card_id, draft);
        assert!(all(&conn, card_id).is_empty());

        let sent = Comment::create(&conn, card_id, None, "a.rs", 1, Side::New, "sent").unwrap();
        Comment::mark_submitted(&conn, card_id, None);
        Comment::delete_draft(&conn, card_id, sent);

        // Already delivered to the agent, so removing it would rewrite history.
        assert_eq!(all(&conn, card_id).len(), 1);
    }
}
