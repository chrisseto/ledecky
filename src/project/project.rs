use std::path::{Path, PathBuf};

use minijinja::context;
use rocket::form::Form;
use rocket::response::Redirect;
use rocket::serde::Serialize;
use rocket::{get, post, State};
use sqlx::sqlite::SqliteRow;
use sqlx::{FromRow, Row};

use crate::config::Settings;
use crate::db::{sql, Db};
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

    pub fn repo(&self) -> PathBuf {
        PathBuf::from(&self.path)
    }

    pub async fn find(db: &Db, id: i64) -> Option<Self> {
        sqlx::query_as(sql(format!(
            "SELECT {} FROM projects WHERE id = ?1",
            Self::COLUMNS
        )))
        .bind(id)
        .fetch_optional(db.pool())
        .await
        .ok()
        .flatten()
    }

    pub async fn all(db: &Db) -> Vec<Self> {
        sqlx::query_as(sql(format!(
            "SELECT {} FROM projects ORDER BY name",
            Self::COLUMNS
        )))
        .fetch_all(db.pool())
        .await
        .unwrap_or_default()
    }

    /// Registers `path`, or returns the id it already has.
    pub async fn upsert(db: &Db, path: &Path) -> sqlx::Result<i64> {
        let path = path.to_string_lossy();

        // NB: in a transaction, because it looks before it inserts — two boards
        // added at once would otherwise both miss and both insert.
        let mut tx = db.pool().begin().await?;

        let existing: Option<i64> = sqlx::query_scalar("SELECT id FROM projects WHERE path = ?1")
            .bind(path.as_ref())
            .fetch_optional(&mut *tx)
            .await?;
        if let Some(id) = existing {
            return Ok(id);
        }

        let name = Path::new(path.as_ref())
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("project");

        let id = sqlx::query("INSERT INTO projects (name, path) VALUES (?1, ?2)")
            .bind(name)
            .bind(path.as_ref())
            .execute(&mut *tx)
            .await?
            .last_insert_rowid();

        tx.commit().await?;
        Ok(id)
    }

    /// Overrides the name taken from the directory.
    pub async fn rename(db: &Db, id: i64, name: &str) {
        let _ = sqlx::query("UPDATE projects SET name = ?2 WHERE id = ?1")
            .bind(id)
            .bind(name)
            .execute(db.pool())
            .await;
    }
}

impl<'r> FromRow<'r, SqliteRow> for Project {
    fn from_row(row: &'r SqliteRow) -> sqlx::Result<Self> {
        Ok(Self {
            id: row.try_get("id")?,
            name: row.try_get("name")?,
            path: row.try_get("path")?,
            created_at: row.try_get("created_at")?,
        })
    }
}

// ---- routes -----------------------------------------------------------------

#[get("/projects/new?<board>")]
pub async fn new(
    db: &State<Db>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    board: Option<i64>,
) -> Tmpl {
    let project = board::current(&db, board).await;
    Shell {
        db: &db,
        settings: &settings,
        cache: &cache,
    }
    .render(
        project,
        board::ADD_PROJECT,
        context! { path => default_root(), error => Option::<String>::None },
    )
    .await
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
pub async fn create(
    db: &State<Db>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    form: Form<ProjectForm>,
) -> Result<Redirect, Tmpl> {
    save(db, settings, cache, form.into_inner()).await
}

async fn save(
    db: &Db,
    settings: &Settings,
    cache: &DiffCache,
    form: ProjectForm,
) -> Result<Redirect, Tmpl> {
    let path = expand(form.path.trim());

    // NB: a helper rather than the closure this was. Putting the modal back up
    // is a render, a render reads the board behind it, and that awaits.
    async fn reject(
        db: &Db,
        settings: &Settings,
        cache: &DiffCache,
        board: Option<i64>,
        path: &str,
        message: String,
    ) -> Tmpl {
        Shell {
            db,
            settings,
            cache,
        }
        .render(
            board::current(db, board).await,
            board::ADD_PROJECT,
            context! { path => path.to_owned(), error => Some(message) },
        )
        .await
    }

    let typed = form.path.clone();
    if !path.is_dir() {
        let message = format!("{} is not a directory", path.display());
        return Err(reject(db, settings, cache, form.board, &typed, message).await);
    }
    if !is_git_repo(&path) {
        let message = format!("{} is not a git repository", path.display());
        return Err(reject(db, settings, cache, form.board, &typed, message).await);
    }

    let path = path.canonicalize().unwrap_or(path);

    let saved = Project::upsert(db, &path).await;
    let name = form
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty());

    if let (Ok(id), Some(name)) = (&saved, name) {
        Project::rename(db, *id, name).await;
    }

    match saved {
        Ok(id) => Ok(Redirect::to(format!("/projects/{id}"))),
        Err(err) => {
            let message = format!("could not save project: {err}");
            Err(reject(db, settings, cache, form.board, &typed, message).await)
        }
    }
}

/// Server-rendered directory autocomplete. Returns just the `<ul>` fragment;
/// htmx swaps it in on every keystroke.
#[get("/projects/complete?<q>")]
pub async fn complete(q: Option<String>) -> Tmpl {
    // NB: a directory listing, on every keystroke. Small, but it is a syscall
    // on a path the user is still typing — which can be a mount that is slow to
    // answer, or one that is not answering at all.
    let q = q.unwrap_or_default();
    Tmpl(
        "_completions.html",
        context! { entries => complete_path(&q).await },
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

async fn complete_path(query: &str) -> Vec<Completion> {
    let raw = if query.trim().is_empty() {
        default_root()
    } else {
        query.to_owned()
    };

    let (dir, prefix) = split_query(&raw);
    let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
        return Vec::new();
    };

    let wanted = prefix.to_lowercase();
    let mut listed = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.file_type().await.is_ok_and(|t| t.is_dir()) {
            listed.push(entry);
        }
    }

    let mut out: Vec<Completion> = listed
        .into_iter()
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

    #[tokio::test]
    async fn completion_finds_directories_and_flags_repositories() {
        let root = tempdir("completion-lists");
        std::fs::create_dir_all(root.join("alpha/.git")).unwrap();
        std::fs::create_dir_all(root.join("beta")).unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::write(root.join("a-file"), "").unwrap();

        let found = complete_path(&format!("{}/", root.display())).await;
        let names: Vec<_> = found.iter().map(|c| c.name.as_str()).collect();

        // Files and dot-directories stay out; repositories are marked.
        assert_eq!(names, ["alpha", "beta"]);
        assert!(found[0].is_repo);
        assert!(!found[1].is_repo);
    }

    #[tokio::test]
    async fn completion_respects_a_typed_prefix() {
        let root = tempdir("completion-prefix");
        std::fs::create_dir_all(root.join("alpha")).unwrap();
        std::fs::create_dir_all(root.join("beta")).unwrap();

        let found = complete_path(&format!("{}/al", root.display())).await;
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "alpha");
    }

    #[tokio::test]
    async fn upsert_is_idempotent() {
        let db = memory_db().await;

        let first = Project::upsert(&db, Path::new("/srv/repo")).await.unwrap();
        let second = Project::upsert(&db, Path::new("/srv/repo")).await.unwrap();
        assert_eq!(first, second);

        let project = Project::find(&db, first).await.unwrap();
        assert_eq!(project.name, "repo");
        assert_eq!(Project::all(&db).await.len(), 1);
    }

    /// Per-test directory; tests run concurrently, so the name has to be unique.
    fn tempdir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("ledecky-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }
}
