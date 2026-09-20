use rocket::serde::Serialize;
use sqlx::sqlite::SqliteRow;
use sqlx::{FromRow, Row};

use crate::db::{sql, Db};

/// The kanban column a card sits in.
///
/// Distinct from [`AgentState`]: a card can be in review with its agent still
/// running, or in progress with nothing running at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(crate = "rocket::serde", rename_all = "snake_case")]
pub enum Lane {
    Todo,
    InProgress,
    InReview,
    Done,
    GarbageCollected,
}

impl Lane {
    pub const ALL: &'static [Self] = &[
        Self::Todo,
        Self::InProgress,
        Self::InReview,
        Self::Done,
        Self::GarbageCollected,
    ];

    /// The lanes that are shown. `GarbageCollected` is deliberately absent: a
    /// collected card is a record, not something to look at or drag.
    pub const VISIBLE: &'static [Self] =
        &[Self::Todo, Self::InProgress, Self::InReview, Self::Done];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Todo => "todo",
            Self::InProgress => "in_progress",
            Self::InReview => "in_review",
            Self::Done => "done",
            Self::GarbageCollected => "garbage_collected",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Todo => "To Do",
            Self::InProgress => "In Progress",
            Self::InReview => "In Review",
            Self::Done => "Done",
            Self::GarbageCollected => "Garbage Collected",
        }
    }

    pub fn parse(raw: &str) -> Self {
        Self::ALL
            .iter()
            .copied()
            .find(|lane| lane.as_str() == raw)
            .unwrap_or(Self::Todo)
    }
}

/// What the card's `claude` process is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(crate = "rocket::serde", rename_all = "snake_case")]
pub enum AgentState {
    Stopped,
    Starting,
    Running,
    Idle,
    AwaitingUser,
    Misconfigured,
    Error,
}

impl AgentState {
    pub const ALL: &'static [Self] = &[
        Self::Stopped,
        Self::Starting,
        Self::Running,
        Self::Idle,
        Self::AwaitingUser,
        Self::Misconfigured,
        Self::Error,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Idle => "idle",
            Self::AwaitingUser => "awaiting_user",
            Self::Misconfigured => "misconfigured",
            Self::Error => "error",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Starting => "starting",
            Self::Running => "working",
            Self::Idle => "idle",
            Self::AwaitingUser => "needs you",
            Self::Misconfigured => "hooks not reaching server",
            Self::Error => "error",
        }
    }

    pub fn parse(raw: &str) -> Self {
        Self::ALL
            .iter()
            .copied()
            .find(|state| state.as_str() == raw)
            .unwrap_or(Self::Stopped)
    }
}

/// One unit of work: a task, its worktree, and the agent doing it.
#[derive(Debug, Clone, Serialize)]
#[serde(crate = "rocket::serde")]
pub struct Card {
    pub id: i64,
    pub project_id: i64,
    /// What the session called itself, once it has. The board falls back to
    /// the task.
    pub title: Option<String>,
    pub task: String,
    pub base_branch: String,
    pub lane: Lane,
    pub lane_label: &'static str,
    pub position: f64,
    pub permission_mode: String,
    pub model: Option<String>,
    pub worktree_path: Option<String>,
    pub session_id: Option<String>,
    pub agent_pid: Option<i64>,
    pub agent_state: AgentState,
    pub agent_state_label: &'static str,
    pub merge_requested: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// What a card is created with. Everything else is derived or set later.
pub struct NewCard<'a> {
    pub project_id: i64,
    pub task: &'a str,
    pub base_branch: &'a str,
    pub permission_mode: &'a str,
    pub model: Option<&'a str>,
}

/// What a card's form can still change, before an agent has seen any of it.
pub struct CardEdit<'a> {
    pub task: &'a str,
    pub base_branch: &'a str,
    pub permission_mode: &'a str,
    pub model: Option<&'a str>,
}

impl Card {
    const COLUMNS: &'static str = "id, project_id, title, task, base_branch, lane, position, \
         permission_mode, model, worktree_path, session_id, agent_pid, agent_state, \
         merge_requested, created_at, updated_at";

    /// [`Card::editable`] as a `WHERE` clause, so the test and the write it
    /// guards are one statement.
    const EDITABLE: &'static str = "lane = 'todo' AND session_id IS NULL";

    /// Whether the task can still be rewritten. Nothing has read it yet: the
    /// card is waiting in To Do with no session behind it.
    pub fn editable(&self) -> bool {
        self.lane == Lane::Todo && self.session_id.is_none()
    }

    /// The opening message for a fresh session, or nothing when resuming — the
    /// agent already has the task in its transcript.
    ///
    /// NB: the task alone. The title is the board's label for this card, not
    /// something the agent needs to be told.
    pub fn opening_prompt(&self) -> Option<String> {
        if self.session_id.is_some() {
            return None;
        }

        Some(self.task.trim().to_owned()).filter(|p| !p.is_empty())
    }

    // ---- queries ------------------------------------------------------------

    pub async fn find(db: &Db, id: i64) -> Option<Self> {
        sqlx::query_as(sql(format!(
            "SELECT {} FROM cards WHERE id = ?1",
            Self::COLUMNS
        )))
        .bind(id)
        .fetch_optional(db.pool())
        .await
        .ok()
        .flatten()
    }

    /// Every card the board and its counts should see.
    ///
    /// NB: collected cards are excluded here rather than at each call site —
    /// this is the only read that feeds the board, and they belong on none of it.
    pub async fn for_project(db: &Db, project_id: i64) -> Vec<Self> {
        sqlx::query_as(sql(format!(
            "SELECT {} FROM cards
             WHERE project_id = ?1 AND lane != '{}'
             ORDER BY position",
            Self::COLUMNS,
            Lane::GarbageCollected.as_str()
        )))
        .bind(project_id)
        .fetch_all(db.pool())
        .await
        .unwrap_or_default()
    }

    /// Cards that claim to have a live agent — the startup sweep's candidates.
    pub async fn with_agent_pid(db: &Db) -> Vec<Self> {
        sqlx::query_as(sql(format!(
            "SELECT {} FROM cards WHERE agent_pid IS NOT NULL",
            Self::COLUMNS
        )))
        .fetch_all(db.pool())
        .await
        .unwrap_or_default()
    }

    /// The cards on a board that still have a worktree, as `(id, path)`.
    pub async fn live_worktrees(db: &Db, project_id: i64) -> Vec<(i64, String)> {
        sqlx::query_as(
            "SELECT id, worktree_path FROM cards
             WHERE project_id = ?1 AND worktree_path IS NOT NULL",
        )
        .bind(project_id)
        .fetch_all(db.pool())
        .await
        .unwrap_or_default()
    }

    pub async fn create(db: &Db, new: NewCard<'_>) -> sqlx::Result<i64> {
        // NB: in a transaction. The position is read and then written, and
        // between the two another card can be created — leaving both at the
        // same place in the lane.
        let mut tx = db.pool().begin().await?;

        let position: f64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(position), -1) + 1 FROM cards
             WHERE project_id = ?1 AND lane = 'todo'",
        )
        .bind(new.project_id)
        .fetch_one(&mut *tx)
        .await
        .unwrap_or(0.0);

        let id = sqlx::query(
            "INSERT INTO cards
                 (project_id, task, base_branch, position, permission_mode, model)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(new.project_id)
        .bind(new.task)
        .bind(new.base_branch)
        .bind(position)
        .bind(new.permission_mode)
        .bind(new.model)
        .execute(&mut *tx)
        .await?
        .last_insert_rowid();

        tx.commit().await?;
        Ok(id)
    }

    /// Rewrites a card that has not been handed to an agent yet, reporting
    /// whether it was still editable when the write landed.
    pub async fn update(db: &Db, id: i64, edit: CardEdit<'_>) -> sqlx::Result<bool> {
        let rows = sqlx::query(sql(format!(
            "UPDATE cards SET task = ?1, base_branch = ?2,
                 permission_mode = ?3, model = ?4, updated_at = datetime('now')
             WHERE id = ?5 AND {}",
            Self::EDITABLE
        )))
        .bind(edit.task)
        .bind(edit.base_branch)
        .bind(edit.permission_mode)
        .bind(edit.model)
        .bind(id)
        .execute(db.pool())
        .await?
        .rows_affected();

        Ok(rows > 0)
    }

    pub async fn delete(db: &Db, id: i64) -> sqlx::Result<()> {
        sqlx::query("DELETE FROM cards WHERE id = ?1")
            .bind(id)
            .execute(db.pool())
            .await?;
        Ok(())
    }

    /// Renames a card from the outside: the session naming itself.
    ///
    /// NB: deliberately not gated by [`Self::EDITABLE`], which the form's
    /// `update` is. This lands once the agent is well under way, which is the
    /// whole point of it.
    pub async fn set_title(db: &Db, id: i64, title: &str) {
        let _ = sqlx::query(
            "UPDATE cards SET title = ?1, updated_at = datetime('now')
             WHERE id = ?2 AND title IS NOT ?1",
        )
        .bind(title)
        .bind(id)
        .execute(db.pool())
        .await;
    }

    pub async fn set_lane(db: &Db, id: i64, lane: Lane) {
        let _ =
            sqlx::query("UPDATE cards SET lane = ?1, updated_at = datetime('now') WHERE id = ?2")
                .bind(lane.as_str())
                .bind(id)
                .execute(db.pool())
                .await;
    }

    pub async fn set_agent_state(db: &Db, id: i64, state: AgentState) {
        let _ = sqlx::query(
            "UPDATE cards SET agent_state = ?1, updated_at = datetime('now') WHERE id = ?2",
        )
        .bind(state.as_str())
        .bind(id)
        .execute(db.pool())
        .await;
    }

    pub async fn set_session_id(db: &Db, id: i64, session_id: &str) {
        let _ =
            sqlx::query("UPDATE cards SET session_id = ?1 WHERE id = ?2 AND session_id IS NOT ?1")
                .bind(session_id)
                .bind(id)
                .execute(db.pool())
                .await;
    }

    /// Forgets the recorded session, so the next start opens a fresh one.
    pub async fn clear_session_id(db: &Db, id: i64) {
        let _ = sqlx::query("UPDATE cards SET session_id = NULL WHERE id = ?1")
            .bind(id)
            .execute(db.pool())
            .await;
    }

    /// Hands the card a worktree and marks its agent starting.
    ///
    /// NB: one write. Between the two a reader would find a card holding a
    /// worktree with nothing running in it, which is what the board draws a
    /// stopped card with a diff from.
    pub async fn starting(db: &Db, id: i64, worktree: &str) -> sqlx::Result<()> {
        sqlx::query(
            "UPDATE cards SET worktree_path = ?1, agent_pid = NULL,
                 agent_state = ?2, updated_at = datetime('now')
             WHERE id = ?3",
        )
        .bind(worktree)
        .bind(AgentState::Starting.as_str())
        .bind(id)
        .execute(db.pool())
        .await
        .map(|_| ())
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn attach_worktree(db: &Db, id: i64, worktree: &str, pid: Option<i64>) {
        let _ = sqlx::query(
            "UPDATE cards SET worktree_path = ?1, agent_pid = ?2, updated_at = datetime('now')
             WHERE id = ?3",
        )
        .bind(worktree)
        .bind(pid)
        .bind(id)
        .execute(db.pool())
        .await;
    }

    pub async fn set_agent_pid(db: &Db, id: i64, pid: Option<i64>) {
        let _ = sqlx::query("UPDATE cards SET agent_pid = ?1 WHERE id = ?2")
            .bind(pid)
            .bind(id)
            .execute(db.pool())
            .await;
    }

    pub async fn detach_worktree(db: &Db, id: i64) {
        let _ = sqlx::query(
            "UPDATE cards SET worktree_path = NULL, session_id = NULL, agent_pid = NULL
             WHERE id = ?1",
        )
        .bind(id)
        .execute(db.pool())
        .await;
    }

    pub async fn request_merge(db: &Db, id: i64, base_sha: &str) {
        let _ =
            sqlx::query("UPDATE cards SET merge_requested = 1, merge_base_sha = ?1 WHERE id = ?2")
                .bind(base_sha)
                .bind(id)
                .execute(db.pool())
                .await;
    }

    pub async fn merge_base_sha(db: &Db, id: i64) -> Option<String> {
        sqlx::query_scalar("SELECT merge_base_sha FROM cards WHERE id = ?1")
            .bind(id)
            .fetch_optional(db.pool())
            .await
            .ok()
            .flatten()
            .flatten()
    }

    pub async fn clear_merge_request(db: &Db, id: i64) {
        let _ = sqlx::query("UPDATE cards SET merge_requested = 0 WHERE id = ?1")
            .bind(id)
            .execute(db.pool())
            .await;
    }

    /// Slots `id` into `lane` at `index` and renumbers the lane.
    ///
    /// Positions are rewritten wholesale rather than interpolated: the lanes are
    /// small, and it keeps them free of float drift.
    pub async fn reorder(db: &Db, id: i64, project_id: i64, lane: Lane, index: usize) {
        // NB: in a transaction, and the lane change is part of it. The
        // renumbering reads the lane it is about to rewrite, so a second move
        // landing in the middle would renumber against an order that has gone.
        let Ok(mut tx) = db.pool().begin().await else {
            return;
        };

        let moved =
            sqlx::query("UPDATE cards SET lane = ?1, updated_at = datetime('now') WHERE id = ?2")
                .bind(lane.as_str())
                .bind(id)
                .execute(&mut *tx)
                .await;
        if moved.is_err() {
            return;
        }

        let mut ids: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM cards
             WHERE project_id = ?1 AND lane = ?2 AND id != ?3
             ORDER BY position",
        )
        .bind(project_id)
        .bind(lane.as_str())
        .bind(id)
        .fetch_all(&mut *tx)
        .await
        .unwrap_or_default();

        ids.insert(index.min(ids.len()), id);
        for (position, card_id) in ids.iter().enumerate() {
            let _ = sqlx::query("UPDATE cards SET position = ?1 WHERE id = ?2")
                .bind(position as f64)
                .bind(card_id)
                .execute(&mut *tx)
                .await;
        }

        let _ = tx.commit().await;
    }
}

/// NB: written out rather than derived. Two of the fields are not columns —
/// `lane_label` and `agent_state_label` are what the enum says about itself —
/// and the two that are columns are strings this app parses rather than stores
/// as a type.
impl<'r> FromRow<'r, SqliteRow> for Card {
    fn from_row(row: &'r SqliteRow) -> sqlx::Result<Self> {
        let agent_state = AgentState::parse(&row.try_get::<String, _>("agent_state")?);
        let lane = Lane::parse(&row.try_get::<String, _>("lane")?);
        Ok(Self {
            id: row.try_get("id")?,
            project_id: row.try_get("project_id")?,
            title: row.try_get("title")?,
            task: row.try_get("task")?,
            base_branch: row.try_get("base_branch")?,
            lane,
            lane_label: lane.label(),
            position: row.try_get("position")?,
            permission_mode: row.try_get("permission_mode")?,
            model: row.try_get("model")?,
            worktree_path: row.try_get("worktree_path")?,
            session_id: row.try_get("session_id")?,
            agent_pid: row.try_get("agent_pid")?,
            agent_state,
            agent_state_label: agent_state.label(),
            merge_requested: row.try_get("merge_requested")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tests::memory_db;

    async fn seeded() -> (Db, i64) {
        let db = memory_db().await;
        let id = crate::project::Project::upsert(&db, std::path::Path::new("/srv/repo"))
            .await
            .unwrap();
        (db, id)
    }

    async fn add(db: &Db, project_id: i64, task: &str) -> i64 {
        Card::create(
            db,
            NewCard {
                project_id,
                task,
                base_branch: "main",
                permission_mode: "acceptEdits",
                model: None,
            },
        )
        .await
        .unwrap()
    }

    #[test]
    fn lanes_and_states_round_trip_through_their_strings() {
        for lane in Lane::ALL {
            assert_eq!(Lane::parse(lane.as_str()), *lane);
        }
        for state in AgentState::ALL {
            assert_eq!(AgentState::parse(state.as_str()), *state);
        }
    }

    #[test]
    fn every_lane_but_the_collected_one_is_visible() {
        assert!(Lane::VISIBLE.iter().all(|lane| Lane::ALL.contains(lane)));
        assert_eq!(Lane::VISIBLE.len(), Lane::ALL.len() - 1);
        assert!(!Lane::VISIBLE.contains(&Lane::GarbageCollected));
    }

    #[test]
    fn unknown_values_fall_back_rather_than_panicking() {
        assert_eq!(Lane::parse("archived"), Lane::Todo);
        assert_eq!(AgentState::parse("confused"), AgentState::Stopped);
    }

    #[tokio::test]
    async fn a_new_card_starts_in_todo_at_the_end() {
        let (db, project_id) = seeded().await;

        let first = add(&db, project_id, "first").await;
        let second = add(&db, project_id, "second").await;

        let cards = Card::for_project(&db, project_id).await;
        assert_eq!(
            cards.iter().map(|c| c.id).collect::<Vec<_>>(),
            [first, second]
        );
        assert!(cards.iter().all(|c| c.lane == Lane::Todo));
        assert_eq!(cards[0].agent_state, AgentState::Stopped);
    }

    #[tokio::test]
    async fn a_collected_card_leaves_the_board_but_not_the_database() {
        let (db, project_id) = seeded().await;

        let kept = add(&db, project_id, "kept").await;
        let collected = add(&db, project_id, "collected").await;
        Card::set_lane(&db, collected, Lane::GarbageCollected).await;

        // Gone from the board and from the project's card count...
        let visible = Card::for_project(&db, project_id).await;
        assert_eq!(visible.iter().map(|c| c.id).collect::<Vec<_>>(), [kept]);

        // ...but the row, and the lane it round-trips through, are still there.
        let card = Card::find(&db, collected).await.unwrap();
        assert_eq!(card.lane, Lane::GarbageCollected);
        assert_eq!(card.task, "collected");
    }

    #[tokio::test]
    async fn reorder_moves_a_card_and_renumbers_the_lane() {
        let (db, project_id) = seeded().await;

        let a = add(&db, project_id, "a").await;
        let b = add(&db, project_id, "b").await;
        let c = add(&db, project_id, "c").await;

        Card::reorder(&db, c, project_id, Lane::Todo, 0).await;

        let order: Vec<_> = Card::for_project(&db, project_id)
            .await
            .iter()
            .map(|card| card.id)
            .collect();
        assert_eq!(order, [c, a, b]);

        // Positions are compacted, not left with gaps.
        let positions: Vec<_> = Card::for_project(&db, project_id)
            .await
            .iter()
            .map(|card| card.position)
            .collect();
        assert_eq!(positions, [0.0, 1.0, 2.0]);
    }

    #[tokio::test]
    async fn reorder_across_lanes_clamps_an_out_of_range_index() {
        let (db, project_id) = seeded().await;

        let a = add(&db, project_id, "a").await;
        Card::reorder(&db, a, project_id, Lane::InReview, 99).await;

        let card = Card::find(&db, a).await.unwrap();
        assert_eq!(card.lane, Lane::InReview);
        assert_eq!(card.position, 0.0);
    }

    #[tokio::test]
    async fn the_opening_prompt_is_the_task_and_nothing_else() {
        let (db, project_id) = seeded().await;

        let id = add(&db, project_id, "Add a flag\n\nMake it verbose.").await;
        Card::set_title(&db, id, "Filed under something else").await;

        // A card carries the session's name, which is no part of the task.
        let card = Card::find(&db, id).await.unwrap();
        assert_eq!(card.title.as_deref(), Some("Filed under something else"));
        assert_eq!(
            card.opening_prompt().unwrap(),
            "Add a flag\n\nMake it verbose."
        );
    }

    #[tokio::test]
    async fn a_resumed_card_has_no_opening_prompt() {
        let (db, project_id) = seeded().await;

        let id = add(&db, project_id, "Add a flag").await;
        Card::set_session_id(&db, id, "session-1").await;

        // The transcript already holds the task, so re-sending it would duplicate.
        assert_eq!(Card::find(&db, id).await.unwrap().opening_prompt(), None);
    }

    #[tokio::test]
    async fn the_startup_sweep_only_considers_cards_with_a_pid() {
        let (db, project_id) = seeded().await;

        let idle = add(&db, project_id, "idle").await;
        let live = add(&db, project_id, "live").await;
        Card::attach_worktree(&db, live, "/srv/worktrees/2", Some(4321)).await;

        let candidates = Card::with_agent_pid(&db).await;
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].id, live);
        assert_eq!(candidates[0].agent_pid, Some(4321));
        assert_eq!(Card::find(&db, idle).await.unwrap().agent_pid, None);
    }

    #[tokio::test]
    async fn a_card_starts_unnamed_however_long_its_task_is() {
        let (db, project_id) = seeded().await;

        // Nothing is cut to fit any more: the board clips what it shows, and
        // the card holds the task whole until a session names it.
        let task = "x".repeat(300);
        let id = add(&db, project_id, &task).await;

        let card = Card::find(&db, id).await.unwrap();
        assert_eq!(card.title, None);
        assert_eq!(card.task, task);
        assert_eq!(card.opening_prompt().unwrap(), task);
    }

    async fn rewrite(db: &Db, id: i64) -> bool {
        Card::update(
            db,
            id,
            CardEdit {
                task: "Teach it to hum\n\nQuietly.",
                base_branch: "release",
                permission_mode: "plan",
                model: Some("opus"),
            },
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn a_card_waiting_in_todo_can_still_be_rewritten() {
        let (db, project_id) = seeded().await;

        let id = add(&db, project_id, "Teach it to whistle").await;
        assert!(Card::find(&db, id).await.unwrap().editable());
        assert!(rewrite(&db, id).await);

        let card = Card::find(&db, id).await.unwrap();
        assert_eq!(card.task, "Teach it to hum\n\nQuietly.");
        assert_eq!(card.base_branch, "release");
        assert_eq!(card.permission_mode, "plan");
        assert_eq!(card.model.as_deref(), Some("opus"));
        // The rewritten task is what the agent will be opened with.
        assert_eq!(
            card.opening_prompt().unwrap(),
            "Teach it to hum\n\nQuietly."
        );
    }

    #[tokio::test]
    async fn a_card_an_agent_has_seen_is_left_alone() {
        let (db, project_id) = seeded().await;

        let moved = add(&db, project_id, "moved").await;
        Card::set_lane(&db, moved, Lane::InProgress).await;

        // Back in To Do, but the session already holds the original task.
        let started = add(&db, project_id, "started").await;
        Card::set_session_id(&db, started, "session-1").await;

        for id in [moved, started] {
            assert!(!Card::find(&db, id).await.unwrap().editable());
            assert!(!rewrite(&db, id).await);
            assert_ne!(
                Card::find(&db, id).await.unwrap().task,
                "Teach it to hum\n\nQuietly."
            );
        }
    }

    #[tokio::test]
    async fn a_session_can_rename_a_card_its_agent_already_holds() {
        let (db, project_id) = seeded().await;

        let id = add(&db, project_id, "Teach it to whistle").await;
        Card::set_lane(&db, id, Lane::InProgress).await;
        Card::set_session_id(&db, id, "session-1").await;

        // The form is shut, but the session naming itself still lands.
        assert!(!rewrite(&db, id).await);
        Card::set_title(&db, id, "Whistling on startup").await;

        let card = Card::find(&db, id).await.unwrap();
        assert_eq!(card.title.as_deref(), Some("Whistling on startup"));
        // Only the label moved; the agent's task is untouched.
        assert_eq!(card.task, "Teach it to whistle");
    }

    #[tokio::test]
    async fn renaming_a_card_to_what_it_is_called_leaves_it_alone() {
        let (db, project_id) = seeded().await;

        let id = add(&db, project_id, "Teach it to whistle").await;
        Card::set_title(&db, id, "Whistling on startup").await;
        let before = Card::find(&db, id).await.unwrap().updated_at;

        // Every hook re-reads the title, so an unchanged one must not keep
        // bumping `updated_at` and churning the board's ETag.
        Card::set_title(&db, id, "Whistling on startup").await;
        assert_eq!(Card::find(&db, id).await.unwrap().updated_at, before);
    }
}
