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

/// Rendered lines beyond which a file is held back behind a button.
///
/// Every file in the range is on the page at once and the whole pane re-renders
/// on each comment, expansion and tick, so one regenerated lockfile would
/// otherwise put megabytes on the wire every time.
const MAX_LINES: usize = 2000;

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
    index: usize,
    path: String,
    name: String,
    additions: u32,
    deletions: u32,
    comments: usize,
    viewed: bool,
}

/// The files of one directory, so the tree can fold a directory away.
#[derive(Serialize)]
#[serde(crate = "rocket::serde")]
struct Group {
    dir: String,
    files: Vec<FileNode>,
}

/// Lines still folded away on one side of a hunk, and the views that would take
/// a bite out of them or open them entirely.
#[derive(Serialize)]
#[serde(crate = "rocket::serde")]
struct Gap {
    lines: usize,
    step: usize,
    step_href: String,
    all_href: String,
}

#[derive(Serialize)]
#[serde(crate = "rocket::serde")]
struct HunkView {
    header: String,
    lines: Vec<Line>,
    above: Option<Gap>,
    below: Option<Gap>,
}

/// One file's diff, as the stacked pane renders it.
#[derive(Serialize)]
#[serde(crate = "rocket::serde")]
struct FileView {
    index: usize,
    path: String,
    old_path: Option<String>,
    additions: u32,
    deletions: u32,
    binary: bool,
    /// Ticked off, so the diff is folded away until the tick comes off.
    viewed: bool,
    /// Lines this file would render, when that is enough to hold it back.
    held_back: Option<usize>,
    /// Asks for a held-back file anyway.
    show_href: String,
    /// Opens every hunk, when something is still folded.
    expand_all_href: Option<String>,
    hunks: Vec<HunkView>,
}

/// What the pane is currently showing. Carried on every form and link so a
/// re-render after a comment lands back on the same view.
#[derive(Debug, Clone, Copy, Default)]
struct View<'a> {
    scope: Option<&'a str>,
    expand: Option<&'a str>,
}

#[get("/cards/<id>/diff?<scope>&<expand>")]
pub fn diff_pane(
    db: &State<Db>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    id: i64,
    scope: Option<&str>,
    expand: Option<&str>,
) -> Result<Tmpl, Status> {
    let view = View { scope, expand };
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

    // The parse is independent of what is on screen and cached, so opening a
    // hunk is a re-slice rather than another run of git and delta.
    let files = match scope.revisions(settings, id, &turns) {
        Some((from, to)) => cache.get(&project.repo(), &from, &to).map_err(|err| {
            error!("card {id}: diffing {from}..{to}: {err:#}");
            Status::InternalServerError
        })?,
        None => Default::default(),
    };

    let counts = |path: &str| comments.iter().filter(|c| c.file_path == path).count();
    let tree = group(&files, &viewed, counts);

    // Every link out of the pane is this same view with one more thing opened,
    // built here so no template has to concatenate a query string.
    let scope_key = scope.key();
    let link = |expansion: &Expansion| {
        rocket::uri!(diff_pane(
            id = id,
            scope = Some(scope_key.as_str()),
            expand = Some(expansion.key())
        ))
        .to_string()
    };

    let rendered: Vec<FileView> = files
        .iter()
        .enumerate()
        .map(|(index, file)| {
            let opened = expansion.file(index);
            let diff = file.hunks(&opened);
            let length: usize = diff.hunks.iter().map(|hunk| hunk.lines.len()).sum();

            let ticked = viewed.contains(&file.path);
            // Asking for a big file once is enough; the expansion carries it.
            let held_back = (!ticked && !opened.shown() && length > MAX_LINES).then_some(length);

            let hunks: Vec<HunkView> = match ticked || held_back.is_some() || file.binary {
                true => Vec::new(),
                false => diff
                    .hunks
                    .into_iter()
                    .map(|hunk| HunkView {
                        header: hunk.header,
                        above: gap(
                            &link,
                            &expansion,
                            index,
                            hunk.gaps.first,
                            Dir::Up,
                            hunk.gaps.above,
                        ),
                        below: gap(
                            &link,
                            &expansion,
                            index,
                            hunk.gaps.last,
                            Dir::Down,
                            hunk.gaps.below,
                        ),
                        lines: hunk.lines,
                    })
                    .collect(),
            };

            let folded = hunks
                .iter()
                .any(|hunk| hunk.above.is_some() || hunk.below.is_some());

            FileView {
                index,
                path: file.path.clone(),
                old_path: file.old_path.clone(),
                additions: file.additions,
                deletions: file.deletions,
                binary: file.binary,
                viewed: ticked,
                held_back,
                show_href: link(&expansion.showing(index)),
                expand_all_href: folded.then(|| link(&expansion.whole_file(index))),
                hunks,
            }
        })
        .collect();

    let totals = (
        files.iter().map(|file| file.additions).sum::<u32>(),
        files.iter().map(|file| file.deletions).sum::<u32>(),
    );

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
        card, tree, threads, scopes, drafts, submitted, last_message, turn_note,
        files => rendered,
        has_diff => !files.is_empty(),
        has_turns => !turns.is_empty(),
        additions => totals.0,
        deletions => totals.1,
        scope => scope.key(),
        expand => expansion.key(),
    })
}

/// The expansion a button hands back, or nothing when that side is already open.
fn gap(
    link: &impl Fn(&Expansion) -> String,
    expansion: &Expansion,
    file: usize,
    hunk: usize,
    dir: Dir,
    lines: usize,
) -> Option<Gap> {
    (lines > 0).then(|| Gap {
        lines,
        step: STEP.min(lines),
        step_href: link(&expansion.plus(file, hunk, dir, STEP.min(lines))),
        all_href: link(&expansion.plus(file, hunk, dir, lines)),
    })
}

/// The diff's files as a tree: one group per directory, in the order the diff
/// lists them.
fn group(
    files: &[ParsedFile],
    viewed: &std::collections::HashSet<String>,
    comments: impl Fn(&str) -> usize,
) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();

    for (index, file) in files.iter().enumerate() {
        let (dir, name) = match file.path.rsplit_once('/') {
            Some((dir, name)) => (dir, name),
            None => ("", file.path.as_str()),
        };

        let node = FileNode {
            index,
            path: file.path.clone(),
            name: name.to_owned(),
            additions: file.additions,
            deletions: file.deletions,
            comments: comments(&file.path),
            viewed: viewed.contains(&file.path),
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
    expand: Option<String>,
}

impl ViewForm {
    fn view(&self) -> View<'_> {
        View {
            scope: Some(&self.scope),
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
    expand: Option<String>,
}

/// Ticks a file off, or puts it back.
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
        expand: form.expand.as_deref(),
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
