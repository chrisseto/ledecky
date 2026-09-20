use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use minijinja::context;
use rocket::form::Form;
use rocket::http::Status;
use rocket::serde::Serialize;
use rocket::{get, post, State};

use crate::agent::{messaging, AgentManager};
use crate::config::Settings;
use crate::db::Db;
use crate::git;
use crate::project::{Card, Lane, Project};
use crate::review::comment::{format_review, Side};
use crate::review::diff::{Line, ParsedFile};
use crate::review::expand::Dir;
use crate::review::scope::Mode;
use crate::review::turn;
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

/// One anchor in the picker: a point in the card's history and the link that
/// selects it, keeping whichever mode is already on screen.
#[derive(Serialize)]
#[serde(crate = "rocket::serde")]
struct Choice {
    label: String,
    /// `live`, `commit`, `turn` or `base` — what colours the row.
    kind: &'static str,
    href: String,
    selected: bool,
}

/// One half of the just/since toggle beside the picker.
#[derive(Serialize)]
#[serde(crate = "rocket::serde")]
struct ModeChoice {
    label: &'static str,
    href: String,
    selected: bool,
    /// Nothing follows the worktree, and the base commit is not the card's work.
    available: bool,
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
    /// Which line has the compose box open, as `<path>#<side>:<line>`.
    ///
    /// Kept in the URL rather than in the DOM so an update re-renders the box
    /// where it already was, instead of the pane arriving without it.
    comment: Option<&'a str>,
}

#[get("/cards/<id>/diff?<scope>&<expand>&<comment>")]
pub fn diff_pane(
    db: &State<Db>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    id: i64,
    scope: Option<&str>,
    expand: Option<&str>,
    comment: Option<&str>,
) -> Result<Tmpl, Status> {
    let view = View {
        scope,
        expand,
        comment,
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
    // Nested in the board page, so the out-of-band copy of the review tab's
    // stat is left off — the markup it would update is in this same response.
    Ok(context! {
        standalone => false,
        ..pane(
            db,
            settings,
            cache,
            id,
            View {
                scope,
                ..View::default()
            },
        )?
    })
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

    // A collected card kept its rows but not its refs, so there is nothing left
    // to diff against — and it is off the board on purpose. Answering here
    // covers the drawer and every review route with it.
    if card.lane == Lane::GarbageCollected {
        return Err(Status::NotFound);
    }

    let turns = Turn::for_card(&conn, id);
    let viewed = Viewed::for_card(&conn, id);

    // A comment belongs to the point in history it was written against, so only
    // the range that ends there asks for it.
    let here = Comment::find_in_range(
        &conn,
        id,
        viewing_turn(&scope, &turns),
        scope.snapshot_turn().is_some(),
    )
    .map_err(|err| failed(id, "reading the comments", err))?;
    // The batch goes whole, so the count is card-wide even where the range is
    // not: a draft left on another one is never simply lost.
    let pending =
        Comment::draft_count(&conn, id).map_err(|err| failed(id, "counting the drafts", err))?;
    drop(conn);

    // Everything below shells out to git, so the lock is already back.
    let repo = project.repo();
    let worktree = card.worktree_path.as_ref().map(PathBuf::from);
    let head = turn::live_head(cache, settings, &repo, worktree.as_deref(), &card, &turns);
    let commits = match worktree.as_deref() {
        Some(worktree) => git::commits(&repo, &settings.base_ref(id), &head_of(worktree)),
        None => Vec::new(),
    };

    // Every link out of the pane is this same view with one thing changed,
    // built here so no template has to concatenate a query string.
    let scope_key = scope.key();
    let expand_key = expansion.key();
    let link = |scope: &str, expand: &str| {
        rocket::uri!(diff_pane(
            id = id,
            scope = Some(scope),
            expand = Some(expand),
            comment = Option::<&str>::None
        ))
        .to_string()
    };
    let opening = |expansion: &Expansion| link(&scope_key, &expansion.key());

    // `<path>#<side>:<line>` split back into what the comment form posts.
    let anchored = view.comment.and_then(|key| {
        let (path, anchor) = key.rsplit_once('#')?;
        let (side, line) = anchor.split_once(':')?;
        Some((path.to_owned(), side.to_owned(), line.to_owned()))
    });

    // What the card last had recorded of it, as a tree, so the worktree can be
    // compared against it.
    let settled = git::tree_of(
        &repo,
        turns
            .last()
            .map(|turn| turn.commit_sha.as_str())
            .unwrap_or(&settings.base_ref(id)),
    );

    let scopes: Vec<_> = Scope::menu(&turns, &commits, head.as_deref(), settled.as_deref())
        .into_iter()
        .map(|entry| {
            // Keep the mode across a change of anchor where it still means
            // something; the ends of the list each only offer one.
            let mode = match entry.anchor.offers(scope.mode) {
                true => scope.mode,
                false => scope.mode.other(),
            };
            let candidate = Scope {
                anchor: entry.anchor,
                mode,
            };

            Choice {
                label: entry.label,
                kind: entry.kind,
                // Changing the range renumbers nothing now that expansion is
                // keyed by path, so what is open survives the move.
                href: link(&candidate.key(), &expand_key),
                selected: candidate == scope,
            }
        })
        .collect();

    let modes: Vec<_> = Mode::ALL
        .iter()
        .map(|mode| {
            let candidate = Scope {
                anchor: scope.anchor.clone(),
                mode: *mode,
            };
            ModeChoice {
                label: mode.label(),
                href: link(&candidate.key(), &expand_key),
                selected: *mode == scope.mode,
                available: scope.anchor.offers(*mode),
            }
        })
        .collect();

    let showing = here.iter().filter(|c| c.is_draft()).count();
    let stranded = pending - showing as i64;
    let submitted = here.len() - showing;

    // Comments hang off `<file>#<side>:<line>` so a template lookup is one hit.
    let mut threads: HashMap<String, Vec<Comment>> = HashMap::new();
    for comment in &here {
        threads
            .entry(comment.anchor())
            .or_default()
            .push(comment.clone());
    }

    // The parse is independent of what is on screen and cached, so opening a
    // hunk is a re-slice rather than another run of git and delta.
    let range = scope.revisions(settings, id, &turns, &commits, head.as_deref());
    let files = match &range {
        Some((from, to)) => cache.get(&repo, from, to).map_err(|err| {
            error!("card {id}: diffing {from}..{to}: {err:#}");
            Status::InternalServerError
        })?,
        None => Default::default(),
    };

    // Only what the reader will actually find in that file: a badge over a
    // range that renders none of them would send them looking for nothing.
    let counts = |path: &str| here.iter().filter(|c| c.file_path == path).count();
    let tree = group(&files, &viewed, counts);

    let rendered: Vec<FileView> = files
        .iter()
        .enumerate()
        .map(|(index, file)| {
            let opened = expansion.file(&file.path);
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
                            &opening,
                            &expansion,
                            &file.path,
                            hunk.gaps.first,
                            Dir::Up,
                            hunk.gaps.above,
                        ),
                        below: gap(
                            &opening,
                            &expansion,
                            &file.path,
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
                show_href: opening(&expansion.showing(&file.path)),
                expand_all_href: folded.then(|| opening(&expansion.whole_file(&file.path))),
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
        card, tree, threads, scopes, modes, submitted, stranded, last_message, turn_note,
        drafts => pending,
        files => rendered,
        has_diff => !files.is_empty(),
        // Whether the card has any history at all to point the picker at — not
        // whether a turn has landed, since a dirty worktree has files to show
        // long before one does.
        has_history => range.is_some(),
        scope_label => scope.label(),
        scope_kind => scope.anchor.kind(),
        additions => totals.0,
        deletions => totals.1,
        standalone => true,
        scope => scope_key,
        expand => expand_key,
        // The same view with nothing being commented on: what a line links to
        // when its box is already open, and what closing one lands on.
        comment_base => link(&scope_key, &expand_key),
        comment => view.comment,
        comment_file => anchored.as_ref().map(|a| a.0.clone()),
        comment_side => anchored.as_ref().map(|a| a.1.clone()),
        comment_line => anchored.as_ref().map(|a| a.2.clone()),
        // Carries the open box, so an update redraws it rather than dropping it.
        source => rocket::uri!(diff_pane(
            id = id,
            scope = Some(&scope_key),
            expand = Some(&expand_key),
            comment = view.comment
        )).to_string(),
    })
}

/// A query that did not answer. Nothing the pane reads is optional, so there is
/// no half-rendered version of it worth serving.
fn failed(card_id: i64, what: &str, err: rusqlite::Error) -> Status {
    error!("card {card_id}: {what}: {err:#}");
    Status::InternalServerError
}

/// The turn a range ends at.
///
/// This is both what a comment written on that range belongs to and what
/// decides whether an existing one still has a place on it, so the write and
/// the read have to agree — a comment left while pinned to turn 1 that answered
/// to the latest turn would vanish the moment it was saved.
fn viewing_turn(scope: &Scope, turns: &[Turn]) -> Option<i64> {
    match scope.snapshot_turn() {
        Some(n) => turns.iter().find(|turn| turn.n == n).map(|turn| turn.id),
        // Every other range ends at the live head, which is the card as it is.
        None => turns.last().map(|turn| turn.id),
    }
}

/// What the agent's worktree has checked out, for listing the commits it made.
///
/// A detached worktree's `HEAD` is the only place its own commits are reachable
/// from — the turn refs are a parallel chain and never contain them.
fn head_of(worktree: &Path) -> String {
    git::run(worktree, &["rev-parse", "HEAD"]).unwrap_or_else(|_| "HEAD".into())
}

/// The expansion a button hands back, or nothing when that side is already open.
fn gap(
    link: &impl Fn(&Expansion) -> String,
    expansion: &Expansion,
    file: &str,
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
            comment: None,
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
        let turns = Turn::for_card(&conn, id);
        Comment::create(
            &conn,
            id,
            viewing_turn(&Scope::parse(Some(&form.scope)), &turns),
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
        comment: None,
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
        comment: None,
    };
    Ok(Tmpl("_review.html", pane(db, settings, cache, id, view)?))
}

/// Hands every draft comment to the agent as one message and marks them sent.
#[post("/cards/<id>/review", data = "<form>")]
pub fn submit_review(
    db: &State<Db>,
    manager: &State<Arc<AgentManager>>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    id: i64,
    form: Form<ViewForm>,
) -> Result<Tmpl, Status> {
    let scope = Scope::parse(Some(&form.scope));
    let (drafts, turn) = {
        let conn = db.lock();
        let turns = Turn::for_card(&conn, id);
        let drafts =
            Comment::drafts(&conn, id).map_err(|err| failed(id, "reading the drafts", err))?;
        (drafts, viewing_turn(&scope, &turns))
    };

    if !drafts.is_empty() {
        let inbox = manager
            .running(id)
            .and_then(|agent| agent.inbox())
            .ok_or(Status::Conflict)?;

        // NB: a dialog holding the terminal is no longer a reason this fails —
        // the session reads its inbox between tool calls. What is left is the
        // socket itself, so leave the drafts alone to be retried.
        let message = format_review(&drafts, &scope.label());
        if let Err(err) = messaging::send(&inbox, &message) {
            warn!("card {id}: sending the review failed: {err:#}");
            return Err(Status::Conflict);
        }

        Comment::mark_submitted(&db.lock(), id, turn);
    }

    Ok(Tmpl(
        "_review.html",
        pane(db, settings, cache, id, form.view())?,
    ))
}
