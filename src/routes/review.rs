use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use minijinja::context;
use rocket::form::Form;
use rocket::http::Status;
use rocket::{get, post, State};
use serde::Serialize;

use crate::agent::Agents;
use crate::db::Db;
use crate::models::Turn;
use crate::tmpl::Tmpl;
use crate::{config, diff, queries};

/// What slice of the card's history the diff pane is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Everything the agent has done: base -> latest turn.
    All,
    /// One turn in isolation: turn n-1 -> turn n.
    Turn(i64),
    /// Everything after a turn: turn n -> latest.
    Since(i64),
}

impl Scope {
    pub fn parse(raw: Option<&str>) -> Self {
        match raw {
            Some(s) => match s.split_once('-') {
                Some(("turn", n)) => n.parse().map(Scope::Turn).unwrap_or(Scope::All),
                Some(("since", n)) => n.parse().map(Scope::Since).unwrap_or(Scope::All),
                _ => Scope::All,
            },
            None => Scope::All,
        }
    }

    pub fn key(self) -> String {
        match self {
            Scope::All => "all".into(),
            Scope::Turn(n) => format!("turn-{n}"),
            Scope::Since(n) => format!("since-{n}"),
        }
    }

    /// Resolves to the pair of revisions to diff, given the card's turns.
    fn revisions(self, card_id: i64, turns: &[Turn]) -> Option<(String, String)> {
        let latest = turns.last()?;
        let base = config::base_ref(card_id);
        let rev = |n: i64| {
            turns
                .iter()
                .find(|t| t.n == n)
                .map(|t| t.commit_sha.clone())
        };

        match self {
            Scope::All => Some((base, latest.commit_sha.clone())),
            Scope::Turn(n) => {
                let to = rev(n)?;
                let from = rev(n - 1).unwrap_or(base);
                Some((from, to))
            }
            Scope::Since(n) => Some((rev(n)?, latest.commit_sha.clone())),
        }
    }

    fn label(self) -> String {
        match self {
            Scope::All => "All changes".into(),
            Scope::Turn(n) => format!("Turn {n}"),
            Scope::Since(n) => format!("Since turn {n}"),
        }
    }
}

#[derive(Serialize)]
struct ScopeOption {
    key: String,
    label: String,
    selected: bool,
}

#[get("/cards/<id>/diff?<scope>")]
pub fn diff_pane(db: &State<Db>, id: i64, scope: Option<&str>) -> Result<Tmpl, Status> {
    Ok(Tmpl("_diff.html", diff_context(db, id, scope)?))
}

/// Everything `_diff.html` needs. Shared with the card focus view, which renders
/// the same fragment inline on first load.
pub fn diff_context(db: &Db, id: i64, scope: Option<&str>) -> Result<minijinja::Value, Status> {
    let scope = Scope::parse(scope);

    let conn = db.lock();
    let card = queries::card(&conn, id).ok_or(Status::NotFound)?;
    let project = queries::project(&conn, card.project_id).ok_or(Status::NotFound)?;
    let turns = queries::turns(&conn, id);
    let comments = queries::comments(&conn, id);
    drop(conn);

    let mut options = vec![ScopeOption {
        key: Scope::All.key(),
        label: Scope::All.label(),
        selected: scope == Scope::All,
    }];
    for turn in &turns {
        for candidate in [Scope::Turn(turn.n), Scope::Since(turn.n)] {
            options.push(ScopeOption {
                key: candidate.key(),
                label: candidate.label(),
                selected: scope == candidate,
            });
        }
    }

    // Comments hang off `<file>#<side>:<line>` so a template lookup is one hit.
    let mut threads: HashMap<String, Vec<_>> = HashMap::new();
    for comment in comments {
        threads
            .entry(format!("{}#{}:{}", comment.file_path, comment.side, comment.line))
            .or_default()
            .push(comment);
    }

    let files = match scope.revisions(id, &turns) {
        Some((from, to)) => diff::between(&PathBuf::from(&project.path), &from, &to)
            .map_err(|err| {
                error!("card {id}: diffing {from}..{to}: {err:#}");
                Status::InternalServerError
            })?,
        None => Vec::new(),
    };

    let drafts = threads
        .values()
        .flatten()
        .filter(|c| c.state == "draft")
        .count();

    // The agent's closing words for the most recent turn — how a failed merge or
    // an unanswered question surfaces outside the terminal.
    let last_message = turns
        .last()
        .and_then(|t| t.last_assistant_message.clone())
        .filter(|m| !m.trim().is_empty());

    Ok(context! { card, files, threads, options, scope => scope.key(), drafts, last_message })
}

#[derive(rocket::FromForm)]
pub struct CommentForm {
    file_path: String,
    side: String,
    line: i64,
    body: String,
    scope: String,
}

#[post("/cards/<id>/comments", data = "<form>")]
pub fn add_comment(db: &State<Db>, id: i64, form: Form<CommentForm>) -> Result<Tmpl, Status> {
    let body = form.body.trim();
    if body.is_empty() {
        return Ok(Tmpl("_diff.html", diff_context(db, id, Some(&form.scope))?));
    }

    let conn = db.lock();
    let turn_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM turns WHERE card_id = ?1 ORDER BY n DESC LIMIT 1",
            [id],
            |r| r.get(0),
        )
        .ok();

    conn.execute(
        "INSERT INTO comments (card_id, turn_id, file_path, line, side, body)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![id, turn_id, form.file_path, form.line, form.side, body],
    )
    .map_err(|_| Status::InternalServerError)?;
    drop(conn);

    Ok(Tmpl("_diff.html", diff_context(db, id, Some(&form.scope))?))
}

#[derive(rocket::FromForm)]
pub struct ScopeForm {
    scope: String,
}

#[post("/cards/<id>/comments/<comment_id>/delete", data = "<form>")]
pub fn delete_comment(
    db: &State<Db>,
    id: i64,
    comment_id: i64,
    form: Form<ScopeForm>,
) -> Result<Tmpl, Status> {
    let conn = db.lock();
    let _ = conn.execute(
        "DELETE FROM comments WHERE id = ?1 AND card_id = ?2 AND state = 'draft'",
        rusqlite::params![comment_id, id],
    );
    drop(conn);

    Ok(Tmpl("_diff.html", diff_context(db, id, Some(&form.scope))?))
}

/// Hands every draft comment to the agent as one message and marks them sent.
#[post("/cards/<id>/review", data = "<form>")]
pub fn submit_review(
    db: &State<Db>,
    agents: &State<Agents>,
    id: i64,
    form: Form<ScopeForm>,
) -> Result<Tmpl, Status> {
    let conn = db.lock();
    let drafts: Vec<_> = queries::comments(&conn, id)
        .into_iter()
        .filter(|c| c.state == "draft")
        .collect();
    drop(conn);

    if drafts.is_empty() {
        return Ok(Tmpl("_diff.html", diff_context(db, id, Some(&form.scope))?));
    }

    let Some(agent) = agents.get(id).filter(|a| a.is_running()) else {
        return Err(Status::Conflict);
    };

    let scope = Scope::parse(Some(&form.scope));
    if !agent.inject(&format_review(&drafts, &scope.label())) {
        // The terminal is busy with a modal; leave the drafts alone to retry.
        return Err(Status::Conflict);
    }

    let conn = db.lock();
    let _ = conn.execute(
        "UPDATE comments SET state = 'submitted' WHERE card_id = ?1 AND state = 'draft'",
        [id],
    );
    drop(conn);

    Ok(Tmpl("_diff.html", diff_context(db, id, Some(&form.scope))?))
}

fn format_review(drafts: &[crate::models::Comment], scope: &str) -> String {
    let mut out = format!("Code review on {scope}:\n");
    for comment in drafts {
        let _ = write!(
            out,
            "\n{}:{} ({})\n{}\n",
            comment.file_path,
            comment.line,
            if comment.side == "old" { "before" } else { "after" },
            comment.body.trim()
        );
    }
    out.push_str("\nAddress each comment, then commit.");
    out
}

#[cfg(test)]
mod tests {
    use super::Scope;

    #[test]
    fn scope_round_trips_through_its_key() {
        for scope in [Scope::All, Scope::Turn(3), Scope::Since(2)] {
            assert_eq!(Scope::parse(Some(&scope.key())), scope);
        }
    }

    #[test]
    fn unknown_scopes_fall_back_to_all() {
        assert_eq!(Scope::parse(None), Scope::All);
        assert_eq!(Scope::parse(Some("nonsense")), Scope::All);
        assert_eq!(Scope::parse(Some("turn-x")), Scope::All);
    }
}
