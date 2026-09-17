use std::collections::HashMap;

use minijinja::context;
use rocket::form::Form;
use rocket::http::Status;
use rocket::serde::Serialize;
use rocket::{get, post, State};

use crate::agent::Agents;
use crate::config::Settings;
use crate::db::Db;
use crate::project::{Card, Project};
use crate::review::comment::{format_review, Side};
use crate::review::diff::{Line, ParsedFile};
use crate::review::expand::Dir;
use crate::review::{Comment, DiffCache, Expansion, Scope, Turn, Viewed};
use crate::tmpl::Tmpl;

/// Lines one click of an expander opens up.
const STEP: usize = 10;

#[derive(Serialize)]
#[serde(crate = "rocket::serde")]
struct Choice {
    key: String,
    label: String,
    selected: bool,
}

/// One file in the tree beside the diff.
#[derive(Serialize)]
#[serde(crate = "rocket::serde")]
struct FileNode {
    path: String,
    name: String,
    additions: u32,
    deletions: u32,
    comments: usize,
    viewed: bool,
    selected: bool,
}

/// The files of one directory, so the tree can fold a directory away.
#[derive(Serialize)]
#[serde(crate = "rocket::serde")]
struct Group {
    dir: String,
    files: Vec<FileNode>,
}

/// Lines still folded away on one side of a hunk, and the expansions that would
/// take a bite out of them or open them entirely.
#[derive(Serialize)]
#[serde(crate = "rocket::serde")]
struct Gap {
    lines: usize,
    step: usize,
    step_key: String,
    all_key: String,
}

#[derive(Serialize)]
#[serde(crate = "rocket::serde")]
struct HunkView {
    header: String,
    lines: Vec<Line>,
    above: Option<Gap>,
    below: Option<Gap>,
}

/// What the pane is currently showing. Carried on every form and link so a
/// re-render after a comment lands back on the same view.
#[derive(Debug, Clone, Copy, Default)]
struct View<'a> {
    scope: Option<&'a str>,
    file: Option<&'a str>,
    expand: Option<&'a str>,
}

#[get("/cards/<id>/diff?<scope>&<file>&<expand>")]
pub fn diff_pane(
    db: &State<Db>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    id: i64,
    scope: Option<&str>,
    file: Option<&str>,
    expand: Option<&str>,
) -> Result<Tmpl, Status> {
    let view = View {
        scope,
        file,
        expand,
    };
    Ok(Tmpl("_review.html", pane(db, settings, cache, id, view)?))
}

/// Everything `_review.html` needs, for the drawer's first render.
pub fn initial(
    db: &Db,
    settings: &Settings,
    cache: &DiffCache,
    id: i64,
    scope: Option<&str>,
) -> Result<minijinja::Value, Status> {
    pane(
        db,
        settings,
        cache,
        id,
        View {
            scope,
            ..View::default()
        },
    )
}

fn pane(
    db: &Db,
    settings: &Settings,
    cache: &DiffCache,
    id: i64,
    view: View<'_>,
) -> Result<minijinja::Value, Status> {
    let scope = Scope::parse(view.scope);
    let expansion = Expansion::parse(view.expand);

    let conn = db.lock();
    let card = Card::find(&conn, id).ok_or(Status::NotFound)?;
    let project = Project::find(&conn, card.project_id).ok_or(Status::NotFound)?;
    let turns = Turn::for_card(&conn, id);
    let comments = Comment::for_card(&conn, id);
    let viewed = Viewed::for_card(&conn, id);
    drop(conn);

    let scopes: Vec<_> = Scope::menu(&turns)
        .into_iter()
        .map(|candidate| Choice {
            key: candidate.key(),
            label: candidate.label(),
            selected: candidate == scope,
        })
        .collect();

    let drafts = comments.iter().filter(|c| c.is_draft()).count();
    let submitted = comments.len() - drafts;

    // Comments hang off `<file>#<side>:<line>` so a template lookup is one hit.
    let mut threads: HashMap<String, Vec<Comment>> = HashMap::new();
    for comment in &comments {
        threads
            .entry(comment.anchor())
            .or_default()
            .push(comment.clone());
    }

    // The parse is independent of what is on screen and cached, so selecting a
    // file or opening a hunk is a re-slice rather than another run of git and
    // delta.
    let files = match scope.revisions(settings, id, &turns) {
        Some((from, to)) => cache.get(&project.repo(), &from, &to).map_err(|err| {
            error!("card {id}: diffing {from}..{to}: {err:#}");
            Status::InternalServerError
        })?,
        None => Default::default(),
    };

    // A file named in the query wins, but only while it is still in the diff:
    // narrowing the scope can drop the file that was open.
    let selected = view
        .file
        .filter(|path| files.iter().any(|file| file.path == *path))
        .map(str::to_owned)
        .or_else(|| files.first().map(|file| file.path.clone()));

    let counts = |path: &str| comments.iter().filter(|c| c.file_path == path).count();
    let tree = group(&files, selected.as_deref(), &viewed, counts);

    let open = selected
        .as_deref()
        .and_then(|path| files.iter().find(|file| file.path == path));
    let collapsed = open.is_some_and(|file| viewed.contains(&file.path));

    // A viewed file is ticked off, so its diff is folded away entirely until the
    // tick comes off again.
    let hunks: Vec<HunkView> = match open.filter(|_| !collapsed) {
        Some(file) => file
            .hunks(&expansion)
            .hunks
            .into_iter()
            .map(|hunk| HunkView {
                header: hunk.header,
                lines: hunk.lines,
                above: gap(&expansion, hunk.gaps.first, Dir::Up, hunk.gaps.above),
                below: gap(&expansion, hunk.gaps.last, Dir::Down, hunk.gaps.below),
            })
            .collect(),
        None => Vec::new(),
    };

    let folded = hunks
        .iter()
        .any(|hunk| hunk.above.is_some() || hunk.below.is_some());

    // The agent's closing words for the most recent turn — how a failed merge or
    // an unanswered question surfaces outside the terminal.
    let last_message = turns
        .last()
        .and_then(|t| t.last_assistant_message.clone())
        .filter(|m| !m.trim().is_empty());

    let turn_note = turns.last().map(|turn| {
        format!(
            "Turn {} — {}",
            turn.n,
            turn.commit_sha.chars().take(7).collect::<String>()
        )
    });

    Ok(context! {
        card, tree, hunks, threads, scopes, drafts, submitted, last_message, turn_note,
        file => open.map(|file| context! {
            path => file.path.clone(),
            additions => file.additions,
            deletions => file.deletions,
            binary => file.binary,
            collapsed,
        }),
        can_expand_all => folded,
        scope => scope.key(),
        expand => expansion.key(),
    })
}

/// The expansion a button hands back, or nothing when that side is already open.
fn gap(expansion: &Expansion, hunk: usize, dir: Dir, lines: usize) -> Option<Gap> {
    (lines > 0).then(|| Gap {
        lines,
        step: STEP.min(lines),
        step_key: expansion.plus(hunk, dir, STEP.min(lines)).key(),
        all_key: expansion.plus(hunk, dir, lines).key(),
    })
}

/// The diff's files as a tree: one group per directory, in the order the diff
/// lists them.
fn group(
    files: &[ParsedFile],
    selected: Option<&str>,
    viewed: &std::collections::HashSet<String>,
    comments: impl Fn(&str) -> usize,
) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();

    for file in files {
        let (dir, name) = match file.path.rsplit_once('/') {
            Some((dir, name)) => (dir, name),
            None => ("", file.path.as_str()),
        };

        let node = FileNode {
            path: file.path.clone(),
            name: name.to_owned(),
            additions: file.additions,
            deletions: file.deletions,
            comments: comments(&file.path),
            viewed: viewed.contains(&file.path),
            selected: selected == Some(file.path.as_str()),
        };

        match groups.iter_mut().find(|group| group.dir == dir) {
            Some(group) => group.files.push(node),
            None => groups.push(Group {
                dir: dir.to_owned(),
                files: vec![node],
            }),
        }
    }
    groups
}

/// The view a form is submitted from, so the re-render matches what was on screen.
#[derive(rocket::FromForm)]
pub struct ViewForm {
    #[field(default = String::new())]
    scope: String,
    file: Option<String>,
    expand: Option<String>,
}

impl ViewForm {
    fn view(&self) -> View<'_> {
        View {
            scope: Some(&self.scope),
            file: self.file.as_deref(),
            expand: self.expand.as_deref(),
        }
    }
}

#[derive(rocket::FromForm)]
pub struct CommentForm {
    file_path: String,
    side: String,
    line: i64,
    body: String,
    #[field(default = String::new())]
    scope: String,
    file: Option<String>,
    expand: Option<String>,
}

#[post("/cards/<id>/comments", data = "<form>")]
pub fn add_comment(
    db: &State<Db>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    id: i64,
    form: Form<CommentForm>,
) -> Result<Tmpl, Status> {
    let body = form.body.trim();
    if !body.is_empty() {
        let conn = db.lock();
        let turn_id = Turn::latest_id(&conn, id);
        Comment::create(
            &conn,
            id,
            turn_id,
            &form.file_path,
            form.line,
            Side::parse(&form.side),
            body,
        )
        .map_err(|_| Status::InternalServerError)?;
    }

    let view = View {
        scope: Some(&form.scope),
        file: form.file.as_deref(),
        expand: form.expand.as_deref(),
    };
    Ok(Tmpl("_review.html", pane(db, settings, cache, id, view)?))
}

#[post("/cards/<id>/comments/<comment_id>/delete", data = "<form>")]
pub fn delete_comment(
    db: &State<Db>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    id: i64,
    comment_id: i64,
    form: Form<ViewForm>,
) -> Result<Tmpl, Status> {
    Comment::delete_draft(&db.lock(), id, comment_id);
    Ok(Tmpl(
        "_review.html",
        pane(db, settings, cache, id, form.view())?,
    ))
}

/// Throws away every comment not yet sent, for when a review is reconsidered
/// wholesale.
#[post("/cards/<id>/comments/discard", data = "<form>")]
pub fn discard_comments(
    db: &State<Db>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    id: i64,
    form: Form<ViewForm>,
) -> Result<Tmpl, Status> {
    Comment::delete_drafts(&db.lock(), id);
    Ok(Tmpl(
        "_review.html",
        pane(db, settings, cache, id, form.view())?,
    ))
}

#[derive(rocket::FromForm)]
pub struct ViewedForm {
    file_path: String,
    #[field(default = String::new())]
    scope: String,
}

/// Ticks a file off, or puts it back. The pane re-renders around the tick, so
/// whatever hunks were open are deliberately forgotten.
#[post("/cards/<id>/viewed", data = "<form>")]
pub fn toggle_viewed(
    db: &State<Db>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    id: i64,
    form: Form<ViewedForm>,
) -> Result<Tmpl, Status> {
    Viewed::toggle(&db.lock(), id, &form.file_path);

    let view = View {
        scope: Some(&form.scope),
        file: Some(&form.file_path),
        expand: None,
    };
    Ok(Tmpl("_review.html", pane(db, settings, cache, id, view)?))
}

/// Hands every draft comment to the agent as one message and marks them sent.
#[post("/cards/<id>/review", data = "<form>")]
pub fn submit_review(
    db: &State<Db>,
    agents: &State<Agents>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    id: i64,
    form: Form<ViewForm>,
) -> Result<Tmpl, Status> {
    let drafts = Comment::drafts(&db.lock(), id);

    if !drafts.is_empty() {
        let agent = agents
            .get(id)
            .filter(|a| a.is_running())
            .ok_or(Status::Conflict)?;

        let scope = Scope::parse(Some(&form.scope));
        // A busy terminal means a modal is up; leave the drafts alone to retry.
        if !agent.inject(&format_review(&drafts, &scope.label())) {
            return Err(Status::Conflict);
        }

        Comment::mark_submitted(&db.lock(), id);
    }

    Ok(Tmpl(
        "_review.html",
        pane(db, settings, cache, id, form.view())?,
    ))
}
