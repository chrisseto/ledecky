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

    pub fn for_card(conn: &Connection, card_id: i64) -> Vec<Self> {
        conn.prepare(&format!(
            "SELECT {} FROM comments WHERE card_id = ?1 ORDER BY id",
            Self::COLUMNS
        ))
        .and_then(|mut stmt| {
            stmt.query_map([card_id], Self::from_row)
                .map(|rows| rows.filter_map(Result::ok).collect())
        })
        .unwrap_or_default()
    }

    pub fn drafts(conn: &Connection, card_id: i64) -> Vec<Self> {
        Self::for_card(conn, card_id)
            .into_iter()
            .filter(Self::is_draft)
            .collect()
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

    pub fn mark_submitted(conn: &Connection, card_id: i64) {
        let _ = conn.execute(
            "UPDATE comments SET state = 'submitted' WHERE card_id = ?1 AND state = 'draft'",
            [card_id],
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
                title: "work",
                description: "",
                base_branch: "main",
                permission_mode: "acceptEdits",
                model: None,
            },
        )
        .unwrap()
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
    fn drafts_are_separated_from_submitted_comments() {
        let db = memory_db();
        let conn = db.lock();
        let card_id = card(&conn);

        Comment::create(&conn, card_id, None, "a.rs", 1, Side::New, "first").unwrap();
        Comment::create(&conn, card_id, None, "a.rs", 2, Side::New, "second").unwrap();
        assert_eq!(Comment::drafts(&conn, card_id).len(), 2);

        Comment::mark_submitted(&conn, card_id);
        assert!(Comment::drafts(&conn, card_id).is_empty());

        // Submitted comments stay visible on the diff.
        let all = Comment::for_card(&conn, card_id);
        assert_eq!(all.len(), 2);
        assert!(all.iter().all(|c| c.state == Comment::SUBMITTED));
    }

    #[test]
    fn only_drafts_can_be_withdrawn() {
        let db = memory_db();
        let conn = db.lock();
        let card_id = card(&conn);

        let draft = Comment::create(&conn, card_id, None, "a.rs", 1, Side::New, "oops").unwrap();
        Comment::delete_draft(&conn, card_id, draft);
        assert!(Comment::for_card(&conn, card_id).is_empty());

        let sent = Comment::create(&conn, card_id, None, "a.rs", 1, Side::New, "sent").unwrap();
        Comment::mark_submitted(&conn, card_id);
        Comment::delete_draft(&conn, card_id, sent);

        // Already delivered to the agent, so removing it would rewrite history.
        assert_eq!(Comment::for_card(&conn, card_id).len(), 1);
    }
}
