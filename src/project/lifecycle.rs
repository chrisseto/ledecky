//! A card's arc: giving it a workspace and an agent, and retiring it once its
//! work has landed.
//!
//! These are the operations that cross subsystems — git, the diff cache, the
//! worktree watcher and the agent manager all at once — which is why they live
//! above the manager rather than in it. `teardown`'s other caller deletes a
//! card and involves no agent at all.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::agent::{Agent, AgentManager};
use crate::config::Settings;
use crate::events::Kind;
use crate::git;
use crate::project::{Card, Lane, Project};
use crate::review::DiffCache;
use crate::watch::Worktrees;

/// Creates the card's worktree if needed and starts its agent.
pub fn start(
    manager: &Arc<AgentManager>,
    settings: &Settings,
    worktrees: &Worktrees,
    card_id: i64,
) -> Result<Arc<Agent>> {
    let db = manager.db();
    let changes = manager.changes();
    let timings = settings.timings();

    let conn = db.lock();
    let card = Card::find(&conn, card_id).context("no such card")?;
    let project = Project::find(&conn, card.project_id).context("no such project")?;
    drop(conn);

    if let Some(existing) = manager.running(card_id) {
        return Ok(existing);
    }

    let repo = project.repo();
    let worktree = settings.worktree_path(card_id);

    if !worktree.join(".git").exists() {
        git::create_worktree(settings, &repo, &worktree, &card.base_branch, card_id)
            .context("creating the worktree")?;
    }

    Card::attach_worktree(&db.lock(), card_id, &worktree.to_string_lossy(), None);
    // Said before the spawn, which blocks: a client that learned of the move
    // over the bus would otherwise keep showing the pre-start state — and the
    // drawer "no agent running" — until the opening prompt lands.
    manager.starting(card_id);

    let card = {
        let conn = db.lock();
        Card::find(&conn, card_id).context("card vanished")?
    };

    let mut agent = manager
        .spawn(&card, &worktree, &repo)
        .context("spawning the agent")?;

    // A recorded session can stop being resumable — a transcript that was never
    // written, or one since pruned. `--resume` then exits before starting,
    // which would leave the card unable to start at all, so forget the session
    // and come back without it.
    if card.session_id.is_some() && manager.resume_failed(&agent, timings.startup_timeout) {
        warn!("card {card_id}: the recorded session is gone; starting a fresh one");
        Card::clear_session_id(&db.lock(), card_id);

        let card = Card::find(&db.lock(), card_id).context("card vanished")?;
        agent = manager
            .spawn(&card, &worktree, &repo)
            .context("spawning the agent")?;
    }

    Card::set_agent_pid(&db.lock(), card_id, agent.pid);

    // Again, because whether the drawer shows a terminal turns on the agent
    // being up rather than on anything the card records.
    changes.card(db, card_id, Kind::State);

    // After the spawn, deliberately: this runs on the request thread and
    // establishing a watch walks the worktree. A worktree this new holds only
    // what the checkout put there, so the walk is short — but nothing should
    // stand between the agent starting and the card being able to say so.
    worktrees.ensure(card_id, card.project_id, &worktree);

    let watching = Arc::clone(manager);
    let started = agent.clone();
    std::thread::spawn(move || watching.watch_startup(started, card_id, timings));

    Ok(agent)
}

/// Kills the agent and removes the worktree. Turn refs are kept.
pub fn teardown(
    manager: &AgentManager,
    settings: &Settings,
    cache: &DiffCache,
    worktrees: &Worktrees,
    card_id: i64,
) {
    let db = manager.db();
    let changes = manager.changes();

    manager.stop(card_id);
    // Before the directory goes, so the watch does not fire on its removal.
    worktrees.forget(card_id);

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
    changes.card(db, card_id, Kind::Board);
}

impl Card {
    /// Reclaims everything this card still holds on disk, and parks it in
    /// [`Lane::GarbageCollected`].
    ///
    /// The row and its turns, comments and events stay: they are the record of
    /// what happened. What goes is the worktree, the scratch indexes, and every
    /// ref the card owns — unlike [`teardown`], the turn refs go too, since
    /// nothing will read them again and they pin a tree apiece forever.
    pub fn collect_garbage(
        &self,
        manager: &AgentManager,
        settings: &Settings,
        cache: &DiffCache,
        worktrees: &Worktrees,
    ) {
        teardown(manager, settings, cache, worktrees, self.id);

        let db = manager.db();
        let project = Project::find(&db.lock(), self.project_id);
        if let Some(project) = project {
            git::purge_refs(&project.repo(), &settings.card_refs(self.id));
        }
        let _ = std::fs::remove_dir_all(settings.card_dir(self.id));

        Card::set_lane(&db.lock(), self.id, Lane::GarbageCollected);
    }
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
pub fn check_merge(
    manager: &AgentManager,
    settings: &Settings,
    cache: &DiffCache,
    worktrees: &Worktrees,
    card_id: i64,
) {
    let db = manager.db();
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
    teardown(manager, settings, cache, worktrees, card_id);
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
