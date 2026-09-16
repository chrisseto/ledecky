use std::path::PathBuf;

use minijinja::context;
use rocket::form::Form;
use rocket::http::Status;
use rocket::response::Redirect;
use rocket::{get, post, State};

use crate::db::Db;
use crate::agent::Agents;
use crate::hooks::HookAuth;
use crate::models::{AgentState, Lane};
use crate::tmpl::Tmpl;
use crate::{git, queries, session};

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
    let project = queries::project(&conn, id).ok_or(Status::NotFound)?;
    let cards = queries::cards_for_project(&conn, id);
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
    let project = queries::project(&conn, id).ok_or(Status::NotFound)?;
    drop(conn);

    let branches = git::branches(&PathBuf::from(&project.path));
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

    let permission_mode = PERMISSION_MODES
        .iter()
        .find(|(m, _)| *m == form.permission_mode)
        .map_or("acceptEdits", |(m, _)| *m);
    let model = Some(form.model.trim()).filter(|m| !m.is_empty());

    let conn = db.lock();
    queries::project(&conn, id).ok_or(Status::NotFound)?;

    let position: f64 = conn
        .query_row(
            "SELECT COALESCE(MAX(position), -1) + 1 FROM cards WHERE project_id = ?1 AND lane = 'todo'",
            [id],
            |r| r.get(0),
        )
        .unwrap_or(0.0);

    conn.execute(
        "INSERT INTO cards (project_id, title, description, base_branch, position, permission_mode, model)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            id,
            title,
            form.description.trim(),
            form.base_branch.trim(),
            position,
            permission_mode,
            model
        ],
    )
    .map_err(|_| Status::InternalServerError)?;

    Ok(Redirect::to(format!("/projects/{id}")))
}

#[derive(rocket::FromForm)]
pub struct MoveForm {
    lane: String,
    index: usize,
}

/// Reorders a card within, or across, lanes. The client sends the drop index and
/// the server rewrites the whole lane's positions — small N, no float drift.
#[post("/cards/<id>/move", data = "<form>")]
pub fn move_card(
    db: &State<Db>,
    agents: &State<Agents>,
    auth: &State<HookAuth>,
    id: i64,
    form: Form<MoveForm>,
) -> Result<Status, Status> {
    let lane = Lane::parse(&form.lane);

    let conn = db.lock();
    let card = queries::card(&conn, id).ok_or(Status::NotFound)?;

    conn.execute(
        "UPDATE cards SET lane = ?1, updated_at = datetime('now') WHERE id = ?2",
        rusqlite::params![lane, id],
    )
    .map_err(|_| Status::InternalServerError)?;

    // Rewrite positions for the destination lane with the card slotted in.
    let mut ids: Vec<i64> = conn
        .prepare(
            "SELECT id FROM cards WHERE project_id = ?1 AND lane = ?2 AND id != ?3 ORDER BY position",
        )
        .and_then(|mut s| {
            s.query_map(rusqlite::params![card.project_id, lane, id], |r| r.get(0))
                .map(|rows| rows.filter_map(Result::ok).collect())
        })
        .unwrap_or_default();

    ids.insert(form.index.min(ids.len()), id);
    for (pos, card_id) in ids.iter().enumerate() {
        let _ = conn.execute(
            "UPDATE cards SET position = ?1 WHERE id = ?2",
            rusqlite::params![pos as f64, card_id],
        );
    }
    drop(conn);

    // Entering In Progress is what creates the worktree and starts the agent.
    // Re-entering it with a live agent is a no-op.
    if lane == Lane::InProgress && card.lane != Lane::InProgress {
        if let Err(err) = session::start(db, agents, auth, id) {
            error!("starting card {id}: {err:#}");
            session::set_state(db, id, AgentState::Error);
        }
    }

    Ok(Status::NoContent)
}

#[post("/cards/<id>/delete")]
pub fn delete_card(db: &State<Db>, agents: &State<Agents>, id: i64) -> Result<Redirect, Status> {
    let project_id = {
        let conn = db.lock();
        queries::card(&conn, id).ok_or(Status::NotFound)?.project_id
    };

    session::teardown(db, agents, id);

    let conn = db.lock();
    conn.execute("DELETE FROM cards WHERE id = ?1", [id])
        .map_err(|_| Status::InternalServerError)?;

    Ok(Redirect::to(format!("/projects/{project_id}")))
}


