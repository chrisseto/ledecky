use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::agent::{Agent, Agents};
use crate::config::Settings;
use crate::db::Db;
use crate::git;
use crate::hooks::HookAuth;
use crate::project::{AgentState, Card, Lane, Project};
use crate::review::DiffCache;

/// How long the TUI needs before it will accept pasted input.
///
/// NB: a timer rather than a `SessionStart` hook, because that event only
/// accepts `command` and `mcp_tool` handlers — an HTTP hook there never fires.
const READY_DELAY: Duration = Duration::from_millis(2500);

/// How long to wait after the opening prompt for *any* hook to come back before
/// concluding our URLs are not reaching us.
const HOOK_GRACE: Duration = Duration::from_secs(15);

/// How long to wait for a resumed session to prove itself one way or the other.
const RESUME_POLL: Duration = Duration::from_millis(100);
const RESUME_CHECKS: u32 = 50;

/// How long to keep retrying the opening prompt while a modal holds the keyboard.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(300);

/// Creates the card's worktree if needed and starts its agent.
pub fn start(
    db: &Db,
    agents: &Agents,
    auth: &HookAuth,
    settings: &Settings,
    card_id: i64,
) -> Result<Arc<Agent>> {
    let conn = db.lock();
    let card = Card::find(&conn, card_id).context("no such card")?;
    let project = Project::find(&conn, card.project_id).context("no such project")?;
    drop(conn);

    if let Some(existing) = agents.get(card_id) {
        if existing.is_running() {
            return Ok(existing);
        }
    }

    let repo = project.repo();
    let worktree = settings.worktree_path(card_id);

    if !worktree.join(".git").exists() {
        git::create_worktree(settings, &repo, &worktree, &card.base_branch, card_id)
            .context("creating the worktree")?;
    }

    {
        let conn = db.lock();
        Card::attach_worktree(&conn, card_id, &worktree.to_string_lossy(), None);
        Card::set_agent_state(&conn, card_id, AgentState::Starting);
    }

    let card = {
        let conn = db.lock();
        Card::find(&conn, card_id).context("card vanished")?
    };

    let mut agent = agents
        .spawn(
            settings,
            &card,
            &worktree,
            &repo,
            &auth.settings_json(card_id),
        )
        .context("spawning the agent")?;

    // A recorded session can stop being resumable — a transcript that was never
    // written, or one since pruned. `--resume` then exits before drawing
    // anything, which would leave the card unable to start at all, so forget the
    // session and come back without it.
    if card.session_id.is_some() && !resumed(&agent) {
        warn!("card {card_id}: the recorded session is gone; starting a fresh one");
        Card::clear_session_id(&db.lock(), card_id);

        let card = Card::find(&db.lock(), card_id).context("card vanished")?;
        agent = agents
            .spawn(
                settings,
                &card,
                &worktree,
                &repo,
                &auth.settings_json(card_id),
            )
            .context("spawning the agent")?;
    }

    Card::set_agent_pid(&db.lock(), card_id, agent.pid);

    let started = agent.clone();
    let db = db.clone();
    std::thread::spawn(move || deliver_opening_prompt(db, started, card_id));

    Ok(agent)
}

/// Whether a `--resume` took.
///
/// The client draws its input box when the conversation was found and exits
/// without drawing anything when it was not, so the two outcomes are told apart
/// as soon as either shows — no waiting out the whole budget on a good start.
/// An undecided run is treated as fine; the ordinary delivery path handles it.
fn resumed(agent: &Agent) -> bool {
    for _ in 0..RESUME_CHECKS {
        if !agent.is_running() {
            return false;
        }
        if agent.is_composing() || agent.is_blocked() {
            return true;
        }
        std::thread::sleep(RESUME_POLL);
    }
    true
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

        // A dialog is the user's to answer, so say so. A client that has not
        // drawn its input box yet is only slow to start, and reporting that as a
        // permission prompt sends them looking for one that is not there.
        if agent.is_blocked() {
            set_state(&db, card_id, AgentState::AwaitingPermission);
        }

        if waited >= PROMPT_TIMEOUT {
            warn!("card {card_id}: could not deliver the opening prompt; the terminal is blocked");
            return;
        }
        std::thread::sleep(READY_DELAY);
        waited += READY_DELAY;
    }

    mark_delivered(&db, card_id);

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

    let conn = db.lock();
    Card::set_agent_pid(&conn, card_id, None);
    Card::set_agent_state(&conn, card_id, AgentState::Stopped);
}

/// Kills the agent and removes the worktree. Turn refs are kept.
pub fn teardown(db: &Db, agents: &Agents, settings: &Settings, cache: &DiffCache, card_id: i64) {
    stop(db, agents, card_id);

    let conn = db.lock();
    let card = Card::find(&conn, card_id);
    let project = card
        .as_ref()
        .and_then(|c| Project::find(&conn, c.project_id));
    drop(conn);

    let (Some(card), Some(project)) = (card, project) else {
        return;
    };

    let worktree = card
        .worktree_path
        .map(PathBuf::from)
        .unwrap_or_else(|| settings.worktree_path(card_id));
    git::remove_worktree(&project.repo(), &worktree);
    // Turn refs are history and are kept; this one only ever described the
    // worktree that has just gone, and would otherwise pin its tree forever.
    let _ = git::run(
        &project.repo(),
        &["update-ref", "-d", &settings.working_ref(card_id)],
    );

    // The card's refs go with it, so anything parsed from them is dead weight.
    cache.forget(&project.repo());
    cache.forget_head(card_id);

    Card::detach_worktree(&db.lock(), card_id);
}

pub fn set_state(db: &Db, card_id: i64, state: AgentState) {
    Card::set_agent_state(&db.lock(), card_id, state);
}

/// Records that the opening prompt went in — unless the hooks have already moved
/// the card on.
///
/// Delivery is confirmed by watching the screen, which takes long enough that a
/// quick agent can finish the whole turn first. Writing `running` over the
/// `idle` its `Stop` hook just recorded would leave the card claiming to be
/// working for as long as it sat there.
fn mark_delivered(db: &Db, card_id: i64) {
    let conn = db.lock();
    let Some(card) = Card::find(&conn, card_id) else {
        return;
    };

    if matches!(
        card.agent_state,
        AgentState::Starting | AgentState::AwaitingPermission
    ) {
        Card::set_agent_state(&conn, card_id, AgentState::Running);
    }
}

/// Asks the agent to land its work on the base branch.
///
/// The server never rewrites the user's branches itself — conflicts are exactly
/// the situation an agent is good at, and a failed rebase run by the server
/// would just leave a mess for someone else to unpick.
pub fn request_merge(db: &Db, agents: &Agents, card_id: i64) -> Result<()> {
    let conn = db.lock();
    let card = Card::find(&conn, card_id).context("no such card")?;
    let project = Project::find(&conn, card.project_id).context("no such project")?;
    drop(conn);

    let agent = agents
        .get(card_id)
        .filter(|a| a.is_running())
        .context("the agent is not running")?;

    let repo = project.repo();
    let base_sha = git::run(&repo, &["rev-parse", &card.base_branch])
        .with_context(|| format!("resolving {}", card.base_branch))?;

    // Recorded before the prompt goes in: the agent can land the merge and fire
    // its `Stop` hook while delivery is still being confirmed, and a hook that
    // arrives without this reads the turn as ordinary work.
    Card::request_merge(&db.lock(), card_id, &base_sha);

    if !agent.inject(&merge_prompt(&card.base_branch, &repo)) {
        Card::clear_merge_request(&db.lock(), card_id);
        bail!("the terminal is busy; answer the prompt showing in it first");
    }

    Ok(())
}

/// NB: the base branch is almost always checked out in the main worktree, and
/// git refuses to move a branch that another worktree holds. Saying so up front
/// saves the agent a failed `git branch -f` and a round of guessing.
fn merge_prompt(branch: &str, repo: &Path) -> String {
    format!(
        "The reviewer approved this work. Land it on `{branch}`:\n\n\
         1. Commit anything still outstanding in this worktree.\n\
         2. `{branch}` is checked out in the main repository at `{repo}`, so it cannot be moved \
            from here. Apply your commits there instead — `git -C {repo} merge --ff-only <sha>`, \
            or rebase onto `{branch}` first if it has moved ahead.\n\
         3. Report the final SHA of `{branch}`.\n\n\
         Do not push.",
        repo = repo.display(),
    )
}

/// Whether an outstanding merge actually landed.
///
/// A moved branch is not enough on its own, and ancestry does not survive a
/// rebase, so the test is that the branch moved *and* now carries the same tree
/// as the snapshot taken at the end of the turn.
fn merge_landed(
    before: Option<&str>,
    after: &str,
    base_tree: Option<&str>,
    turn_tree: Option<&str>,
) -> bool {
    if Some(after) == before {
        return false;
    }
    base_tree.is_some() && base_tree == turn_tree
}

/// Called after each turn snapshot while a merge is outstanding.
pub fn check_merge(db: &Db, agents: &Agents, settings: &Settings, cache: &DiffCache, card_id: i64) {
    let conn = db.lock();
    let Some(card) = Card::find(&conn, card_id).filter(|c| c.merge_requested) else {
        return;
    };
    let Some(project) = Project::find(&conn, card.project_id) else {
        return;
    };
    let before = Card::merge_base_sha(&conn, card_id);
    let latest = crate::review::Turn::latest(&conn, card_id);
    drop(conn);

    let repo = project.repo();
    let Ok(after) = git::run(&repo, &["rev-parse", &card.base_branch]) else {
        return;
    };

    let tree_of = |rev: &str| git::run(&repo, &["rev-parse", &format!("{rev}^{{tree}}")]).ok();
    let base_tree = tree_of(&after);
    let turn_tree = latest.as_ref().and_then(|t| tree_of(&t.commit_sha));

    if !merge_landed(
        before.as_deref(),
        &after,
        base_tree.as_deref(),
        turn_tree.as_deref(),
    ) {
        info!(
            "card {card_id}: {} has not taken the work yet",
            card.base_branch
        );
        return;
    }

    info!(
        "card {card_id}: merged into {} at {after}",
        card.base_branch
    );
    {
        let conn = db.lock();
        Card::set_lane(&conn, card_id, Lane::Done);
        Card::clear_merge_request(&conn, card_id);
    }
    teardown(db, agents, settings, cache, card_id);
}

/// Kills agents left behind by a server that did not shut down cleanly.
///
/// Closing the pty master hangs up the session, so an agent normally dies with
/// us even under `SIGKILL`. This covers what that misses: a child that ignores
/// `SIGHUP`, or one that had already broken away from the terminal.
pub fn sweep_orphans(db: &Db, settings: &Settings) {
    let cards = Card::with_agent_pid(&db.lock());

    for card in cards {
        let Some(pid) = card.agent_pid else { continue };

        if owns(pid, &settings.worktrees_dir()) {
            warn!("card {}: killing orphaned agent {pid}", card.id);
            kill_pid(pid);
        }
        Card::set_agent_pid(&db.lock(), card.id, None);
        Card::set_agent_state(&db.lock(), card.id, AgentState::Stopped);
    }
}

/// Whether `pid` is a live process working inside one of our worktrees.
///
/// Pids get recycled, so a recorded number alone is not enough to justify a
/// kill; the working directory is what proves it is still ours.
fn owns(pid: i64, worktrees: &Path) -> bool {
    let Ok(cwd) = std::fs::read_link(format!("/proc/{pid}/cwd")) else {
        return false; // gone, or not ours to look at
    };
    cwd.starts_with(worktrees)
}

fn kill_pid(pid: i64) {
    let _ = std::process::Command::new("kill")
        .arg("-KILL")
        .arg(pid.to_string())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_merge_prompt_points_at_the_main_checkout() {
        let prompt = merge_prompt("main", Path::new("/srv/repo"));

        assert!(prompt.starts_with("The reviewer approved this work."));
        // The agent has to be told where the branch actually lives, or it will
        // try `git branch -f` from a worktree and be refused.
        assert!(prompt.contains("checked out in the main repository at `/srv/repo`"));
        assert!(prompt.contains("git -C /srv/repo merge --ff-only"));
        assert!(prompt.contains("Do not push."));
    }

    #[test]
    fn a_merge_needs_the_branch_to_move() {
        // Same sha before and after: nothing happened.
        assert!(!merge_landed(Some("aaa"), "aaa", Some("t1"), Some("t1")));
    }

    #[test]
    fn a_merge_needs_the_trees_to_match() {
        // Moved, but to something that is not the reviewed work.
        assert!(!merge_landed(Some("aaa"), "bbb", Some("t1"), Some("t2")));
        // Moved and carrying the reviewed tree.
        assert!(merge_landed(Some("aaa"), "bbb", Some("t1"), Some("t1")));
    }

    #[test]
    fn a_merge_is_not_claimed_without_trees_to_compare() {
        assert!(!merge_landed(Some("aaa"), "bbb", None, None));
        assert!(!merge_landed(None, "bbb", Some("t1"), None));
    }

    #[test]
    fn ownership_requires_a_cwd_under_our_worktrees() {
        // This process is not in a worktree, so it is never a sweep candidate.
        let pid = std::process::id() as i64;
        assert!(!owns(pid, Path::new("/nonexistent/worktrees")));

        // A pid that cannot exist reads as not ours rather than panicking.
        assert!(!owns(i64::from(u32::MAX), Path::new("/")));
    }

    #[test]
    fn ownership_holds_for_a_process_inside_the_worktrees_dir() {
        // The current process's cwd is by definition under its own ancestors.
        let cwd = std::env::current_dir().unwrap();
        let pid = std::process::id() as i64;
        assert!(owns(pid, &cwd));
    }

    // ---- the sweep ----------------------------------------------------------

    use crate::db::tests::memory_db;
    use crate::project::NewCard;
    use rocket::figment::providers::Serialized;
    use std::process::{Child, Command};

    fn sweep_settings(data_dir: &Path) -> Settings {
        Settings::from(
            &rocket::figment::Figment::new()
                .merge(Serialized::default("app_slug", "ledecky"))
                .merge(Serialized::default(
                    "data_dir",
                    data_dir.to_string_lossy().to_string(),
                )),
        )
        .unwrap()
    }

    /// A long-lived process parked in `cwd`, standing in for an agent.
    fn park(cwd: &Path) -> Child {
        std::fs::create_dir_all(cwd).unwrap();
        Command::new("sleep")
            .arg("120")
            .current_dir(cwd)
            .spawn()
            .unwrap()
    }

    fn scratch(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("ledecky-sweep-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn card_with_pid(db: &Db, pid: Option<i64>, worktree: &Path) -> i64 {
        let conn = db.lock();
        let project = Project::upsert(&conn, Path::new("/srv/repo")).unwrap();
        let card = Card::create(
            &conn,
            NewCard {
                project_id: project,
                task: "work",
                base_branch: "main",
                permission_mode: "acceptEdits",
                model: None,
            },
        )
        .unwrap();
        Card::attach_worktree(&conn, card, &worktree.to_string_lossy(), pid);
        card
    }

    #[test]
    fn the_sweep_kills_an_agent_still_sitting_in_its_worktree() {
        let data_dir = scratch("kills");
        let settings = sweep_settings(&data_dir);
        let worktree = settings.worktree_path(1);

        let mut child = park(&worktree);
        let pid = i64::from(child.id());

        let db = memory_db();
        let card = card_with_pid(&db, Some(pid), &worktree);

        sweep_orphans(&db, &settings);

        // The process is gone and the card no longer claims one.
        assert!(child.wait().is_ok());
        assert!(!owns(pid, &settings.worktrees_dir()));

        let conn = db.lock();
        let swept = Card::find(&conn, card).unwrap();
        assert_eq!(swept.agent_pid, None);
        assert_eq!(swept.agent_state, AgentState::Stopped);
    }

    #[test]
    fn the_sweep_spares_a_process_that_is_not_ours() {
        let data_dir = scratch("spares");
        let settings = sweep_settings(&data_dir);

        // Parked outside the worktrees tree: a recycled pid, not our agent.
        let elsewhere = data_dir.join("not-a-worktree");
        let mut child = park(&elsewhere);
        let pid = i64::from(child.id());

        let db = memory_db();
        let card = card_with_pid(&db, Some(pid), &elsewhere);

        sweep_orphans(&db, &settings);

        assert!(owns(pid, &elsewhere), "an unrelated process was killed");
        // The stale record is still cleared, so it is not reconsidered.
        assert_eq!(Card::find(&db.lock(), card).unwrap().agent_pid, None);

        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn the_sweep_ignores_cards_with_nothing_recorded() {
        let settings = sweep_settings(&scratch("empty"));
        let db = memory_db();
        let card = card_with_pid(&db, None, Path::new("/srv/worktrees/1"));

        sweep_orphans(&db, &settings);

        assert_eq!(Card::find(&db.lock(), card).unwrap().agent_pid, None);
    }
}
