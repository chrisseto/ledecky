//! The registry of running agents, and every transition of a card's agent
//! state.
//!
//! One writer. Before this, `AgentState` was written from three modules and
//! nothing reconciled those writes against whether the process was actually
//! alive — a card could sit at "needs you" pointing at a terminal with a corpse
//! behind it, because the only code that saw the death had no way to reach the
//! registry.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock, Weak};
use std::time::Duration;

use anyhow::{Context, Result};
use portable_pty::CommandBuilder;
use rocket::fairing::{self, Fairing, Info};
use rocket::tokio::sync::mpsc;
use rocket::tokio::task::spawn_blocking;
use rocket::{Orbit, Rocket};

use crate::agent::agent::{self, Watcher};
use crate::agent::{messaging, Agent};
use crate::config::{Settings, Timings};
use crate::db::Db;
use crate::events::{Changes, Kind};
use crate::git;
use crate::hooks::HookAuth;
use crate::project::{AgentState, Card, Lane, Project};

/// How often a starting session is re-checked while it proves itself.
const STARTUP_POLL: Duration = Duration::from_millis(100);

pub struct AgentManager {
    agents: RwLock<HashMap<i64, Arc<Agent>>>,
    db: Db,
    changes: Changes,
    /// Held here so the manager can configure the agents it starts. Nothing
    /// above needs to carry it: `settings_json` is only ever wanted at the
    /// moment of a spawn, which happens in here.
    auth: HookAuth,
    settings: Settings,
    /// Where the pump's reports go. See [`Report`].
    reports: mpsc::UnboundedSender<Report>,
}

/// What the pump has to tell the manager.
///
/// NB: a channel rather than the call itself. The pump runs on a thread of
/// `portable-pty`'s making, so it cannot await the writes these turn into — and
/// blocking on the runtime from there panics outright once the runtime is
/// shutting down, which is exactly when a pty closes and the pump reports. A
/// send costs the pump nothing and cannot fail loudly: once the manager is
/// gone, so is anything a report would have written.
enum Report {
    /// The dialog that was holding the keyboard has left the screen.
    DialogCleared(i64),
    /// The pty closed: the agent is gone, however it went.
    Exited(i64),
}

impl AgentManager {
    /// NB: `Arc` from the start, because `spawn` hands the pump a `Weak` to
    /// this and there is nowhere else for that to come from.
    pub fn new(db: Db, changes: Changes, auth: HookAuth, settings: Settings) -> Arc<Self> {
        let (reports, mut inbox) = mpsc::unbounded_channel();
        let manager = Arc::new(Self {
            agents: RwLock::new(HashMap::new()),
            db,
            changes,
            auth,
            settings,
            reports,
        });

        // NB: a `Weak`, so this task is not what keeps the manager alive. The
        // manager owns the sender, so dropping it closes the channel and ends
        // this — which is also what stops a report outliving the runtime.
        let applying = Arc::downgrade(&manager);
        rocket::tokio::spawn(async move {
            while let Some(report) = inbox.recv().await {
                let Some(manager) = applying.upgrade() else {
                    return;
                };
                match report {
                    Report::DialogCleared(card_id) => manager.resumed(card_id).await,
                    Report::Exited(card_id) => manager.stopped(card_id).await,
                }
            }
        });

        manager
    }

    /// Whether a hook callback carries this server's token.
    pub fn verify(&self, token: &str) -> bool {
        self.auth.matches(token)
    }

    pub fn get(&self, card_id: i64) -> Option<Arc<Agent>> {
        self.agents.read().unwrap().get(&card_id).cloned()
    }

    /// The agent for a card, if one is running.
    ///
    /// NB: the check is inherently a moment ago — the child can exit an
    /// instruction later — but having it in one place beats four call sites each
    /// remembering, which is what let a dead agent take keystrokes.
    pub fn running(&self, card_id: i64) -> Option<Arc<Agent>> {
        self.get(card_id).filter(|agent| agent.is_running())
    }

    /// Spawns `claude` in `worktree` and registers it under `card.id`.
    ///
    /// `repo` is the project's main checkout, added as a second allowed directory
    /// so the agent can land its work on the base branch — which lives there, not
    /// in the worktree.
    pub fn spawn(
        self: &Arc<Self>,
        card: &Card,
        worktree: &Path,
        repo: &Path,
    ) -> Result<Arc<Agent>> {
        if let Some(existing) = self.get(card.id) {
            if existing.is_running() {
                return Ok(existing);
            }
            // NB: killed, not just dropped. There is no `Drop` for `Agent`, so
            // an evicted one is never reaped and lingers as a zombie for the
            // life of the server.
            self.evict(card.id);
        }

        let cmd = self.command(card, worktree, repo);
        let (agent, reader) = Agent::attach(cmd, self.settings.timings())?;

        self.agents.write().unwrap().insert(card.id, agent.clone());

        // portable-pty hands back a blocking reader, so it gets its own thread.
        // It reports back through a `Weak`, so it cannot keep the manager — and
        // through it this agent — alive.
        let watcher: Weak<dyn Watcher> = Arc::downgrade(self) as Weak<dyn Watcher>;
        let pumped = agent.clone();
        let card_id = card.id;
        std::thread::spawn(move || agent::pump(reader, pumped, watcher, card_id));

        Ok(agent)
    }

    /// The command line for a card's agent.
    fn command(&self, card: &Card, worktree: &Path, repo: &Path) -> CommandBuilder {
        let mut cmd = CommandBuilder::new(&self.settings.agent_bin);
        cmd.arg("--permission-mode");
        cmd.arg(&card.permission_mode);
        // Restarting a card picks the conversation back up rather than starting
        // over with no memory of the work already done.
        if let Some(session_id) = &card.session_id {
            cmd.arg("--resume");
            cmd.arg(session_id);
        }
        if let Some(model) = &card.model {
            cmd.arg("--model");
            cmd.arg(model);
        }
        cmd.arg("--settings");
        cmd.arg(self.auth.settings_json(card.id));
        cmd.arg("--add-dir");
        cmd.arg(repo);
        // NB: no `--name`. Naming the session suppresses the name it would
        // give itself, which is the one the card takes.

        // The opening task, handed over on the command line rather than typed at
        // the terminal. The client holds it behind the workspace-trust and
        // `bypassPermissions` dialogs and submits it once they are answered, so
        // nothing here has to watch a screen to find out whether it landed.
        //
        // NB: `--` first. A task is the user's prose and may well start with a
        // dash, which would otherwise be read as a flag.
        if let Some(prompt) = card.opening_prompt() {
            cmd.arg("--");
            cmd.arg(prompt);
        }

        cmd.cwd(worktree);
        // Match what xterm.js renders; the inherited TERM may be anything.
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        // Applied last so a deployment can override the above if it must.
        for (key, value) in &self.settings.agent_env {
            cmd.env(key, value);
        }
        // If this server was itself launched from a Claude Code session, these
        // markers make the child think it is a nested agent and disable session
        // persistence. Auth vars are deliberately left alone.
        for marker in [
            "CLAUDECODE",
            "CLAUDE_CODE_CHILD_SESSION",
            "CLAUDE_CODE_ENTRYPOINT",
            "CLAUDE_CODE_SSE_PORT",
            "CLAUDE_SESSION_ID",
        ] {
            cmd.env_remove(marker);
        }

        cmd
    }

    /// Takes an agent out of the registry and reaps it.
    fn evict(&self, card_id: i64) -> Option<Arc<Agent>> {
        let agent = self.agents.write().unwrap().remove(&card_id);
        if let Some(agent) = &agent {
            agent.kill();
        }
        agent
    }

    // ---- transitions ----
    //
    // One method per event that can move a card's agent state, named for the
    // cause rather than the field it writes. Each takes the database lock once
    // for its whole body: `needs_user` writes both the state and the lane, and
    // announcing those separately would fetch the card twice.

    /// The card is starting an agent.
    pub async fn starting(&self, card_id: i64, worktree: &str) {
        // NB: one write, so a reader never finds a card holding a worktree with
        // nothing running in it — which is what the board draws a stopped card
        // with a diff from.
        let _ = Card::starting(&self.db, card_id, worktree).await;
        self.announce(card_id).await;
    }

    /// Writes a state and moves the lane if that state calls for it, under one
    /// lock and one announcement.
    async fn settle(&self, card_id: i64, state: AgentState) {
        Card::set_agent_state(&self.db, card_id, state).await;

        let lane = Card::find(&self.db, card_id)
            .await
            .and_then(|card| lane_for(state, card.lane, card.merge_requested));
        if let Some(lane) = lane {
            Card::set_lane(&self.db, card_id, lane).await;
        }

        self.announce(card_id).await;
    }

    /// The user asked for the agent to stop.
    pub async fn stop(&self, card_id: i64) {
        self.evict(card_id);
        self.stopped(card_id).await;
    }

    /// A turn began — `UserPromptSubmit`.
    ///
    /// NB: driven by the prompt rather than by anything dialog-shaped, so a
    /// review sent to an idle card moves it too.
    pub async fn turn_started(&self, card_id: i64) {
        self.settle(card_id, AgentState::Running).await;
    }

    /// A turn ended — `Stop`. The work is there to look at, so the card goes to
    /// review.
    pub async fn turn_ended(&self, card_id: i64) {
        self.settle(card_id, AgentState::Idle).await;
    }

    /// A dialog is up and is the user's to answer.
    ///
    /// Parks the card in In Review as well as saying so on the chip: a chip is
    /// easy to miss at board scale, a lane is not.
    pub async fn needs_user(&self, card_id: i64) {
        self.settle(card_id, AgentState::AwaitingUser).await;
    }

    /// No hook has reached us, so our callbacks are not arriving — most likely
    /// an `allowedHttpHookUrls` allowlist that does not name us.
    pub async fn hooks_silent(&self, card_id: i64) {
        self.write(card_id, AgentState::Misconfigured).await;
    }

    /// The agent could not be started at all.
    pub async fn failed(&self, card_id: i64) {
        self.write(card_id, AgentState::Error).await;
    }

    /// Claude Code reported its own session ending.
    pub async fn session_ended(&self, card_id: i64) {
        self.write(card_id, AgentState::Stopped).await;
    }

    async fn write(&self, card_id: i64, state: AgentState) {
        Card::set_agent_state(&self.db, card_id, state).await;
        self.announce(card_id).await;
    }

    /// Work has resumed, for a card that was waiting on a dialog.
    ///
    /// NB: only overwrites the state we set ourselves. A turn can end — and its
    /// `Stop` hook record `idle` — before the redraw that proves the dialog is
    /// gone, and writing `running` over that would leave the card claiming to
    /// work.
    async fn resumed(&self, card_id: i64) {
        let awaiting = Card::find(&self.db, card_id)
            .await
            .is_some_and(|card| card.agent_state == AgentState::AwaitingUser);

        if awaiting {
            self.turn_started(card_id).await;
        }
    }

    /// The agent is gone: no pid, and stopped however it went.
    async fn stopped(&self, card_id: i64) {
        Card::set_agent_pid(&self.db, card_id, None).await;
        self.write(card_id, AgentState::Stopped).await;
    }

    async fn announce(&self, card_id: i64) {
        self.changes.card(&self.db, card_id, Kind::State).await;
    }

    /// Watches a session through its first moments.
    ///
    /// The task itself went in on the command line, so there is nothing to deliver.
    /// What is left is the one thing no hook reports: a dialog holding the keyboard.
    /// Neither the workspace-trust prompt nor the `bypassPermissions` consent fires
    /// anything, and the client holds the task behind them — so a session that never
    /// signals is one the user has to answer, and the card says so rather than
    /// sitting in `starting` with nobody told.
    pub(crate) async fn watch_startup(
        self: Arc<Self>,
        agent: Arc<Agent>,
        card_id: i64,
        timings: Timings,
    ) {
        // NB: watched for as long as the misconfigured check would have waited
        // anyway. The trust prompt can sit there for as long as it takes somebody to
        // notice it, and a shorter look just races the client's first paint.
        if settled(&agent, timings.hook_grace).await == Startup::Blocked {
            agent.saw_dialog();
            self.needs_user(card_id).await;
            // Deliberately no misconfigured check behind this. A session
            // holding a dialog has not run a turn, so of course no hook has
            // fired — saying the hooks are broken would be the second wrong
            // thing to tell someone whose agent is simply waiting on them.
            return;
        }

        // Silence past this point means our hook URLs are not reaching us —
        // most likely an `allowedHttpHookUrls` allowlist. Without hooks there
        // are no turn snapshots and no lane transitions, so say so on the card.
        rocket::tokio::time::sleep(timings.hook_grace).await;
        if agent.is_running() && self.hook_events(card_id).await == 0 {
            let grace = timings.hook_grace;
            warn!("card {card_id}: no hooks received within {grace:?}");
            self.hooks_silent(card_id).await;
        }
    }

    async fn hook_events(&self, card_id: i64) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM events WHERE card_id = ?1")
            .bind(card_id)
            .fetch_one(self.db.pool())
            .await
            .unwrap_or(0)
    }

    /// Asks the agent to land its work on the base branch.
    ///
    /// The server never rewrites the user's branches itself — conflicts are exactly
    /// the situation an agent is good at, and a failed rebase run by the server
    /// would just leave a mess for someone else to unpick.
    pub async fn request_merge(&self, card_id: i64) -> Result<()> {
        let card = Card::find(&self.db, card_id)
            .await
            .context("no such card")?;
        let project = Project::find(&self.db, card.project_id)
            .await
            .context("no such project")?;

        let inbox = self
            .running(card_id)
            .context("the agent is not running")?
            .inbox()
            .context("the session has not reported an inbox socket")?;

        let repo = project.repo();
        let base_sha = git::run(&repo, &["rev-parse", &card.base_branch])
            .await
            .with_context(|| format!("resolving {}", card.base_branch))?;

        // Recorded before the message goes out: the agent can land the merge and
        // fire its `Stop` hook while we are still here, and a hook that arrives
        // without this reads the turn as ordinary work.
        Card::request_merge(&self.db, card_id, &base_sha).await;

        // NB: on the blocking pool. The inbox is a unix socket written under a
        // timeout, which blocks the thread it is on however short it is.
        let prompt = merge_prompt(&card.base_branch, &repo);
        let sent = spawn_blocking(move || messaging::send(&inbox, &prompt))
            .await
            .expect("asking for a merge panicked");
        if let Err(err) = sent {
            Card::clear_merge_request(&self.db, card_id).await;
            return Err(err).context("asking the agent to merge");
        }

        self.changes.card(&self.db, card_id, Kind::Board).await;
        Ok(())
    }

    /// Kills agents left behind by a server that did not shut down cleanly.
    ///
    /// Closing the pty master hangs up the session, so an agent normally dies with
    /// us even under `SIGKILL`. This covers what that misses: a child that ignores
    /// `SIGHUP`, or one that had already broken away from the terminal.
    pub async fn sweep_orphans(&self) {
        for card in Card::with_agent_pid(&self.db).await {
            let Some(pid) = card.agent_pid else { continue };

            if owns(pid, &self.settings.worktrees_dir()) {
                warn!("card {}: killing orphaned agent {pid}", card.id);
                kill_pid(pid);
            }
            Card::set_agent_pid(&self.db, card.id, None).await;
            Card::set_agent_state(&self.db, card.id, AgentState::Stopped).await;
        }
    }

    /// Whether a `--resume` exited before it started, which is how the client
    /// reports a conversation it could not find.
    pub async fn resume_failed(&self, agent: &Agent, within: Duration) -> bool {
        settled(agent, within).await == Startup::Gone
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn changes(&self) -> &Changes {
        &self.changes
    }
}

/// What became of a session between spawning it and it reporting for duty.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Startup {
    /// The `SessionStart` hook ran: the client is up and past any dialog.
    Ready,
    /// The process exited first. A `--resume` that found nothing does this.
    Gone,
    /// A dialog is on screen, which is the user's to answer.
    Blocked,
    /// Still running, and neither up nor visibly waiting on anything.
    Stalled,
}

/// Waits for the session to say it is up, to die, or to put a dialog up.
///
/// Readiness is the `SessionStart` hook reporting its inbox socket. That event
/// fires only once the client is past the workspace-trust and
/// `bypassPermissions` dialogs, so it separates a session that is ready from one
/// waiting on somebody far better than watching the screen for an input box did
/// — and the same wait answers whether a `--resume` took, since a resume that
/// finds nothing exits without ever firing it.
///
/// NB: the screen is polled rather than read once at the end. The client takes
/// seconds to paint, and where it lands in that window is not something to race:
/// checking a single time picked up a blank screen and called it stalled.
async fn settled(agent: &Agent, within: Duration) -> Startup {
    let checks = within.as_millis() / STARTUP_POLL.as_millis().max(1);
    for _ in 0..checks {
        if agent.inbox().is_some() {
            return Startup::Ready;
        }
        if !agent.is_running() {
            return Startup::Gone;
        }
        if agent.is_blocked() {
            return Startup::Blocked;
        }
        rocket::tokio::time::sleep(STARTUP_POLL).await;
    }
    Startup::Stalled
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

/// Two things the manager needs from the server's own lifetime, rather than
/// from a caller: the port hook callbacks should name, and a chance to take its
/// agents down with it.
///
/// NB: a fairing on the manager rather than a pair of `AdHoc`s in `main`. Those
/// had to fish the manager back out of managed state by type, which is a lookup
/// that compiles whether or not anything was ever managed — attaching the
/// manager itself cannot miss.
#[rocket::async_trait]
impl Fairing for AgentManager {
    fn info(&self) -> Info {
        Info {
            name: "agents",
            kind: fairing::Kind::Liftoff | fairing::Kind::Shutdown,
        }
    }

    /// The bound port is only knowable here: `port = 0` asks for a free one, and
    /// hook URLs have to name the one agents can actually reach.
    async fn on_liftoff(&self, rocket: &Rocket<Orbit>) {
        self.auth.bind(rocket.config().port);
    }

    /// Live agents are children of this process; leaving them behind would
    /// strand worktrees with nothing driving them.
    async fn on_shutdown(&self, _: &Rocket<Orbit>) {
        for agent in self.agents.write().unwrap().drain().map(|(_, a)| a) {
            agent.kill();
        }
    }
}

/// The pump's two reports. Neither has another caller, so the bodies live here
/// rather than being inherent methods with a trait forwarding to them.
impl Watcher for AgentManager {
    fn dialog_cleared(&self, card_id: i64) {
        let _ = self.reports.send(Report::DialogCleared(card_id));
    }

    /// NB: the only path that reports a crash, and the only one that clears a
    /// stale `AwaitingUser`. Without it a card whose agent dies while a dialog
    /// is up says "needs you" forever, because the redraw that would have
    /// cleared it can never come.
    fn exited(&self, card_id: i64) {
        // Inline, unlike the write behind it: this reaps the child, and a
        // report waiting its turn is a zombie waiting with it.
        self.evict(card_id);
        let _ = self.reports.send(Report::Exited(card_id));
    }
}

/// Which lane a card belongs in once its agent reaches `state`, if it should
/// move at all.
///
/// NB: deliberately a function rather than something the manager does inline.
/// Lane is board policy, not agent state — a card is also dragged between lanes
/// by hand, and dragging into In Progress is what *starts* an agent — so the
/// one direction that is derived stays visible and testable on its own.
fn lane_for(state: AgentState, lane: Lane, merge_requested: bool) -> Option<Lane> {
    match state {
        // Waiting on somebody, or finished a turn: either way there is something
        // to look at, and a chip is easy to miss at board scale where a lane is
        // not.
        AgentState::AwaitingUser | AgentState::Idle if lane == Lane::InProgress => {
            Some(Lane::InReview)
        }
        // NB: a merge is asked for and finished in review, the only lane
        // offering the button. The merge prompt fires `UserPromptSubmit` like
        // any other, and moving the card would take that button away mid-merge.
        AgentState::Running if lane == Lane::InReview && !merge_requested => Some(Lane::InProgress),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tests::memory_db;
    use crate::project::{NewCard, Project};
    use std::path::{Path, PathBuf};
    use std::process::Child;

    // ---- the lane policy ----

    #[test]
    fn a_dialog_parks_a_working_card_in_review() {
        assert_eq!(
            lane_for(AgentState::AwaitingUser, Lane::InProgress, false),
            Some(Lane::InReview)
        );
    }

    #[test]
    fn a_card_already_in_review_stays_there_when_a_dialog_opens() {
        assert_eq!(
            lane_for(AgentState::AwaitingUser, Lane::InReview, false),
            None
        );
    }

    #[test]
    fn work_resuming_brings_a_parked_card_back() {
        assert_eq!(
            lane_for(AgentState::Running, Lane::InReview, false),
            Some(Lane::InProgress)
        );
    }

    /// The merge button only exists in review, and the merge prompt fires
    /// `UserPromptSubmit` like any other — so moving the card would take the
    /// button away halfway through.
    #[test]
    fn a_card_mid_merge_keeps_its_lane() {
        assert_eq!(lane_for(AgentState::Running, Lane::InReview, true), None);
    }

    #[test]
    fn a_finished_turn_puts_the_work_up_for_review() {
        assert_eq!(
            lane_for(AgentState::Idle, Lane::InProgress, false),
            Some(Lane::InReview)
        );
    }

    #[test]
    fn states_that_say_nothing_about_a_lane_move_nothing() {
        for state in [
            AgentState::Stopped,
            AgentState::Misconfigured,
            AgentState::Error,
            AgentState::Starting,
        ] {
            assert_eq!(lane_for(state, Lane::InProgress, false), None);
            assert_eq!(lane_for(state, Lane::InReview, false), None);
        }
    }

    // ---- the transitions ----

    /// A card in `lane`, with its agent in `state`.
    async fn card_in(db: &Db, lane: Lane, state: AgentState) -> i64 {
        let project = Project::upsert(db, Path::new("/srv/repo")).await.unwrap();
        let card = Card::create(
            db,
            NewCard {
                project_id: project,
                task: "waiting",
                base_branch: "main",
                permission_mode: "acceptEdits",
                model: None,
            },
        )
        .await
        .unwrap();
        Card::set_lane(db, card, lane).await;
        Card::set_agent_state(db, card, state).await;
        card
    }

    /// A manager over a scratch database. The bus has no subscribers: these
    /// cover the transitions, not the announcing.
    fn manager(db: &Db) -> Arc<AgentManager> {
        AgentManager::new(
            db.clone(),
            Changes::default(),
            HookAuth::new(),
            Settings::default(),
        )
    }

    async fn look(db: &Db, card_id: i64) -> (Lane, AgentState) {
        let card = Card::find(db, card_id).await.unwrap();
        (card.lane, card.agent_state)
    }

    #[tokio::test]
    async fn a_card_waiting_on_a_dialog_steps_into_review_and_back() {
        let db = memory_db().await;
        let card = card_in(&db, Lane::InProgress, AgentState::Running).await;

        manager(&db).needs_user(card).await;
        assert_eq!(
            look(&db, card).await,
            (Lane::InReview, AgentState::AwaitingUser)
        );

        // What the pump's report turns into, which is the half worth asserting
        // on; the trait method itself only hands it over.
        manager(&db).resumed(card).await;
        assert_eq!(
            look(&db, card).await,
            (Lane::InProgress, AgentState::Running)
        );
    }

    #[tokio::test]
    async fn a_card_already_in_review_only_changes_state_on_the_way_in() {
        let db = memory_db().await;
        let card = card_in(&db, Lane::InReview, AgentState::Running).await;

        manager(&db).needs_user(card).await;
        assert_eq!(
            look(&db, card).await,
            (Lane::InReview, AgentState::AwaitingUser)
        );
    }

    #[tokio::test]
    async fn a_prompt_pulls_an_idle_card_out_of_review() {
        // Sending a review to an idle card is the case the dialog watcher never
        // sees: there is no dialog, only a `UserPromptSubmit`.
        let db = memory_db().await;
        let card = card_in(&db, Lane::InReview, AgentState::Idle).await;

        manager(&db).turn_started(card).await;
        assert_eq!(
            look(&db, card).await,
            (Lane::InProgress, AgentState::Running)
        );
    }

    #[tokio::test]
    async fn an_outstanding_merge_keeps_the_card_in_review() {
        // The merge prompt fires `UserPromptSubmit` like any other, and In Review
        // is the only lane offering the button.
        let db = memory_db().await;
        let card = card_in(&db, Lane::InReview, AgentState::Idle).await;
        Card::request_merge(&db, card, "abc123").await;

        manager(&db).turn_started(card).await;
        assert_eq!(look(&db, card).await, (Lane::InReview, AgentState::Running));
    }

    #[tokio::test]
    async fn a_dialog_redraw_does_not_overwrite_a_finished_turn() {
        // The `Stop` hook can land before the redraw that proves the dialog is
        // gone; `running` written over that `idle` would strand the card.
        let db = memory_db().await;
        let card = card_in(&db, Lane::InReview, AgentState::Idle).await;

        manager(&db).resumed(card).await;
        assert_eq!(look(&db, card).await, (Lane::InReview, AgentState::Idle));
    }

    // ---- the sweep ----------------------------------------------------------

    use rocket::figment::providers::Serialized;
    use std::process::Command;

    // ---- the merge prompt ----

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

    // ---- the startup sweep ----

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

    // ---- the lane pair ------------------------------------------------------

    /// A manager whose settings point at a scratch data directory.
    fn sweeper(db: &Db, settings: Settings) -> Arc<AgentManager> {
        AgentManager::new(db.clone(), Changes::default(), HookAuth::new(), settings)
    }

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

    async fn card_with_pid(db: &Db, pid: Option<i64>, worktree: &Path) -> i64 {
        let project = Project::upsert(db, Path::new("/srv/repo")).await.unwrap();
        let card = Card::create(
            db,
            NewCard {
                project_id: project,
                task: "work",
                base_branch: "main",
                permission_mode: "acceptEdits",
                model: None,
            },
        )
        .await
        .unwrap();
        Card::attach_worktree(db, card, &worktree.to_string_lossy(), pid).await;
        card
    }

    #[tokio::test]
    async fn the_sweep_kills_an_agent_still_sitting_in_its_worktree() {
        let data_dir = scratch("kills");
        let settings = sweep_settings(&data_dir);
        let worktree = settings.worktree_path(1);

        let mut child = park(&worktree);
        let pid = i64::from(child.id());

        let db = memory_db().await;
        let card = card_with_pid(&db, Some(pid), &worktree).await;

        sweeper(&db, settings.clone()).sweep_orphans().await;

        // The process is gone and the card no longer claims one.
        assert!(child.wait().is_ok());
        assert!(!owns(pid, &settings.worktrees_dir()));

        let swept = Card::find(&db, card).await.unwrap();
        assert_eq!(swept.agent_pid, None);
        assert_eq!(swept.agent_state, AgentState::Stopped);
    }

    #[tokio::test]
    async fn the_sweep_spares_a_process_that_is_not_ours() {
        let data_dir = scratch("spares");
        let settings = sweep_settings(&data_dir);

        // Parked outside the worktrees tree: a recycled pid, not our agent.
        let elsewhere = data_dir.join("not-a-worktree");
        let mut child = park(&elsewhere);
        let pid = i64::from(child.id());

        let db = memory_db().await;
        let card = card_with_pid(&db, Some(pid), &elsewhere).await;

        sweeper(&db, settings.clone()).sweep_orphans().await;

        assert!(owns(pid, &elsewhere), "an unrelated process was killed");
        // The stale record is still cleared, so it is not reconsidered.
        assert_eq!(Card::find(&db, card).await.unwrap().agent_pid, None);

        let _ = child.kill();
        let _ = child.wait();
    }

    #[tokio::test]
    async fn the_sweep_ignores_cards_with_nothing_recorded() {
        let settings = sweep_settings(&scratch("empty"));
        let db = memory_db().await;
        let card = card_with_pid(&db, None, Path::new("/srv/worktrees/1")).await;

        sweeper(&db, settings.clone()).sweep_orphans().await;

        assert_eq!(Card::find(&db, card).await.unwrap().agent_pid, None);
    }
}
