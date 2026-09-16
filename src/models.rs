use rusqlite::Row;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Project {
    pub id: i64,
    pub name: String,
    pub path: String,
    pub created_at: String,
}

impl Project {
    pub const COLUMNS: &'static str = "id, name, path, created_at";

    pub fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            name: row.get("name")?,
            path: row.get("path")?,
            created_at: row.get("created_at")?,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
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
    pub agent_state: AgentState,
    pub agent_state_label: &'static str,
    pub merge_requested: bool,
    pub created_at: String,
    pub updated_at: String,
}

impl Card {
    pub const COLUMNS: &'static str = "id, project_id, title, description, base_branch, lane, \
         position, permission_mode, model, worktree_path, session_id, agent_state, merge_requested, \
         created_at, \
         updated_at";

    pub fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
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
            agent_state,
            agent_state_label: agent_state.label(),
            merge_requested: row.get("merge_requested")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }
}

macro_rules! str_enum {
    ($name:ident { $($variant:ident => $repr:literal, $label:literal);+ $(;)? }, default $default:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            #[allow(dead_code)]
            #[allow(dead_code)] // not every enum enumerates itself in a template
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $repr),+ }
            }

            pub fn label(self) -> &'static str {
                match self { $(Self::$variant => $label),+ }
            }

            pub fn parse(s: &str) -> Self {
                match s { $($repr => Self::$variant,)+ _ => Self::$default }
            }
        }

        impl rusqlite::ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
                Ok(self.as_str().into())
            }
        }
    };
}

str_enum!(Lane {
    Todo        => "todo",        "To Do";
    InProgress  => "in_progress", "In Progress";
    InReview    => "in_review",   "In Review";
    Done        => "done",        "Done";
}, default Todo);

str_enum!(AgentState {
    Stopped            => "stopped",            "stopped";
    Starting           => "starting",           "starting";
    Running            => "running",            "working";
    Idle               => "idle",               "idle";
    AwaitingPermission => "awaiting_permission", "needs permission";
    Misconfigured      => "misconfigured",      "hooks not reaching server";
    Error              => "error",              "error";
}, default Stopped);

#[derive(Debug, Clone, Serialize)]
pub struct Turn {
    pub id: i64,
    pub n: i64,
    pub commit_sha: String,
    pub parent_sha: String,
    pub last_assistant_message: Option<String>,
    pub created_at: String,
}

impl Turn {
    pub const COLUMNS: &'static str =
        "id, n, commit_sha, parent_sha, last_assistant_message, created_at";

    pub fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            n: row.get("n")?,
            commit_sha: row.get("commit_sha")?,
            parent_sha: row.get("parent_sha")?,
            last_assistant_message: row.get("last_assistant_message")?,
            created_at: row.get("created_at")?,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Comment {
    pub id: i64,
    pub turn_id: Option<i64>,
    pub file_path: String,
    pub line: i64,
    pub side: String,
    pub body: String,
    pub state: String,
    pub created_at: String,
}

impl Comment {
    pub const COLUMNS: &'static str =
        "id, turn_id, file_path, line, side, body, state, created_at";

    pub fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            turn_id: row.get("turn_id")?,
            file_path: row.get("file_path")?,
            line: row.get("line")?,
            side: row.get("side")?,
            body: row.get("body")?,
            state: row.get("state")?,
            created_at: row.get("created_at")?,
        })
    }
}
