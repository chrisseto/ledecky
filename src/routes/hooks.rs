use std::path::PathBuf;

use rocket::http::Status;
use rocket::serde::json::{json, Json, Value};
use rocket::{post, State};

use crate::agent::Agents;
use crate::db::Db;
use crate::hooks::HookAuth;
use crate::models::{AgentState, Lane};
use crate::{git, queries, session};

/// Receives Claude Code's HTTP hooks. Always answers 200 with an empty decision:
/// a hook that blocks or errors would stall the agent, and nothing here is worth
/// interrupting a turn for.
#[post("/hooks/<token>/<card_id>/<event>", data = "<payload>")]
pub fn receive(
    db: &State<Db>,
    agents: &State<Agents>,
    auth: &State<HookAuth>,
    token: &str,
    card_id: i64,
    event: &str,
    payload: Json<Value>,
) -> Result<Json<Value>, Status> {
    if !auth.matches(token) {
        return Err(Status::Forbidden);
    }

    record_event(db, card_id, event, &payload);

    // Every event carries `session_id`, and `SessionStart` is unavailable over
    // HTTP, so learn it from whichever hook arrives first.
    if let Some(session_id) = payload.get("session_id").and_then(Value::as_str) {
        let conn = db.lock();
        let _ = conn.execute(
            "UPDATE cards SET session_id = ?1 WHERE id = ?2 AND session_id IS NOT ?1",
            rusqlite::params![session_id, card_id],
        );
    }

    match event {
        "prompt" => session::set_state(db, card_id, AgentState::Running),

        "permission" => session::set_state(db, card_id, AgentState::AwaitingPermission),

        "stop" => on_stop(db, agents, card_id, &payload),

        "end" => session::set_state(db, card_id, AgentState::Stopped),

        _ => {}
    }

    Ok(Json(json!({})))
}

fn on_stop(db: &Db, agents: &Agents, card_id: i64, payload: &Value) {
    let last_message = payload
        .get("last_assistant_message")
        .and_then(Value::as_str)
        .unwrap_or_default();

    if let Err(err) = snapshot(db, card_id, last_message) {
        error!("card {card_id}: snapshotting the turn failed: {err:#}");
    }

    // A non-empty `background_tasks` means the turn ended but work is still in
    // flight and will wake the session back up. That is not idle.
    let paused = payload
        .get("background_tasks")
        .and_then(Value::as_array)
        .is_some_and(|tasks| !tasks.is_empty());
    if paused {
        return;
    }

    session::set_state(db, card_id, AgentState::Idle);

    let conn = db.lock();
    let lane = queries::card(&conn, card_id).map(|c| c.lane);
    drop(conn);

    if lane == Some(Lane::InProgress) {
        let conn = db.lock();
        queries::set_lane(&conn, card_id, Lane::InReview);
    }

    // Last, so that a merge landing this turn overrides the lane and state set
    // above with Done and a torn-down worktree.
    session::check_merge(db, agents, card_id);
}

fn snapshot(db: &Db, card_id: i64, last_message: &str) -> anyhow::Result<()> {
    let conn = db.lock();
    let Some(card) = queries::card(&conn, card_id) else {
        return Ok(());
    };
    let Some(project) = queries::project(&conn, card.project_id) else {
        return Ok(());
    };
    let Some(worktree) = card.worktree_path.clone().map(PathBuf::from) else {
        return Ok(());
    };

    let n: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(n), 0) + 1 FROM turns WHERE card_id = ?1",
            [card_id],
            |r| r.get(0),
        )
        .unwrap_or(1);
    let parent: String = conn
        .query_row(
            "SELECT commit_sha FROM turns WHERE card_id = ?1 ORDER BY n DESC LIMIT 1",
            [card_id],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| crate::config::base_ref(card_id));
    drop(conn);

    let repo = PathBuf::from(&project.path);
    let parent_sha = git::run(&repo, &["rev-parse", &parent])?;

    let Some(sha) = git::snapshot_turn(&repo, &worktree, card_id, n, &parent_sha)? else {
        return Ok(()); // nothing changed on disk; a chat-only turn
    };

    let conn = db.lock();
    let _ = conn.execute(
        "INSERT INTO turns (card_id, n, ref_name, commit_sha, parent_sha, last_assistant_message)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            card_id,
            n,
            crate::config::turn_ref(card_id, n),
            sha,
            parent_sha,
            last_message
        ],
    );

    Ok(())
}

fn record_event(db: &Db, card_id: i64, kind: &str, payload: &Value) {
    let conn = db.lock();
    let _ = conn.execute(
        "INSERT INTO events (card_id, kind, payload_json) VALUES (?1, ?2, ?3)",
        rusqlite::params![card_id, kind, payload.to_string()],
    );
}
