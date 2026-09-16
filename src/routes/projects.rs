use std::path::{Path, PathBuf};

use minijinja::context;
use rocket::form::Form;
use rocket::response::Redirect;
use rocket::{get, post, State};

use crate::db::Db;
use crate::queries;
use crate::tmpl::Tmpl;

#[get("/")]
pub fn index(db: &State<Db>) -> Tmpl {
    let projects = queries::projects(&db.lock());
    Tmpl("projects.html", context! { projects })
}

#[get("/projects/new")]
pub fn new() -> Tmpl {
    Tmpl(
        "project_new.html",
        context! { path => default_root(), error => Option::<String>::None },
    )
}

#[derive(rocket::FromForm)]
pub struct ProjectForm {
    path: String,
}

#[post("/projects", data = "<form>")]
pub fn create(db: &State<Db>, form: Form<ProjectForm>) -> Result<Redirect, Tmpl> {
    let path = expand(form.path.trim());

    let reject = |msg: String| {
        Tmpl(
            "project_new.html",
            context! { path => form.path.clone(), error => Some(msg) },
        )
    };

    if !path.is_dir() {
        return Err(reject(format!("{} is not a directory", path.display())));
    }
    if !is_git_repo(&path) {
        return Err(reject(format!("{} is not a git repository", path.display())));
    }

    let path = path.canonicalize().unwrap_or(path);
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("project")
        .to_owned();

    let conn = db.lock();
    let id = match conn.query_row(
        "SELECT id FROM projects WHERE path = ?1",
        [path.to_string_lossy()],
        |r| r.get::<_, i64>(0),
    ) {
        Ok(existing) => existing,
        Err(_) => {
            conn.execute(
                "INSERT INTO projects (name, path) VALUES (?1, ?2)",
                rusqlite::params![name, path.to_string_lossy()],
            )
            .map_err(|e| reject(format!("could not save project: {e}")))?;
            conn.last_insert_rowid()
        }
    };

    Ok(Redirect::to(format!("/projects/{id}")))
}

/// Server-rendered directory autocomplete. Returns just the `<ul>` fragment;
/// unpoly swaps it in on every keystroke.
#[get("/projects/complete?<q>")]
pub fn complete(q: Option<String>) -> Tmpl {
    let q = q.unwrap_or_default();
    Tmpl("_completions.html", context! { entries => complete_path(&q) })
}

#[derive(serde::Serialize)]
pub struct Completion {
    path: String,
    name: String,
    is_repo: bool,
}

fn complete_path(query: &str) -> Vec<Completion> {
    let raw = if query.trim().is_empty() {
        default_root()
    } else {
        query.to_owned()
    };
    let expanded = expand(&raw);

    // A trailing slash means "list inside this directory"; otherwise the last
    // segment is a prefix to filter the parent's entries by.
    let (dir, prefix) = if raw.ends_with('/') || expanded.is_dir() && raw.is_empty() {
        (expanded.clone(), String::new())
    } else {
        match (expanded.parent(), expanded.file_name()) {
            (Some(parent), Some(name)) => {
                (parent.to_path_buf(), name.to_string_lossy().into_owned())
            }
            _ => (expanded.clone(), String::new()),
        }
    };

    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut out: Vec<Completion> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            // Hidden directories only surface once the user types the dot.
            if name.starts_with('.') && !prefix.starts_with('.') {
                return None;
            }
            if !name.to_lowercase().starts_with(&prefix.to_lowercase()) {
                return None;
            }
            let path = e.path();
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

fn is_git_repo(path: &Path) -> bool {
    // `.git` is a directory in a normal clone and a file in a linked worktree.
    path.join(".git").exists()
}

fn expand(path: &str) -> PathBuf {
    match path.strip_prefix('~') {
        Some(rest) => {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(format!("{home}{rest}"))
        }
        None => PathBuf::from(path),
    }
}

fn default_root() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    format!("{home}/")
}
