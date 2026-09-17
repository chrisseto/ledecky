use std::path::PathBuf;

use rocket::http::Status;
use rocket::serde::json::Json;
use rocket::{post, State};
use serde_json::{json, Value};

use crate::agent::{session, Agents};
use crate::config::Settings;
use crate::db::Db;
use crate::hooks::HookAuth;
use crate::project::{AgentState, Card, Lane, Project};
use crate::review::{DiffCache, Turn};

/// Receives Claude Code's HTTP hooks. Always answers 200 with an empty decision:
/// a hook that blocks or errors would stall the agent, and nothing here is worth
/// interrupting a turn for.
#[post("/hooks/<token>/<card_id>/<event>", data = "<payload>")]
pub fn receive(
    db: &State<Db>,
    agents: &State<Agents>,
    auth: &State<HookAuth>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
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
    //
    // NB: only if its transcript is on disk. With transcript saving off the
    // client still reports an id, but `--resume` will never find it — and a
    // card holding one of those could not be started again.
    if let Some(session_id) = payload.get("session_id").and_then(Value::as_str) {
        let saved = payload
            .get("transcript_path")
            .and_then(Value::as_str)
            .is_some_and(|path| std::path::Path::new(path).exists());

        if saved {
            Card::set_session_id(&db.lock(), card_id, session_id);
        }
    }

    match event {
        "prompt" => session::set_state(db, card_id, AgentState::Running),
        "permission" => session::set_state(db, card_id, AgentState::AwaitingPermission),
        "stop" => on_stop(db, agents, settings, cache, card_id, &payload),
        "end" => session::set_state(db, card_id, AgentState::Stopped),
        _ => {}
    }

    Ok(Json(json!({})))
}

fn on_stop(
    db: &Db,
    agents: &Agents,
    settings: &Settings,
    cache: &DiffCache,
    card_id: i64,
    payload: &Value,
) {
    let last_message = payload
        .get("last_assistant_message")
        .and_then(Value::as_str)
        .unwrap_or_default();

    if let Err(err) = snapshot(db, settings, card_id, last_message) {
        error!("card {card_id}: snapshotting the turn failed: {err:#}");
    }

    if is_paused(payload) {
        return;
    }

    session::set_state(db, card_id, AgentState::Idle);

    let conn = db.lock();
    let lane = Card::find(&conn, card_id).map(|c| c.lane);
    if lane == Some(Lane::InProgress) {
        Card::set_lane(&conn, card_id, Lane::InReview);
    }
    drop(conn);

    // Last, so that a merge landing this turn overrides the lane and state set
    // above with Done and a torn-down worktree.
    session::check_merge(db, agents, settings, cache, card_id);
}

/// A non-empty `background_tasks` means the turn ended but work is still in
/// flight and will wake the session back up. That is not idle.
fn is_paused(payload: &Value) -> bool {
    payload
        .get("background_tasks")
        .and_then(Value::as_array)
        .is_some_and(|tasks| !tasks.is_empty())
}

fn snapshot(db: &Db, settings: &Settings, card_id: i64, last_message: &str) -> anyhow::Result<()> {
    let conn = db.lock();
    let Some(card) = Card::find(&conn, card_id) else {
        return Ok(());
    };
    let Some(project) = Project::find(&conn, card.project_id) else {
        return Ok(());
    };
    let Some(worktree) = card.worktree_path.clone().map(PathBuf::from) else {
        return Ok(());
    };

    Turn::snapshot(
        &conn,
        settings,
        card_id,
        &project.repo(),
        &worktree,
        last_message,
    )?;
    Ok(())
}

fn record_event(db: &Db, card_id: i64, kind: &str, payload: &Value) {
    let _ = db.lock().execute(
        "INSERT INTO events (card_id, kind, payload_json) VALUES (?1, ?2, ?3)",
        rusqlite::params![card_id, kind, payload.to_string()],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_flight_background_work_is_not_idle() {
        let payload = json!({
            "background_tasks": [{ "id": "t1", "type": "shell", "status": "running" }],
        });
        assert!(is_paused(&payload));
    }

    #[test]
    fn a_finished_turn_with_nothing_pending_is_idle() {
        assert!(!is_paused(&json!({ "background_tasks": [] })));
        // Older payloads may omit the key entirely.
        assert!(!is_paused(&json!({})));
        assert!(!is_paused(&json!({ "background_tasks": null })));
    }
}
