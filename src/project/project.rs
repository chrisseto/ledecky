use std::path::{Path, PathBuf};

use minijinja::context;
use rocket::form::Form;
use rocket::response::Redirect;
use rocket::serde::Serialize;
use rocket::{get, post, State};
use rusqlite::{Connection, Row};

use crate::config::Settings;
use crate::db::Db;
use crate::project::board::{self, Shell};
use crate::review::DiffCache;
use crate::tmpl::Tmpl;

/// A git repository the board tracks work against.
#[derive(Debug, Clone, Serialize)]
#[serde(crate = "rocket::serde")]
pub struct Project {
    pub id: i64,
    pub name: String,
    pub path: String,
    pub created_at: String,
}

impl Project {
    const COLUMNS: &'static str = "id, name, path, created_at";

    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            name: row.get("name")?,
            path: row.get("path")?,
            created_at: row.get("created_at")?,
        })
    }

    pub fn repo(&self) -> PathBuf {
        PathBuf::from(&self.path)
    }

    pub fn find(conn: &Connection, id: i64) -> Option<Self> {
        conn.query_row(
            &format!("SELECT {} FROM projects WHERE id = ?1", Self::COLUMNS),
            [id],
            Self::from_row,
        )
        .ok()
    }

    pub fn all(conn: &Connection) -> Vec<Self> {
        conn.prepare(&format!(
            "SELECT {} FROM projects ORDER BY name",
            Self::COLUMNS
        ))
        .and_then(|mut stmt| {
            stmt.query_map([], Self::from_row)
                .map(|rows| rows.filter_map(Result::ok).collect())
        })
        .unwrap_or_default()
    }

    /// Registers `path`, or returns the id it already has.
    pub fn upsert(conn: &Connection, path: &Path) -> rusqlite::Result<i64> {
        let path = path.to_string_lossy();
        if let Ok(id) = conn.query_row("SELECT id FROM projects WHERE path = ?1", [&path], |r| {
            r.get::<_, i64>(0)
        }) {
            return Ok(id);
        }

        let name = Path::new(path.as_ref())
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("project");

        conn.execute(
            "INSERT INTO projects (name, path) VALUES (?1, ?2)",
            rusqlite::params![name, path],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Overrides the name taken from the directory.
    pub fn rename(conn: &Connection, id: i64, name: &str) {
        let _ = conn.execute(
            "UPDATE projects SET name = ?2 WHERE id = ?1",
            rusqlite::params![id, name],
        );
    }
}

// ---- routes -----------------------------------------------------------------

#[get("/projects/new?<board>")]
pub fn new(
    db: &State<Db>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    board: Option<i64>,
) -> Tmpl {
    let project = board::current(db, board);
    Shell {
        db,
        settings,
        cache,
    }
    .render(
        project,
        board::ADD_PROJECT,
        context! { path => default_root(), error => Option::<String>::None },
    )
}

#[derive(rocket::FromForm)]
pub struct ProjectForm {
    path: String,
    /// Optional; the directory's own name is the default.
    name: Option<String>,
    /// The board the modal was opened over, so a rejection lands back on it.
    board: Option<i64>,
}

#[post("/projects", data = "<form>")]
pub fn create(
    db: &State<Db>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    form: Form<ProjectForm>,
) -> Result<Redirect, Tmpl> {
    let path = expand(form.path.trim());

    let reject = |message: String| {
        Shell {
            db,
            settings,
            cache,
        }
        .render(
            board::current(db, form.board),
            board::ADD_PROJECT,
            context! { path => form.path.clone(), error => Some(message) },
        )
    };

    if !path.is_dir() {
        return Err(reject(format!("{} is not a directory", path.display())));
    }
    if !is_git_repo(&path) {
        return Err(reject(format!(
            "{} is not a git repository",
            path.display()
        )));
    }

    let path = path.canonicalize().unwrap_or(path);

    // The lock goes back before `reject` runs: rendering the modal again takes
    // it for itself.
    let saved = {
        let conn = db.lock();
        let saved = Project::upsert(&conn, &path);
        let name = form
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty());

        if let (Ok(id), Some(name)) = (&saved, name) {
            Project::rename(&conn, *id, name);
        }
        saved
    };

    let id = saved.map_err(|err| reject(format!("could not save project: {err}")))?;
    Ok(Redirect::to(format!("/projects/{id}")))
}

/// Server-rendered directory autocomplete. Returns just the `<ul>` fragment;
/// unpoly swaps it in on every keystroke.
#[get("/projects/complete?<q>")]
pub fn complete(q: Option<String>) -> Tmpl {
    let q = q.unwrap_or_default();
    Tmpl(
        "_completions.html",
        context! { entries => complete_path(&q) },
    )
}

// ---- directory completion ---------------------------------------------------

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(crate = "rocket::serde")]
pub struct Completion {
    pub path: String,
    pub name: String,
    pub is_repo: bool,
}

fn complete_path(query: &str) -> Vec<Completion> {
    let raw = if query.trim().is_empty() {
        default_root()
    } else {
        query.to_owned()
    };

    let (dir, prefix) = split_query(&raw);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };

    let wanted = prefix.to_lowercase();
    let mut out: Vec<Completion> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Hidden directories only surface once the user types the dot.
            if name.starts_with('.') && !prefix.starts_with('.') {
                return None;
            }
            if !name.to_lowercase().starts_with(&wanted) {
                return None;
            }

            let path = entry.path();
            Some(Completion {
                name,
                is_repo: is_git_repo(&path),
                path: path.to_string_lossy().into_owned(),
            })
        })
        .collect();

    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.truncate(50);
    out
}

/// Splits what was typed into the directory to list and the prefix to match.
///
/// A trailing slash means "show me what is inside this"; otherwise the last
/// segment is a partial name.
fn split_query(raw: &str) -> (PathBuf, String) {
    let expanded = expand(raw);

    if raw.ends_with('/') {
        return (expanded, String::new());
    }

    match (expanded.parent(), expanded.file_name()) {
        (Some(parent), Some(name)) => (parent.to_path_buf(), name.to_string_lossy().into_owned()),
        _ => (expanded, String::new()),
    }
}

fn is_git_repo(path: &Path) -> bool {
    // `.git` is a directory in a normal clone and a file in a linked worktree.
    path.join(".git").exists()
}

fn expand(path: &str) -> PathBuf {
    expand_with_home(path, &std::env::var("HOME").unwrap_or_default())
}

/// Split out from `expand` so it can be tested without mutating the process
/// environment, which other tests read concurrently.
fn expand_with_home(path: &str, home: &str) -> PathBuf {
    match path.strip_prefix('~') {
        Some(rest) => PathBuf::from(format!("{home}{rest}")),
        None => PathBuf::from(path),
    }
}

fn default_root() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    format!("{home}/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tests::memory_db;

    #[test]
    fn a_trailing_slash_lists_the_directory() {
        let (dir, prefix) = split_query("/tmp/");
        assert_eq!(dir, PathBuf::from("/tmp/"));
        assert_eq!(prefix, "");
    }

    #[test]
    fn a_partial_name_filters_its_parent() {
        let (dir, prefix) = split_query("/tmp/kan");
        assert_eq!(dir, PathBuf::from("/tmp"));
        assert_eq!(prefix, "kan");
    }

    #[test]
    fn a_leading_tilde_expands_to_home() {
        assert_eq!(
            expand_with_home("~/code", "/home/someone"),
            PathBuf::from("/home/someone/code")
        );
        assert_eq!(
            expand_with_home("/absolute", "/home/someone"),
            PathBuf::from("/absolute")
        );
    }

    #[test]
    fn completion_finds_directories_and_flags_repositories() {
        let root = tempdir("completion-lists");
        std::fs::create_dir_all(root.join("alpha/.git")).unwrap();
        std::fs::create_dir_all(root.join("beta")).unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::write(root.join("a-file"), "").unwrap();

        let found = complete_path(&format!("{}/", root.display()));
        let names: Vec<_> = found.iter().map(|c| c.name.as_str()).collect();

        // Files and dot-directories stay out; repositories are marked.
        assert_eq!(names, ["alpha", "beta"]);
        assert!(found[0].is_repo);
        assert!(!found[1].is_repo);
    }

    #[test]
    fn completion_respects_a_typed_prefix() {
        let root = tempdir("completion-prefix");
        std::fs::create_dir_all(root.join("alpha")).unwrap();
        std::fs::create_dir_all(root.join("beta")).unwrap();

        let found = complete_path(&format!("{}/al", root.display()));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "alpha");
    }

    #[test]
    fn upsert_is_idempotent() {
        let db = memory_db();
        let conn = db.lock();

        let first = Project::upsert(&conn, Path::new("/srv/repo")).unwrap();
        let second = Project::upsert(&conn, Path::new("/srv/repo")).unwrap();
        assert_eq!(first, second);

        let project = Project::find(&conn, first).unwrap();
        assert_eq!(project.name, "repo");
        assert_eq!(Project::all(&conn).len(), 1);
    }

    /// Per-test directory; tests run concurrently, so the name has to be unique.
    fn tempdir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("kanban2-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }
}
