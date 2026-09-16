use minijinja::context;
use rocket::form::Form;
use rocket::http::Status;
use rocket::response::Redirect;
use rocket::{get, post, State};

use crate::agent::{session, Agents};
use crate::config::Settings;
use crate::db::Db;
use crate::git;
use crate::hooks::HookAuth;
use crate::project::{AgentState, Card, Lane, NewCard, Project};
use crate::review::DiffCache;
use crate::tmpl::Tmpl;

pub const PERMISSION_MODES: &[(&str, &str)] = &[
    ("acceptEdits", "Accept edits — prompts for Bash and other tools"),
    ("bypassPermissions", "Bypass permissions — fully unattended"),
    ("default", "Manual — prompt for everything"),
    ("plan", "Plan — read-only until you approve"),
];

pub const MODELS: &[(&str, &str)] = &[
    ("", "Default"),
    ("opus", "Opus"),
    ("sonnet", "Sonnet"),
    ("haiku", "Haiku"),
];

#[get("/projects/<id>")]
pub fn board(db: &State<Db>, id: i64) -> Result<Tmpl, Status> {
    let conn = db.lock();
    let project = Project::find(&conn, id).ok_or(Status::NotFound)?;
    let cards = Card::for_project(&conn, id);
    drop(conn);

    let lanes: Vec<_> = Lane::ALL
        .iter()
        .map(|lane| {
            context! {
                key => lane.as_str(),
                label => lane.label(),
                cards => cards.iter().filter(|c| c.lane == *lane).collect::<Vec<_>>(),
            }
        })
        .collect();

    Ok(Tmpl("board.html", context! { project, lanes }))
}

#[get("/projects/<id>/cards/new")]
pub fn new_card(db: &State<Db>, id: i64) -> Result<Tmpl, Status> {
    let conn = db.lock();
    let project = Project::find(&conn, id).ok_or(Status::NotFound)?;
    drop(conn);

    let branches = git::branches(&project.repo());
    Ok(Tmpl(
        "card_new.html",
        context! {
            project,
            branches,
            permission_modes => PERMISSION_MODES,
            models => MODELS,
            error => Option::<String>::None,
        },
    ))
}

#[derive(rocket::FromForm)]
pub struct CardForm {
    title: String,
    description: String,
    base_branch: String,
    permission_mode: String,
    model: String,
}

#[post("/projects/<id>/cards", data = "<form>")]
pub fn create_card(db: &State<Db>, id: i64, form: Form<CardForm>) -> Result<Redirect, Status> {
    let title = form.title.trim();
    if title.is_empty() {
        return Err(Status::BadRequest);
    }

    let conn = db.lock();
    Project::find(&conn, id).ok_or(Status::NotFound)?;

    Card::create(
        &conn,
        NewCard {
            project_id: id,
            title,
            description: form.description.trim(),
            base_branch: form.base_branch.trim(),
            permission_mode: permission_mode(&form.permission_mode),
            model: Some(form.model.trim()).filter(|m| !m.is_empty()),
        },
    )
    .map_err(|_| Status::InternalServerError)?;

    Ok(Redirect::to(format!("/projects/{id}")))
}

/// Only modes the form offers are accepted; anything else is someone poking at
/// the endpoint, and `acceptEdits` is the safe reading.
fn permission_mode(requested: &str) -> &'static str {
    PERMISSION_MODES
        .iter()
        .find(|(mode, _)| *mode == requested)
        .map_or("acceptEdits", |(mode, _)| *mode)
}

#[derive(rocket::FromForm)]
pub struct MoveForm {
    lane: String,
    index: usize,
}

#[post("/cards/<id>/move", data = "<form>")]
pub fn move_card(
    db: &State<Db>,
    agents: &State<Agents>,
    auth: &State<HookAuth>,
    settings: &State<Settings>,
    id: i64,
    form: Form<MoveForm>,
) -> Result<Status, Status> {
    let lane = Lane::parse(&form.lane);

    let conn = db.lock();
    let card = Card::find(&conn, id).ok_or(Status::NotFound)?;
    Card::reorder(&conn, id, card.project_id, lane, form.index);
    drop(conn);

    // Entering In Progress is what creates the worktree and starts the agent.
    // Re-entering it with a live agent is a no-op.
    if lane == Lane::InProgress && card.lane != Lane::InProgress {
        if let Err(err) = session::start(db, agents, auth, settings, id) {
            error!("card {id}: {err:#}");
            session::set_state(db, id, AgentState::Error);
        }
    }

    Ok(Status::NoContent)
}

#[post("/cards/<id>/delete")]
pub fn delete_card(
    db: &State<Db>,
    agents: &State<Agents>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    id: i64,
) -> Result<Redirect, Status> {
    let project_id = {
        let conn = db.lock();
        Card::find(&conn, id).ok_or(Status::NotFound)?.project_id
    };

    session::teardown(db, agents, settings, cache, id);
    Card::delete(&db.lock(), id).map_err(|_| Status::InternalServerError)?;

    Ok(Redirect::to(format!("/projects/{project_id}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_offered_permission_modes_are_accepted() {
        assert_eq!(permission_mode("bypassPermissions"), "bypassPermissions");
        assert_eq!(permission_mode("plan"), "plan");
        // Anything unrecognised lands on the conservative default.
        assert_eq!(permission_mode("rm -rf"), "acceptEdits");
        assert_eq!(permission_mode(""), "acceptEdits");
    }
}
