use std::path::{Path, PathBuf};
use std::sync::Arc;

use rocket::http::Status;
use rocket::serde::json::Json;
use rocket::{post, State};
use serde_json::{json, Value};

use crate::agent::messaging::Inbox;
use crate::agent::AgentManager;
use crate::config::Settings;
use crate::db::Db;
use crate::events::Kind;
use crate::project::lifecycle;
use crate::project::{Card, Project};
use crate::review::{DiffCache, Turn};
use crate::watch::Worktrees;

/// Receives a session's inbox socket, reported by the `SessionStart` hook.
///
/// NB: off `/hooks` on purpose. This is posted by a re-execution of this binary
/// (`hooks::dispatch`), not by Claude Code, so it carries none of the turn
/// payload the others share — and it must not be recorded as a hook event,
/// because its arrival proves only that *we* can reach ourselves. Whether Claude
/// Code's own HTTP hooks land is the separate question `watch_startup` asks.
#[post("/inbox/<token>/<card_id>", data = "<payload>")]
pub fn session_start(
    manager: &State<Arc<AgentManager>>,
    token: &str,
    card_id: i64,
    payload: Json<Value>,
) -> Result<Json<Value>, Status> {
    if !manager.verify(token) {
        return Err(Status::Forbidden);
    }

    let socket = payload
        .get("socket")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if socket.is_empty() {
        return Err(Status::BadRequest);
    }

    if let Some(agent) = manager.running(card_id) {
        agent.set_inbox(Inbox {
            socket: socket.to_owned(),
            token: payload
                .get("token")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        });
    }

    Ok(Json(json!({})))
}

/// Receives Claude Code's HTTP hooks. Always answers 200 with an empty decision:
/// a hook that blocks or errors would stall the agent, and nothing here is worth
/// interrupting a turn for.
// NB: every parameter below the token is a Rocket request guard, which is how
// the framework injects managed state. Grouping them to please the lint would
// mean a hand-written `FromRequest` whose only purpose is a smaller signature.
#[allow(clippy::too_many_arguments)]
#[post("/hooks/<token>/<card_id>/<event>", data = "<payload>")]
pub async fn receive(
    db: &State<Db>,
    manager: &State<Arc<AgentManager>>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    worktrees: &State<Worktrees>,
    token: &str,
    card_id: i64,
    event: &str,
    payload: Json<Value>,
) -> Result<Json<Value>, Status> {
    if !manager.verify(token) {
        return Err(Status::Forbidden);
    }

    let server = Server {
        db,
        manager,
        settings,
        cache,
        worktrees,
    };

    answer(&server, card_id, event, payload).await
}

/// Everything a hook can reach.
///
/// A struct because answering one takes all five, and a call listing them reads
/// as nothing at all.
struct Server<'a> {
    db: &'a Db,
    manager: &'a Arc<AgentManager>,
    settings: &'a Settings,
    cache: &'a DiffCache,
    worktrees: &'a Worktrees,
}

/// What a hook actually does, once it has proved who it is.
///
/// Separate so the token check reads as the one thing standing in front of it.
async fn answer(
    server: &Server<'_>,
    card_id: i64,
    event: &str,
    payload: Json<Value>,
) -> Result<Json<Value>, Status> {
    let Server {
        db,
        manager,
        settings,
        cache,
        worktrees,
    } = server;

    record_event(db, card_id, event, &payload).await;

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
        Card::set_session_id(db, card_id, session_id).await;
    }

    let title = match transcript {
        Some(path) => session_title(path).await,
        None => None,
    };
    if let Some(title) = title {
        Card::set_title(db, card_id, &title).await;
    }

    match event {
        "prompt" => manager.turn_started(card_id).await,
        // NB: a tool permission, a question, a plan to approve and an MCP
        // elicitation all arrive here — which is why the card says "needs you"
        // rather than naming one of them. Nothing reports the answer, so the
        // terminal watcher is armed here and clears the card once the dialog
        // leaves the screen.
        "needs-user" => {
            manager.needs_user(card_id).await;
            if let Some(agent) = manager.running(card_id) {
                agent.expect_dialog();
            }
        }
        "stop" => on_stop(db, manager, settings, cache, worktrees, card_id, &payload).await,
        "end" => manager.session_ended(card_id).await,
        _ => {}
    }

    Ok(Json(json!({})))
}

async fn on_stop(
    db: &Db,
    manager: &Arc<AgentManager>,
    settings: &Settings,
    cache: &DiffCache,
    worktrees: &Worktrees,
    card_id: i64,
    payload: &Value,
) {
    let changes = manager.changes();
    let last_message = payload
        .get("last_assistant_message")
        .and_then(Value::as_str)
        .unwrap_or_default();

    match snapshot(db, settings, card_id, last_message).await {
        // The turn is a new point in the picker, and its message is what the
        // pane's footer shows. Nothing else announces it: a commit the agent
        // made touches only `.git`, which the worktree watcher filters out.
        Ok(()) => changes.card(db, card_id, Kind::Diff).await,
        Err(err) => error!("card {card_id}: snapshotting the turn failed: {err:#}"),
    }

    if is_paused(payload) {
        return;
    }

    // The state and the lane it implies, under one lock.
    manager.turn_ended(card_id).await;

    // Last, so that a merge landing this turn overrides the lane and state set
    // above with Done and a torn-down worktree.
    lifecycle::check_merge(manager, settings, cache, worktrees, card_id).await;
}

/// A non-empty `background_tasks` means the turn ended but work is still in
/// flight and will wake the session back up. That is not idle.
fn is_paused(payload: &Value) -> bool {
    payload
        .get("background_tasks")
        .and_then(Value::as_array)
        .is_some_and(|tasks| !tasks.is_empty())
}

async fn snapshot(
    db: &Db,
    settings: &Settings,
    card_id: i64,
    last_message: &str,
) -> anyhow::Result<()> {
    let Some(card) = Card::find(db, card_id).await else {
        return Ok(());
    };
    let Some(project) = Project::find(db, card.project_id).await else {
        return Ok(());
    };
    let Some(worktree) = card.worktree_path.clone().map(PathBuf::from) else {
        return Ok(());
    };

    Turn::snapshot(
        db,
        settings,
        card_id,
        &project.repo(),
        &worktree,
        last_message,
    )
    .await?;
    Ok(())
}

/// What the client calls this session.
///
/// NB: `session_title` on a hook payload only carries a name given with
/// `--name`; a generated one can only be had from the transcript.
async fn session_title(transcript: &Path) -> Option<String> {
    title_in(&tokio::fs::read_to_string(transcript).await.ok()?)
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

async fn record_event(db: &Db, card_id: i64, kind: &str, payload: &Value) {
    let _ = sqlx::query("INSERT INTO events (card_id, kind, payload_json) VALUES (?1, ?2, ?3)")
        .bind(card_id)
        .bind(kind)
        .bind(payload.to_string())
        .execute(db.pool())
        .await;
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
