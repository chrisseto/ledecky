use rocket::serde::Serialize;
use rusqlite::{Connection, Row};

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
}

impl Lane {
    pub const ALL: &'static [Self] = &[Self::Todo, Self::InProgress, Self::InReview, Self::Done];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Todo => "todo",
            Self::InProgress => "in_progress",
            Self::InReview => "in_review",
            Self::Done => "done",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Todo => "To Do",
            Self::InProgress => "In Progress",
            Self::InReview => "In Review",
            Self::Done => "Done",
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
    AwaitingPermission,
    Misconfigured,
    Error,
}

impl AgentState {
    pub const ALL: &'static [Self] = &[
        Self::Stopped,
        Self::Starting,
        Self::Running,
        Self::Idle,
        Self::AwaitingPermission,
        Self::Misconfigured,
        Self::Error,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Idle => "idle",
            Self::AwaitingPermission => "awaiting_permission",
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
            Self::AwaitingPermission => "needs permission",
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

macro_rules! to_sql {
    ($ty:ty) => {
        impl rusqlite::ToSql for $ty {
            fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
                Ok(self.as_str().into())
            }
        }
    };
}
to_sql!(Lane);
to_sql!(AgentState);

/// One unit of work: a task, its worktree, and the agent doing it.
#[derive(Debug, Clone, Serialize)]
#[serde(crate = "rocket::serde")]
pub struct Card {
    pub id: i64,
    pub project_id: i64,
    pub title: String,
    pub description: String,
    pub base_branch: String,
    pub lane: Lane,
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
    pub title: &'a str,
    pub description: &'a str,
    pub base_branch: &'a str,
    pub permission_mode: &'a str,
    pub model: Option<&'a str>,
}

impl Card {
    const COLUMNS: &'static str = "id, project_id, title, description, base_branch, lane, position, \
         permission_mode, model, worktree_path, session_id, agent_pid, agent_state, \
         merge_requested, created_at, updated_at";

    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let agent_state = AgentState::parse(&row.get::<_, String>("agent_state")?);
        Ok(Self {
            id: row.get("id")?,
            project_id: row.get("project_id")?,
            title: row.get("title")?,
            description: row.get("description")?,
            base_branch: row.get("base_branch")?,
            lane: Lane::parse(&row.get::<_, String>("lane")?),
            position: row.get("position")?,
            permission_mode: row.get("permission_mode")?,
            model: row.get("model")?,
            worktree_path: row.get("worktree_path")?,
            session_id: row.get("session_id")?,
            agent_pid: row.get("agent_pid")?,
            agent_state,
            agent_state_label: agent_state.label(),
            merge_requested: row.get("merge_requested")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }

    /// The opening message for a fresh session, or nothing when resuming — the
    /// agent already has the task in its transcript.
    pub fn opening_prompt(&self) -> Option<String> {
        if self.session_id.is_some() {
            return None;
        }

        let description = self.description.trim();
        let prompt = if description.is_empty() {
            self.title.trim().to_owned()
        } else {
            format!("{}\n\n{description}", self.title.trim())
        };

        Some(prompt).filter(|p| !p.is_empty())
    }

    // ---- queries ------------------------------------------------------------

    pub fn find(conn: &Connection, id: i64) -> Option<Self> {
        conn.query_row(
            &format!("SELECT {} FROM cards WHERE id = ?1", Self::COLUMNS),
            [id],
            Self::from_row,
        )
        .ok()
    }

    pub fn for_project(conn: &Connection, project_id: i64) -> Vec<Self> {
        conn.prepare(&format!(
            "SELECT {} FROM cards WHERE project_id = ?1 ORDER BY position",
            Self::COLUMNS
        ))
        .and_then(|mut stmt| {
            stmt.query_map([project_id], Self::from_row)
                .map(|rows| rows.filter_map(Result::ok).collect())
        })
        .unwrap_or_default()
    }

    /// Cards that claim to have a live agent — the startup sweep's candidates.
    pub fn with_agent_pid(conn: &Connection) -> Vec<Self> {
        conn.prepare(&format!(
            "SELECT {} FROM cards WHERE agent_pid IS NOT NULL",
            Self::COLUMNS
        ))
        .and_then(|mut stmt| {
            stmt.query_map([], Self::from_row)
                .map(|rows| rows.filter_map(Result::ok).collect())
        })
        .unwrap_or_default()
    }

    pub fn create(conn: &Connection, new: NewCard<'_>) -> rusqlite::Result<i64> {
        let position: f64 = conn
            .query_row(
                "SELECT COALESCE(MAX(position), -1) + 1 FROM cards
                 WHERE project_id = ?1 AND lane = 'todo'",
                [new.project_id],
                |r| r.get(0),
            )
            .unwrap_or(0.0);

        conn.execute(
            "INSERT INTO cards
                 (project_id, title, description, base_branch, position, permission_mode, model)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                new.project_id,
                new.title,
                new.description,
                new.base_branch,
                position,
                new.permission_mode,
                new.model
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn delete(conn: &Connection, id: i64) -> rusqlite::Result<()> {
        conn.execute("DELETE FROM cards WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn set_lane(conn: &Connection, id: i64, lane: Lane) {
        let _ = conn.execute(
            "UPDATE cards SET lane = ?1, updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![lane, id],
        );
    }

    pub fn set_agent_state(conn: &Connection, id: i64, state: AgentState) {
        let _ = conn.execute(
            "UPDATE cards SET agent_state = ?1, updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![state, id],
        );
    }

    pub fn set_session_id(conn: &Connection, id: i64, session_id: &str) {
        let _ = conn.execute(
            "UPDATE cards SET session_id = ?1 WHERE id = ?2 AND session_id IS NOT ?1",
            rusqlite::params![session_id, id],
        );
    }

    pub fn attach_worktree(conn: &Connection, id: i64, worktree: &str, pid: Option<i64>) {
        let _ = conn.execute(
            "UPDATE cards SET worktree_path = ?1, agent_pid = ?2, updated_at = datetime('now')
             WHERE id = ?3",
            rusqlite::params![worktree, pid, id],
        );
    }

    pub fn set_agent_pid(conn: &Connection, id: i64, pid: Option<i64>) {
        let _ = conn.execute(
            "UPDATE cards SET agent_pid = ?1 WHERE id = ?2",
            rusqlite::params![pid, id],
        );
    }

    pub fn detach_worktree(conn: &Connection, id: i64) {
        let _ = conn.execute(
            "UPDATE cards SET worktree_path = NULL, session_id = NULL, agent_pid = NULL
             WHERE id = ?1",
            [id],
        );
    }

    pub fn request_merge(conn: &Connection, id: i64, base_sha: &str) {
        let _ = conn.execute(
            "UPDATE cards SET merge_requested = 1, merge_base_sha = ?1 WHERE id = ?2",
            rusqlite::params![base_sha, id],
        );
    }

    pub fn merge_base_sha(conn: &Connection, id: i64) -> Option<String> {
        conn.query_row(
            "SELECT merge_base_sha FROM cards WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .ok()
        .flatten()
    }

    pub fn clear_merge_request(conn: &Connection, id: i64) {
        let _ = conn.execute("UPDATE cards SET merge_requested = 0 WHERE id = ?1", [id]);
    }

    /// Slots `id` into `lane` at `index` and renumbers the lane.
    ///
    /// Positions are rewritten wholesale rather than interpolated: the lanes are
    /// small, and it keeps them free of float drift.
    pub fn reorder(conn: &Connection, id: i64, project_id: i64, lane: Lane, index: usize) {
        Self::set_lane(conn, id, lane);

        let mut ids: Vec<i64> = conn
            .prepare(
                "SELECT id FROM cards
                 WHERE project_id = ?1 AND lane = ?2 AND id != ?3
                 ORDER BY position",
            )
            .and_then(|mut stmt| {
                stmt.query_map(rusqlite::params![project_id, lane, id], |r| r.get(0))
                    .map(|rows| rows.filter_map(Result::ok).collect())
            })
            .unwrap_or_default();

        ids.insert(index.min(ids.len()), id);
        for (position, card_id) in ids.iter().enumerate() {
            let _ = conn.execute(
                "UPDATE cards SET position = ?1 WHERE id = ?2",
                rusqlite::params![position as f64, card_id],
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tests::memory_db;

    fn seeded() -> (crate::db::Db, i64) {
        let db = memory_db();
        let id = {
            let conn = db.lock();
            crate::project::Project::upsert(&conn, std::path::Path::new("/srv/repo")).unwrap()
        };
        (db, id)
    }

    fn add(conn: &Connection, project_id: i64, title: &str) -> i64 {
        Card::create(
            conn,
            NewCard {
                project_id,
                title,
                description: "",
                base_branch: "main",
                permission_mode: "acceptEdits",
                model: None,
            },
        )
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
    fn unknown_values_fall_back_rather_than_panicking() {
        assert_eq!(Lane::parse("archived"), Lane::Todo);
        assert_eq!(AgentState::parse("confused"), AgentState::Stopped);
    }

    #[test]
    fn a_new_card_starts_in_todo_at_the_end() {
        let (db, project_id) = seeded();
        let conn = db.lock();

        let first = add(&conn, project_id, "first");
        let second = add(&conn, project_id, "second");

        let cards = Card::for_project(&conn, project_id);
        assert_eq!(cards.iter().map(|c| c.id).collect::<Vec<_>>(), [first, second]);
        assert!(cards.iter().all(|c| c.lane == Lane::Todo));
        assert_eq!(cards[0].agent_state, AgentState::Stopped);
    }

    #[test]
    fn reorder_moves_a_card_and_renumbers_the_lane() {
        let (db, project_id) = seeded();
        let conn = db.lock();

        let a = add(&conn, project_id, "a");
        let b = add(&conn, project_id, "b");
        let c = add(&conn, project_id, "c");

        Card::reorder(&conn, c, project_id, Lane::Todo, 0);

        let order: Vec<_> = Card::for_project(&conn, project_id)
            .iter()
            .map(|card| card.id)
            .collect();
        assert_eq!(order, [c, a, b]);

        // Positions are compacted, not left with gaps.
        let positions: Vec<_> = Card::for_project(&conn, project_id)
            .iter()
            .map(|card| card.position)
            .collect();
        assert_eq!(positions, [0.0, 1.0, 2.0]);
    }

    #[test]
    fn reorder_across_lanes_clamps_an_out_of_range_index() {
        let (db, project_id) = seeded();
        let conn = db.lock();

        let a = add(&conn, project_id, "a");
        Card::reorder(&conn, a, project_id, Lane::InReview, 99);

        let card = Card::find(&conn, a).unwrap();
        assert_eq!(card.lane, Lane::InReview);
        assert_eq!(card.position, 0.0);
    }

    #[test]
    fn the_opening_prompt_joins_title_and_description() {
        let (db, project_id) = seeded();
        let conn = db.lock();

        let id = Card::create(
            &conn,
            NewCard {
                project_id,
                title: "Add a flag",
                description: "Make it verbose.",
                base_branch: "main",
                permission_mode: "acceptEdits",
                model: None,
            },
        )
        .unwrap();

        let card = Card::find(&conn, id).unwrap();
        assert_eq!(
            card.opening_prompt().unwrap(),
            "Add a flag\n\nMake it verbose."
        );
    }

    #[test]
    fn a_resumed_card_has_no_opening_prompt() {
        let (db, project_id) = seeded();
        let conn = db.lock();

        let id = add(&conn, project_id, "Add a flag");
        Card::set_session_id(&conn, id, "session-1");

        // The transcript already holds the task, so re-sending it would duplicate.
        assert_eq!(Card::find(&conn, id).unwrap().opening_prompt(), None);
    }

    #[test]
    fn the_startup_sweep_only_considers_cards_with_a_pid() {
        let (db, project_id) = seeded();
        let conn = db.lock();

        let idle = add(&conn, project_id, "idle");
        let live = add(&conn, project_id, "live");
        Card::attach_worktree(&conn, live, "/srv/worktrees/2", Some(4321));

        let candidates = Card::with_agent_pid(&conn);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].id, live);
        assert_eq!(candidates[0].agent_pid, Some(4321));
        assert_eq!(Card::find(&conn, idle).unwrap().agent_pid, None);
    }
}
