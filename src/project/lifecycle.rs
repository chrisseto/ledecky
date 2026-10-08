//! A card's arc: giving it a workspace and an agent, and retiring it once its
//! work has landed.
//!
//! These are the operations that cross subsystems — git, the diff cache, the
//! worktree watcher and the agent manager all at once — which is why they sit
//! here rather than in the manager, which holds none of the first three.
//! `teardown`'s other caller deletes a card and involves no agent at all.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::agent::agent::Paste;
use crate::agent::{Agent, AgentManager};
use crate::config::Settings;
use crate::events::Kind;
use crate::project::{AgentState, Card, Lane, Project};
use crate::review::DiffCache;
use crate::vcs::git;
use crate::vcs::{self, Worktree};
use crate::watch::Worktrees;

/// Creates the card's worktree if needed and starts its agent.
pub async fn start(
    manager: &Arc<AgentManager>,
    settings: &Settings,
    worktrees: &Worktrees,
    card_id: i64,
) -> Result<Arc<Agent>> {
    Ok(start_with(manager, settings, worktrees, card_id, None)
        .await?
        .agent)
}

/// A started agent, and whether the `prompt` it was given went in with it.
pub struct Started {
    pub agent: Arc<Agent>,
    /// False whenever the caller's `prompt` went nowhere: one was already
    /// running, or a failed `--resume` sent us back with the card's own task
    /// instead. A caller with a message to deliver has to type it in itself.
    pub carried_prompt: bool,
}

/// The same, with `prompt` as the session's opening task.
///
/// NB: a given `prompt` wins over the card's own, which only ever goes in on a
/// session's first start — and a card still waiting for that has no turns, so
/// nothing to review and nothing to merge.
pub async fn start_with(
    manager: &Arc<AgentManager>,
    settings: &Settings,
    worktrees: &Worktrees,
    card_id: i64,
    prompt: Option<&str>,
) -> Result<Started> {
    let db = manager.db();
    let changes = manager.changes();
    let timings = settings.timings();

    let card = Card::find(db, card_id).await.context("no such card")?;
    let project = Project::find(db, card.project_id)
        .await
        .context("no such project")?;

    if let Some(existing) = manager.running(card_id) {
        return Ok(Started {
            agent: existing,
            carried_prompt: false,
        });
    }

    let repo = project.repo();
    let worktree = settings.worktree_path(card_id);

    if !vcs::exists(card.vcs, &worktree) {
        vcs::create(
            settings,
            card.vcs,
            &repo,
            &worktree,
            &card.base_branch,
            card_id,
        )
        .await
        .context("creating the workspace")?;
    }

    // Said before the spawn: a client that learned of the move
    // over the bus would otherwise keep showing the pre-start state — and the
    // drawer "no agent running" — until the opening prompt lands.
    manager.starting(card_id, &worktree.to_string_lossy()).await;

    let card = Card::find(db, card_id).await.context("card vanished")?;

    let mut agent = manager
        .spawn(&card, &worktree, &repo, prompt)
        .context("spawning the agent")?;

    let mut carried_prompt = prompt.is_some();

    // A recorded session can stop being resumable — a transcript that was never
    // written, or one since pruned. `--resume` then exits before starting,
    // which would leave the card unable to start at all, so forget the session
    // and come back without it.
    if card.session_id.is_some() && manager.resume_failed(&agent, timings.startup_timeout).await {
        warn!("card {card_id}: the recorded session is gone; starting a fresh one");
        Card::clear_session_id(db, card_id).await;

        let card = Card::find(db, card_id).await.context("card vanished")?;
        // NB: without `prompt`. This is a conversation that knows nothing of the
        // work a review or a merge request is about, and its own task is what it
        // needs — the card's task would otherwise be lost for good, since the
        // fresh session records an id straight away and `opening_prompt` never
        // offers it again. The caller is told the message did not go.
        agent = manager
            .spawn(&card, &worktree, &repo, None)
            .context("spawning the agent")?;
        carried_prompt = false;
    }

    Card::set_agent_pid(db, card_id, agent.pid).await;

    // Again, because whether the drawer shows a terminal turns on the agent
    // being up rather than on anything the card records.
    changes.card(db, card_id, Kind::State).await;

    // After the spawn, deliberately: establishing a watch walks the worktree,
    // and this is still the request. A worktree this new holds only what the
    // checkout put there, so the walk is short — but nothing should stand
    // between the agent starting and the card being able to say so.
    worktrees
        .ensure(
            card_id,
            card.project_id,
            &Worktree::new(card.vcs, &repo, &worktree),
        )
        .await;

    let watching = Arc::clone(manager);
    let started = agent.clone();
    rocket::tokio::spawn(watching.watch_startup(started, card_id, timings));

    Ok(Started {
        agent,
        carried_prompt,
    })
}

/// What became of a message handed to a card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// Typed at the session that was already running, and acknowledged by it.
    Pasted,
    /// Handed to a session started to take it.
    Opened,
    /// A dialog holds the terminal, so there was nowhere to put it.
    Blocked,
    /// The terminal's input box already held something, which is not ours.
    Occupied,
    /// Written at the terminal, but the session never said it took it.
    Unconfirmed,
    /// The terminal submitted something that was not this: a draft the emptiness
    /// check could not see went to the agent in its place.
    Displaced,
    /// Delivered, but a draft the emptiness check could not see went with it.
    Merged,
    /// Nothing was running, and nothing could be started.
    Failed,
}

impl Delivery {
    /// Whether the message reached the agent. `Merged` did: something of the
    /// user's went with it, which is worth saying but is not a failure to send.
    pub fn landed(self) -> bool {
        matches!(self, Delivery::Pasted | Delivery::Opened | Delivery::Merged)
    }
}

impl From<Paste> for Delivery {
    fn from(paste: Paste) -> Self {
        match paste {
            Paste::Submitted => Delivery::Pasted,
            Paste::NoBox => Delivery::Blocked,
            Paste::Occupied => Delivery::Occupied,
            Paste::Unconfirmed => Delivery::Unconfirmed,
            Paste::Displaced => Delivery::Displaced,
            Paste::Merged => Delivery::Merged,
        }
    }
}

/// Gets `message` to the card's agent, starting one for it if none is running.
///
/// A running session takes it as a paste. A stopped one takes it the way an
/// opening task arrives: on the command line of the session started to deliver
/// it, so the client holds it behind the startup dialogs and submits it itself.
/// That road has no screen to watch and nothing to wait for here, which is why
/// a cold start is not the slow case it would be if the message had to be
/// pasted once the client finished drawing itself.
pub async fn deliver(
    manager: &Arc<AgentManager>,
    settings: &Settings,
    worktrees: &Worktrees,
    card_id: i64,
    message: &str,
) -> Delivery {
    if let Some(agent) = manager.running(card_id) {
        // Hook-derived, and asked before the screen is: `Notification` says a
        // dialog was put up, and only a redraw ever says it has gone — so a card
        // still recorded as waiting is one to leave alone, whatever the screen
        // has got round to painting.
        let waiting = Card::find(manager.db(), card_id)
            .await
            .is_some_and(|card| card.agent_state == AgentState::AwaitingUser);
        if waiting {
            return Delivery::Blocked;
        }

        return agent.paste(message).await.into();
    }

    match start_with(manager, settings, worktrees, card_id, Some(message)).await {
        Ok(started) if started.carried_prompt => Delivery::Opened,
        // It is up, but took something other than this message: one was already
        // running by the time we looked again, or a failed `--resume` handed the
        // fresh session the card's own task. Either way it still has to be typed.
        Ok(started) => started.agent.paste(message).await.into(),
        Err(err) => {
            error!("card {card_id}: starting an agent to deliver a message: {err:#}");
            Delivery::Failed
        }
    }
}

/// Kills the agent and removes the worktree. Turn refs are kept.
pub async fn teardown(
    manager: &AgentManager,
    settings: &Settings,
    cache: &DiffCache,
    worktrees: &Worktrees,
    card_id: i64,
) {
    let db = manager.db();
    let changes = manager.changes();

    manager.stop(card_id).await;
    // Before the directory goes, so the watch does not fire on its removal.
    worktrees.forget(card_id);

    let card = Card::find(db, card_id).await;
    let project = match &card {
        Some(card) => Project::find(db, card.project_id).await,
        None => None,
    };

    let (Some(card), Some(project)) = (card, project) else {
        return;
    };

    let worktree = card
        .worktree_path
        .map(PathBuf::from)
        .unwrap_or_else(|| settings.worktree_path(card_id));
    vcs::remove(settings, card.vcs, &project.repo(), &worktree, card_id).await;
    // Turn refs are history and are kept; this one only ever described the
    // worktree that has just gone, and would otherwise pin its tree forever.
    let _ = git::run(
        &project.repo(),
        &["update-ref", "-d", &settings.working_ref(card_id)],
    )
    .await;

    // The card's refs go with it, so anything parsed from them is dead weight.
    cache.forget(&project.repo());
    cache.forget_head(card_id);

    Card::detach_worktree(db, card_id).await;
    changes.card(db, card_id, Kind::Board).await;
}

impl Card {
    /// Reclaims everything this card still holds on disk, and parks it in
    /// [`Lane::GarbageCollected`].
    ///
    /// The row and its turns, comments and events stay: they are the record of
    /// what happened. What goes is the worktree, the scratch indexes, and every
    /// ref the card owns — unlike [`teardown`], the turn refs go too, since
    /// nothing will read them again and they pin a tree apiece forever.
    pub async fn collect_garbage(
        &self,
        manager: &AgentManager,
        settings: &Settings,
        cache: &DiffCache,
        worktrees: &Worktrees,
    ) {
        teardown(manager, settings, cache, worktrees, self.id).await;

        let db = manager.db();
        if let Some(project) = Project::find(db, self.project_id).await {
            git::purge_refs(&project.repo(), &settings.card_refs(self.id)).await;
        }
        let _ = std::fs::remove_dir_all(settings.card_dir(self.id));

        Card::set_lane(db, self.id, Lane::GarbageCollected).await;
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
pub async fn check_merge(
    manager: &AgentManager,
    settings: &Settings,
    cache: &DiffCache,
    worktrees: &Worktrees,
    card_id: i64,
) {
    let db = manager.db();
    let Some(card) = Card::find(db, card_id).await.filter(|c| c.merge_requested) else {
        return;
    };
    let Some(project) = Project::find(db, card.project_id).await else {
        return;
    };
    let before = Card::merge_base_sha(db, card_id).await;
    let latest = crate::review::Turn::latest(db, card_id).await;

    let repo = project.repo();
    let Ok(after) = git::run(&repo, &["rev-parse", &card.base_branch]).await else {
        return;
    };

    let tree_of = async |rev: &str| {
        git::run(&repo, &["rev-parse", &format!("{rev}^{{tree}}")])
            .await
            .ok()
    };
    let base_tree = tree_of(&after).await;
    let turn_tree = match latest.as_ref() {
        Some(turn) => tree_of(&turn.commit_sha).await,
        None => None,
    };

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
    Card::set_lane(db, card_id, Lane::Done).await;
    Card::clear_merge_request(db, card_id).await;
    teardown(manager, settings, cache, worktrees, card_id).await;
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
