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
use crate::review::{context_lines, Comment, DiffCache, Scope, Turn, CONTEXT_CHOICES};
use crate::tmpl::Tmpl;

#[derive(Serialize)]
#[serde(crate = "rocket::serde")]
struct Choice {
    key: String,
    label: String,
    selected: bool,
}

/// What the pane is currently showing. Carried on every form so a re-render
/// after a comment lands back on the same view.
#[derive(Debug, Clone, Copy)]
struct View<'a> {
    scope: Option<&'a str>,
    context: Option<&'a str>,
}

#[get("/cards/<id>/diff?<scope>&<context>")]
pub fn diff_pane(
    db: &State<Db>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    id: i64,
    scope: Option<&str>,
    context: Option<&str>,
) -> Result<Tmpl, Status> {
    let view = View { scope, context };
    Ok(Tmpl("_diff.html", pane(db, settings, cache, id, view)?))
}

/// Everything `_diff.html` needs, for the focus view's first render.
pub fn initial(
    db: &Db,
    settings: &Settings,
    cache: &DiffCache,
    id: i64,
    scope: Option<&str>,
) -> Result<minijinja::Value, Status> {
    pane(
        db,
        settings,
        cache,
        id,
        View {
            scope,
            context: None,
        },
    )
}

fn pane(
    db: &Db,
    settings: &Settings,
    cache: &DiffCache,
    id: i64,
    view: View<'_>,
) -> Result<minijinja::Value, Status> {
    let scope = Scope::parse(view.scope);
    let context_lines = context_lines(view.context);

    let conn = db.lock();
    let card = Card::find(&conn, id).ok_or(Status::NotFound)?;
    let project = Project::find(&conn, card.project_id).ok_or(Status::NotFound)?;
    let turns = Turn::for_card(&conn, id);
    let comments = Comment::for_card(&conn, id);
    drop(conn);

    let scopes: Vec<_> = Scope::menu(&turns)
        .into_iter()
        .map(|candidate| Choice {
            key: candidate.key(),
            label: candidate.label(),
            selected: candidate == scope,
        })
        .collect();

    let contexts: Vec<_> = CONTEXT_CHOICES
        .iter()
        .map(|(lines, label)| Choice {
            key: lines.to_string(),
            label: (*label).to_owned(),
            selected: *lines == context_lines,
        })
        .collect();

    let drafts = comments.iter().filter(|c| c.is_draft()).count();

    // Comments hang off `<file>#<side>:<line>` so a template lookup is one hit.
    let mut threads: HashMap<String, Vec<Comment>> = HashMap::new();
    for comment in comments {
        threads.entry(comment.anchor()).or_default().push(comment);
    }

    // The parse is context-independent and cached, so widening the window is a
    // re-slice rather than another run of git and delta.
    let files = match scope.revisions(settings, id, &turns) {
        Some((from, to)) => cache
            .get(&project.repo(), &from, &to)
            .map_err(|err| {
                error!("card {id}: diffing {from}..{to}: {err:#}");
                Status::InternalServerError
            })?
            .iter()
            .map(|file| file.hunks(context_lines))
            .collect(),
        None => Vec::new(),
    };

    // The agent's closing words for the most recent turn — how a failed merge or
    // an unanswered question surfaces outside the terminal.
    let last_message = turns
        .last()
        .and_then(|t| t.last_assistant_message.clone())
        .filter(|m| !m.trim().is_empty());

    Ok(context! {
        card, files, threads, scopes, contexts, drafts, last_message,
        scope => scope.key(),
        context => context_lines.to_string(),
    })
}

/// The view a form is submitted from, so the re-render matches what was on screen.
#[derive(rocket::FromForm)]
pub struct ViewForm {
    scope: String,
    context: Option<String>,
}

impl ViewForm {
    fn view(&self) -> View<'_> {
        View {
            scope: Some(&self.scope),
            context: self.context.as_deref(),
        }
    }
}

#[derive(rocket::FromForm)]
pub struct CommentForm {
    file_path: String,
    side: String,
    line: i64,
    body: String,
    #[field(default = String::new())]
    scope: String,
    context: Option<String>,
}

#[post("/cards/<id>/comments", data = "<form>")]
pub fn add_comment(
    db: &State<Db>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
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

    let view = View {
        scope: Some(&form.scope),
        context: form.context.as_deref(),
    };
    Ok(Tmpl("_diff.html", pane(db, settings, cache, id, view)?))
}

#[post("/cards/<id>/comments/<comment_id>/delete", data = "<form>")]
pub fn delete_comment(
    db: &State<Db>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    id: i64,
    comment_id: i64,
    form: Form<ViewForm>,
) -> Result<Tmpl, Status> {
    Comment::delete_draft(&db.lock(), id, comment_id);
    Ok(Tmpl("_diff.html", pane(db, settings, cache, id, form.view())?))
}

/// Hands every draft comment to the agent as one message and marks them sent.
#[post("/cards/<id>/review", data = "<form>")]
pub fn submit_review(
    db: &State<Db>,
    agents: &State<Agents>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    id: i64,
    form: Form<ViewForm>,
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

    Ok(Tmpl("_diff.html", pane(db, settings, cache, id, form.view())?))
}
