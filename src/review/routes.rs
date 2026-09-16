use std::collections::HashMap;

use minijinja::context;
use rocket::form::Form;
use rocket::http::Status;
use rocket::serde::Serialize;
use rocket::{get, post, State};

use crate::agent::Agents;
use crate::config::Settings;
use crate::db::Db;
use crate::project::{Card, Project};
use crate::review::comment::{format_review, Side};
use crate::review::{diff, Comment, Scope, Turn};
use crate::tmpl::Tmpl;

#[derive(Serialize)]
#[serde(crate = "rocket::serde")]
struct ScopeOption {
    key: String,
    label: String,
    selected: bool,
}

#[get("/cards/<id>/diff?<scope>")]
pub fn diff_pane(
    db: &State<Db>,
    settings: &State<Settings>,
    id: i64,
    scope: Option<&str>,
) -> Result<Tmpl, Status> {
    Ok(Tmpl("_diff.html", context(db, settings, id, scope)?))
}

/// Everything `_diff.html` needs. Shared with the card focus view, which renders
/// the same fragment inline on first load.
pub fn context(
    db: &Db,
    settings: &Settings,
    id: i64,
    scope: Option<&str>,
) -> Result<minijinja::Value, Status> {
    let scope = Scope::parse(scope);

    let conn = db.lock();
    let card = Card::find(&conn, id).ok_or(Status::NotFound)?;
    let project = Project::find(&conn, card.project_id).ok_or(Status::NotFound)?;
    let turns = Turn::for_card(&conn, id);
    let comments = Comment::for_card(&conn, id);
    drop(conn);

    let options: Vec<_> = Scope::menu(&turns)
        .into_iter()
        .map(|candidate| ScopeOption {
            key: candidate.key(),
            label: candidate.label(),
            selected: candidate == scope,
        })
        .collect();

    let drafts = comments.iter().filter(|c| c.is_draft()).count();

    // Comments hang off `<file>#<side>:<line>` so a template lookup is one hit.
    let mut threads: HashMap<String, Vec<Comment>> = HashMap::new();
    for comment in comments {
        threads.entry(comment.anchor()).or_default().push(comment);
    }

    let files = match scope.revisions(settings, id, &turns) {
        Some((from, to)) => diff::between(&project.repo(), &from, &to).map_err(|err| {
            error!("card {id}: diffing {from}..{to}: {err:#}");
            Status::InternalServerError
        })?,
        None => Vec::new(),
    };

    // The agent's closing words for the most recent turn — how a failed merge or
    // an unanswered question surfaces outside the terminal.
    let last_message = turns
        .last()
        .and_then(|t| t.last_assistant_message.clone())
        .filter(|m| !m.trim().is_empty());

    Ok(context! {
        card, files, threads, options, drafts, last_message,
        scope => scope.key(),
    })
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
pub fn add_comment(
    db: &State<Db>,
    settings: &State<Settings>,
    id: i64,
    form: Form<CommentForm>,
) -> Result<Tmpl, Status> {
    let body = form.body.trim();
    if !body.is_empty() {
        let conn = db.lock();
        let turn_id = Turn::latest_id(&conn, id);
        Comment::create(
            &conn,
            id,
            turn_id,
            &form.file_path,
            form.line,
            Side::parse(&form.side),
            body,
        )
        .map_err(|_| Status::InternalServerError)?;
    }

    Ok(Tmpl("_diff.html", context(db, settings, id, Some(&form.scope))?))
}

#[derive(rocket::FromForm)]
pub struct ScopeForm {
    scope: String,
}

#[post("/cards/<id>/comments/<comment_id>/delete", data = "<form>")]
pub fn delete_comment(
    db: &State<Db>,
    settings: &State<Settings>,
    id: i64,
    comment_id: i64,
    form: Form<ScopeForm>,
) -> Result<Tmpl, Status> {
    Comment::delete_draft(&db.lock(), id, comment_id);
    Ok(Tmpl("_diff.html", context(db, settings, id, Some(&form.scope))?))
}

/// Hands every draft comment to the agent as one message and marks them sent.
#[post("/cards/<id>/review", data = "<form>")]
pub fn submit_review(
    db: &State<Db>,
    agents: &State<Agents>,
    settings: &State<Settings>,
    id: i64,
    form: Form<ScopeForm>,
) -> Result<Tmpl, Status> {
    let drafts = Comment::drafts(&db.lock(), id);

    if !drafts.is_empty() {
        let agent = agents
            .get(id)
            .filter(|a| a.is_running())
            .ok_or(Status::Conflict)?;

        let scope = Scope::parse(Some(&form.scope));
        // A busy terminal means a modal is up; leave the drafts alone to retry.
        if !agent.inject(&format_review(&drafts, &scope.label())) {
            return Err(Status::Conflict);
        }

        Comment::mark_submitted(&db.lock(), id);
    }

    Ok(Tmpl("_diff.html", context(db, settings, id, Some(&form.scope))?))
}
