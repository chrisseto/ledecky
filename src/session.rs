use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::agent::{Agent, Agents};
use crate::config;
use crate::db::Db;
use crate::git;
use crate::hooks::HookAuth;
use crate::models::AgentState;
use crate::queries;

/// How long the TUI needs before it will accept pasted input.
///
/// NB: this is a timer rather than a `SessionStart` hook because that event only
/// accepts `command` and `mcp_tool` handlers — an HTTP hook there never fires.
const READY_DELAY: Duration = Duration::from_millis(2500);

/// How long to wait after the opening prompt for *any* hook to come back before
/// concluding our URLs are not reaching us.
const HOOK_GRACE: Duration = Duration::from_secs(15);

/// How long to keep retrying the opening prompt while a modal holds the keyboard.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(300);

/// Creates the card's worktree if needed and starts its agent.
pub fn start(db: &Db, agents: &Agents, auth: &HookAuth, card_id: i64) -> Result<Arc<Agent>> {
    let conn = db.lock();
    let card = queries::card(&conn, card_id).context("no such card")?;
    let project = queries::project(&conn, card.project_id).context("no such project")?;
    drop(conn);

    if let Some(existing) = agents.get(card_id) {
        if existing.is_running() {
            return Ok(existing);
        }
    }

    let repo = PathBuf::from(&project.path);
    let worktree = config::worktree_path(card_id);

    if !worktree.join(".git").exists() {
        git::create_worktree(&repo, &worktree, &card.base_branch, card_id)
            .context("creating the worktree")?;
    }

    {
        let conn = db.lock();
        let _ = conn.execute(
            "UPDATE cards SET worktree_path = ?1, agent_state = ?2, updated_at = datetime('now')
             WHERE id = ?3",
            rusqlite::params![worktree.to_string_lossy(), AgentState::Starting, card_id],
        );
    }

    let card = {
        let conn = db.lock();
        queries::card(&conn, card_id).context("card vanished")?
    };

    let agent = agents
        .spawn(&card, &worktree, &repo, &auth.settings_json(card_id))
        .context("spawning claude")?;

    let started = agent.clone();
    let db = db.clone();
    std::thread::spawn(move || deliver_opening_prompt(db, started, card_id));

    Ok(agent)
}

/// Keeps trying to hand the agent its opening task.
///
/// Delivery fails while a modal owns the keyboard — the workspace trust prompt,
/// or the consent dialog `bypassPermissions` shows the first time. Those are the
/// user's to answer in the terminal pane, so the card reports that it is waiting
/// on them rather than forcing an answer.
fn deliver_opening_prompt(db: Db, agent: Arc<Agent>, card_id: i64) {
    std::thread::sleep(READY_DELAY);

    let mut waited = Duration::ZERO;
    loop {
        if !agent.is_running() {
            return;
        }
        if agent.flush_pending_prompt() {
            break;
        }
        if waited.is_zero() {
            set_state(&db, card_id, AgentState::AwaitingPermission);
        }
        if waited >= PROMPT_TIMEOUT {
            warn!("card {card_id}: could not deliver the opening prompt; the terminal is blocked");
            return;
        }
        std::thread::sleep(READY_DELAY);
        waited += READY_DELAY;
    }

    set_state(&db, card_id, AgentState::Running);

    // Silence past this point means our hook URLs are not reaching us — most
    // likely an `allowedHttpHookUrls` allowlist. Without hooks there are no turn
    // snapshots and no lane transitions, so say so on the card.
    std::thread::sleep(HOOK_GRACE);
    if agent.is_running() && hook_events(&db, card_id) == 0 {
        warn!("card {card_id}: no hooks received within {HOOK_GRACE:?}");
        set_state(&db, card_id, AgentState::Misconfigured);
    }
}

fn hook_events(db: &Db, card_id: i64) -> i64 {
    db.lock()
        .query_row(
            "SELECT COUNT(*) FROM events WHERE card_id = ?1",
            [card_id],
            |r| r.get(0),
        )
        .unwrap_or(0)
}

/// Kills the agent but leaves the worktree and its refs in place.
pub fn stop(db: &Db, agents: &Agents, card_id: i64) {
    if let Some(agent) = agents.remove(card_id) {
        agent.kill();
    }
    set_state(db, card_id, AgentState::Stopped);
}

/// Kills the agent and removes the worktree. Turn refs are kept.
pub fn teardown(db: &Db, agents: &Agents, card_id: i64) {
    stop(db, agents, card_id);

    let conn = db.lock();
    let card = queries::card(&conn, card_id);
    let project = card
        .as_ref()
        .and_then(|c| queries::project(&conn, c.project_id));
    drop(conn);

    if let (Some(card), Some(project)) = (card, project) {
        let worktree = card
            .worktree_path
            .map(PathBuf::from)
            .unwrap_or_else(|| config::worktree_path(card_id));
        git::remove_worktree(&PathBuf::from(project.path), &worktree);

        let conn = db.lock();
        let _ = conn.execute(
            "UPDATE cards SET worktree_path = NULL, session_id = NULL WHERE id = ?1",
            [card_id],
        );
    }
}

pub fn set_state(db: &Db, card_id: i64, state: AgentState) {
    let conn = db.lock();
    let _ = conn.execute(
        "UPDATE cards SET agent_state = ?1, updated_at = datetime('now') WHERE id = ?2",
        rusqlite::params![state, card_id],
    );
}

/// Asks the agent to land its work on the base branch.
///
/// The server never rewrites the user's branches itself — conflicts are exactly
/// the situation an agent is good at, and a failed rebase run by the server
/// would just leave a mess for someone else to unpick.
pub fn request_merge(db: &Db, agents: &Agents, card_id: i64) -> Result<()> {
    let conn = db.lock();
    let card = queries::card(&conn, card_id).context("no such card")?;
    let project = queries::project(&conn, card.project_id).context("no such project")?;
    drop(conn);

    let agent = agents
        .get(card_id)
        .filter(|a| a.is_running())
        .context("the agent is not running")?;

    let repo = PathBuf::from(&project.path);
    let base_sha = git::run(&repo, &["rev-parse", &card.base_branch])
        .with_context(|| format!("resolving {}", card.base_branch))?;

    // NB: the base branch is almost always checked out in the main worktree, and
    // git refuses to move a branch that another worktree holds. Saying so up
    // front saves the agent a failed `git branch -f` and a round of guessing.
    let prompt = format!(
        "The reviewer approved this work. Land it on `{branch}`:\n\n\
         1. Commit anything still outstanding in this worktree.\n\
         2. `{branch}` is checked out in the main repository at `{repo}`, so it cannot be moved \
            from here. Apply your commits there instead — `git -C {repo} merge --ff-only <sha>`, \
            or rebase onto `{branch}` first if it has moved ahead.\n\
         3. Report the final SHA of `{branch}`.\n\n\
         Do not push.",
        branch = card.base_branch,
        repo = repo.display(),
    );

    if !agent.inject(&prompt) {
        anyhow::bail!("the terminal is busy; answer the prompt showing in it first");
    }

    let conn = db.lock();
    let _ = conn.execute(
        "UPDATE cards SET merge_requested = 1, merge_base_sha = ?1 WHERE id = ?2",
        rusqlite::params![base_sha, card_id],
    );

    Ok(())
}

/// Called after each turn snapshot while a merge is outstanding.
///
/// Success means the base branch moved *and* now carries the same tree as the
/// latest snapshot — proof the work actually landed, which a plain ancestry check
/// would miss after a rebase rewrote the commits.
pub fn check_merge(db: &Db, agents: &Agents, card_id: i64) {
    let conn = db.lock();
    let Some(card) = queries::card(&conn, card_id).filter(|c| c.merge_requested) else {
        return;
    };
    let Some(project) = queries::project(&conn, card.project_id) else {
        return;
    };
    let before: Option<String> = conn
        .query_row(
            "SELECT merge_base_sha FROM cards WHERE id = ?1",
            [card_id],
            |r| r.get(0),
        )
        .ok();
    let latest = queries::turns(&conn, card_id).pop();
    drop(conn);

    let repo = PathBuf::from(&project.path);
    let Ok(after) = git::run(&repo, &["rev-parse", &card.base_branch]) else {
        return;
    };

    if Some(&after) == before.as_ref() {
        info!("card {card_id}: {} has not moved yet", card.base_branch);
        return;
    }

    let base_tree = git::run(&repo, &["rev-parse", &format!("{after}^{{tree}}")]).ok();
    let turn_tree = latest
        .and_then(|t| git::run(&repo, &["rev-parse", &format!("{}^{{tree}}", t.commit_sha)]).ok());

    if base_tree.is_none() || base_tree != turn_tree {
        warn!("card {card_id}: {} moved but does not match the worktree", card.base_branch);
        return;
    }

    info!("card {card_id}: merged into {} at {after}", card.base_branch);
    {
        let conn = db.lock();
        queries::set_lane(&conn, card_id, crate::models::Lane::Done);
        let _ = conn.execute(
            "UPDATE cards SET merge_requested = 0 WHERE id = ?1",
            [card_id],
        );
    }
    teardown(db, agents, card_id);
}
