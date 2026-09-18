use std::path::{Path, PathBuf};

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

    // NB: only counts if the transcript is on disk. With transcript saving off
    // the client still reports an id, but `--resume` will never find it — and a
    // card holding one of those could not be started again.
    let transcript = payload
        .get("transcript_path")
        .and_then(Value::as_str)
        .map(Path::new)
        .filter(|path| path.exists());

    // Every event carries `session_id`, and `SessionStart` is unavailable over
    // HTTP, so learn it from whichever hook arrives first.
    if let (Some(session_id), Some(_)) = (
        payload.get("session_id").and_then(Value::as_str),
        transcript,
    ) {
        Card::set_session_id(&db.lock(), card_id, session_id);
    }

    if let Some(title) = transcript.and_then(session_title) {
        Card::set_title(&db.lock(), card_id, &title);
    }

    match event {
        "prompt" => session::resume(db, card_id),
        // NB: a tool permission, a question, a plan to approve and an MCP
        // elicitation all arrive here — which is why the card says "needs you"
        // rather than naming one of them. Nothing reports the answer, so the
        // terminal watcher is armed here and clears the card once the dialog
        // leaves the screen.
        "needs-user" => {
            session::await_user(db, card_id);
            if let Some(agent) = agents.get(card_id) {
                agent.expect_dialog();
            }
        }
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

/// What the client calls this session.
///
/// NB: `session_title` on a hook payload only carries a name given with
/// `--name`; a generated one can only be had from the transcript.
fn session_title(transcript: &Path) -> Option<String> {
    title_in(&std::fs::read_to_string(transcript).ok()?)
}

/// A session's own name, preferred over the one it generated for itself.
///
/// Both are standalone metadata lines among the messages, re-appended whenever
/// the transcript is rewritten, so the last of each kind is the current one.
fn title_in(transcript: &str) -> Option<String> {
    // `None` until a line of that kind has been read, so that the newest one
    // settles it even when what it holds is an empty string — a cleared title,
    // which must not fall back to an older line of its own kind.
    let mut given: Option<Option<String>> = None;
    let mut generated: Option<Option<String>> = None;

    // One pass, from the end, stopping as soon as the answer cannot change:
    // transcripts run to megabytes. The substring test keeps `from_str` off the
    // messages, which are almost all of it.
    for line in transcript.lines().rev() {
        let field = match line {
            _ if given.is_none() && line.contains("custom-title") => "customTitle",
            _ if generated.is_none() && line.contains("ai-title") => "aiTitle",
            _ => continue,
        };

        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(title) = entry.get(field).and_then(Value::as_str) else {
            continue;
        };

        let title = Some(title.trim().to_owned()).filter(|t| !t.is_empty());
        match field {
            "customTitle" => given = Some(title),
            _ => generated = Some(title),
        }

        // A name the session was given outranks whatever it generated, so the
        // rest of the file cannot change the answer.
        if matches!(given, Some(Some(_))) || generated.is_some() && given.is_some() {
            break;
        }
    }

    given.flatten().or_else(|| generated.flatten())
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

    /// Metadata lines as the client writes them, with a couple of messages
    /// between to stand in for the rest of a transcript.
    fn transcript(lines: &[&str]) -> String {
        lines
            .iter()
            .flat_map(|line| [r#"{"type":"user","message":{}}"#, line])
            .map(|line| format!("{line}\n"))
            .collect()
    }

    #[test]
    fn a_session_that_named_itself_is_read_off_its_transcript() {
        let text = transcript(&[
            r#"{"type":"ai-title","aiTitle":"Teach it to whistle","sessionId":"s1"}"#,
        ]);
        assert_eq!(title_in(&text).unwrap(), "Teach it to whistle");
    }

    #[test]
    fn a_name_the_session_was_given_wins_over_one_it_made_up() {
        let text = transcript(&[
            r#"{"type":"custom-title","customTitle":"What the human called it","sessionId":"s1"}"#,
            r#"{"type":"ai-title","aiTitle":"What the model called it","sessionId":"s1"}"#,
        ]);
        // Ordering does not decide it: a custom title outranks a generated one
        // wherever it sits.
        assert_eq!(title_in(&text).unwrap(), "What the human called it");
    }

    #[test]
    fn the_last_of_a_kind_is_the_current_one() {
        // Titles are re-appended whenever the transcript is rewritten, so a
        // rename leaves the superseded ones behind it in the file.
        let text = transcript(&[
            r#"{"type":"ai-title","aiTitle":"First guess","sessionId":"s1"}"#,
            r#"{"type":"ai-title","aiTitle":"Second guess","sessionId":"s1"}"#,
        ]);
        assert_eq!(title_in(&text).unwrap(), "Second guess");
    }

    #[test]
    fn a_transcript_with_no_title_yet_leaves_the_card_alone() {
        assert_eq!(title_in(&transcript(&[])), None);
        assert_eq!(title_in(""), None);
        // A cleared title is not a title, and must not fall back to an older one.
        let cleared = transcript(&[
            r#"{"type":"ai-title","aiTitle":"Stale","sessionId":"s1"}"#,
            r#"{"type":"ai-title","aiTitle":"  ","sessionId":"s1"}"#,
        ]);
        assert_eq!(title_in(&cleared), None);
    }

    #[test]
    fn a_line_that_only_mentions_a_title_is_not_one() {
        // A message quoting the marker, and a half-written line: neither is a
        // title entry, and the truncated one must not take the scan down with it.
        let text = transcript(&[
            r#"{"type":"user","message":{"text":"grep for \"type\":\"ai-title\" in there"}}"#,
            r#"{"type":"ai-title","aiTitle":"Teach it t"#,
        ]);
        assert_eq!(title_in(&text), None);
    }
}
