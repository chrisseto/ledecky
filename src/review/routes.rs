use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use minijinja::context;
use rocket::form::Form;
use rocket::http::Status;
use rocket::serde::Serialize;
use rocket::tokio::task::spawn_blocking;
use rocket::{get, post, State};

use crate::agent::{messaging, AgentManager};
use crate::config::Settings;
use crate::db::Db;
use crate::git;
use crate::project::{Card, Lane, Project};
use crate::review::comment::{format_review, Side};
use crate::review::diff::{Line, ParsedFile, Segment};
use crate::review::scope::Mode;
use crate::review::turn;
use crate::review::{Comment, DiffCache, Expansion, Scope, Turn, Viewed};
use crate::tmpl::Tmpl;

/// Rendered lines beyond which a file is held back behind a button.
///
/// Every file in the range is on the page at once, and an expansion still
/// re-renders the whole pane, so one regenerated lockfile would otherwise put
/// megabytes on the wire for a change that touched one hunk elsewhere.
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
/// One stretch of a file's diff, tagged `kind` for the template to switch on.
#[derive(Serialize)]
#[serde(crate = "rocket::serde", tag = "kind", rename_all = "snake_case")]
enum SegmentView {
    Lines { lines: Vec<Line> },
    Fold { links: Vec<FoldLink> },
}

#[derive(Serialize)]
#[serde(crate = "rocket::serde")]
struct FoldLink {
    label: String,
    href: String,
    icon: Option<&'static str>,
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
    segments: Vec<SegmentView>,
}

/// One of the agent's commits in the range, its message laid out as lines a
/// comment can hang off.
#[derive(Serialize)]
#[serde(crate = "rocket::serde")]
struct CommitView {
    sha: String,
    short: String,
    lines: Vec<MessageLine>,
}

/// A message line, shaped like a diff `Line` so the template draws both alike.
/// Plain `text` rather than `html`, which the template escapes itself.
#[derive(Serialize)]
#[serde(crate = "rocket::serde")]
struct MessageLine {
    kind: &'static str,
    new_line: u32,
    text: String,
    anchor: String,
}

impl CommitView {
    /// Numbered as `git log` prints the message, so a line number in the review
    /// is one the agent can find.
    fn new(commit: &git::Commit) -> Self {
        let lines = commit
            .message
            .lines()
            .zip(1u32..)
            .map(|(text, n)| MessageLine {
                kind: "message",
                new_line: n,
                text: text.to_owned(),
                anchor: format!("{}:{n}", Side::Message.as_str()),
            })
            .collect();

        Self {
            sha: commit.sha.clone(),
            short: commit.sha.chars().take(7).collect(),
            lines,
        }
    }
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
pub async fn diff_pane(
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
    Ok(Tmpl(
        "_review.html",
        pane(db, settings, cache, id, view).await?,
    ))
}

/// Everything `_review.html` needs, for the drawer's first render.
pub async fn initial(
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
        ).await?
    })
}

async fn pane(
    db: &Db,
    settings: &Settings,
    cache: &DiffCache,
    id: i64,
    view: View<'_>,
) -> Result<minijinja::Value, Status> {
    let scope = Scope::parse(view.scope);
    let expansion = Expansion::parse(view.expand);

    let card = Card::find(db, id).await.ok_or(Status::NotFound)?;
    let project = Project::find(db, card.project_id)
        .await
        .ok_or(Status::NotFound)?;

    // A collected card kept its rows but not its refs, so there is nothing left
    // to diff against — and it is off the board on purpose. Answering here
    // covers the drawer and every review route with it.
    if card.lane == Lane::GarbageCollected {
        return Err(Status::NotFound);
    }

    let turns = Turn::for_card(db, id).await;
    let viewed = Viewed::for_card(db, id).await;
    let repo = project.repo();

    // Where the base stands now, which says whether the card's newest turns are
    // in the era its worktree is. A range measured from one that is not would
    // reach across a rebase, so the key off the query string is settled onto one
    // that does not before anything — the comments below included — reads it.
    let base_at = git::rev_parse(&repo, git::Rev::Ref(&settings.base_ref(id))).await;
    let scope = scope.settle(&turns, base_at.as_deref());

    // A comment belongs to the point in history it was written against, so only
    // the range that ends there asks for it.
    let here = Comment::find_in_range(
        db,
        id,
        viewing_turn(&scope, &turns),
        scope.snapshot_turn().is_some(),
    )
    .await
    .map_err(|err| failed(id, "reading the comments", err))?;
    // The batch goes whole, so the count is card-wide even where the range is
    // not: a draft left on another one is never simply lost.
    let pending = Comment::draft_count(db, id)
        .await
        .map_err(|err| failed(id, "counting the drafts", err))?;

    let worktree = card.worktree_path.as_ref().map(PathBuf::from);
    let head = turn::live_head(
        cache,
        settings,
        &repo,
        worktree.as_deref(),
        &card,
        &turns,
        turn::Freshness::Fresh,
    )
    .await;
    let commits = match worktree.as_deref() {
        Some(worktree) => {
            git::commits(&repo, &settings.base_ref(id), &head_of(worktree).await).await
        }
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
    )
    .await;

    let scopes: Vec<_> = Scope::menu(
        &turns,
        &commits,
        head.as_deref(),
        settled.as_deref(),
        base_at.as_deref(),
    )
    .into_iter()
    .map(|entry| {
        // Keep the mode across a change of anchor where it still means
        // something; the ends of the list each only offer one, and a turn
        // whose `since` would cross a rebase offers only the other.
        let candidate = Scope {
            anchor: entry.anchor,
            mode: scope.mode,
        }
        .settle(&turns, base_at.as_deref());

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
                available: candidate.offers(&turns, base_at.as_deref()),
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
    let range = scope.revisions(
        settings,
        id,
        &turns,
        &commits,
        head.as_deref(),
        base_at.as_deref(),
    );
    let files = match &range {
        Some((from, to)) => cache.get(&repo, from, to).await.map_err(|err| {
            error!("card {id}: diffing {from}..{to}: {err:#}");
            Status::InternalServerError
        })?,
        None => Default::default(),
    };

    // Only what the reader will actually find in that file: a badge over a
    // range that renders none of them would send them looking for nothing.
    let counts = |path: &str| here.iter().filter(|c| c.file_path == path).count();
    let tree = group(&files, &viewed, counts);

    // NB: `range` gates this too — with no head there is nothing on screen for
    // a message to describe.
    let messages: Vec<CommitView> = match range {
        Some(_) => scope
            .commits_in(&turns, &commits)
            .into_iter()
            .map(CommitView::new)
            .collect(),
        None => Vec::new(),
    };
    let message_comments: usize = messages.iter().map(|m| counts(&m.sha)).sum();

    let rendered: Vec<FileView> = files
        .iter()
        .enumerate()
        .map(|(index, file)| {
            let opened = expansion.file(&file.path);
            let diff = file.window(&opened);
            let length = diff.lines().count();

            let ticked = viewed.contains(&file.path);
            // Asking for a big file once is enough; the expansion carries it.
            let held_back = (!ticked && !opened.shown() && length > MAX_LINES).then_some(length);

            // NB: a ticked file is still rendered in full. It is a closed
            // `<details>`, so the browser neither lays it out nor paints it,
            // and the reader gets it back without asking the server — which is
            // the point. Eliding it here would make reopening a round trip.
            let segments: Vec<SegmentView> = match held_back.is_some() || file.binary {
                true => Vec::new(),
                false => diff
                    .segments
                    .into_iter()
                    .map(|segment| match segment {
                        Segment::Lines(lines) => SegmentView::Lines { lines },
                        Segment::Fold(fold) => SegmentView::Fold {
                            links: fold
                                .openings()
                                .into_iter()
                                .map(|o| FoldLink {
                                    href: opening(
                                        &expansion.plus(&file.path, o.hunk, o.dir, o.lines),
                                    ),
                                    label: o.label,
                                    icon: o.icon,
                                })
                                .collect(),
                        },
                    })
                    .collect(),
            };

            let folded = segments
                .iter()
                .any(|segment| matches!(segment, SegmentView::Fold { .. }));

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
                segments,
            }
        })
        .collect();

    let totals = (
        files.iter().map(|file| file.additions).sum::<u32>(),
        files.iter().map(|file| file.deletions).sum::<u32>(),
    );

    Ok(context! {
        card, tree, threads, scopes, modes, submitted, stranded,
        messages, message_comments,
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
        // Closing the box swaps its own block, so this is the one link out of
        // the pane that does not lead back to the pane.
        cancel_href => view.comment
            .map(|key| anchored_href(id, key, false, &scope_key, Some(&expand_key))),
        // Carries the open box, so an update redraws it rather than dropping it.
        source => rocket::uri!(diff_pane(
            id = id,
            scope = Some(&scope_key),
            expand = Some(&expand_key),
            comment = view.comment
        ))
        .to_string(),
    })
}

/// A query that did not answer. Nothing the pane reads is optional, so there is
/// no half-rendered version of it worth serving.
fn failed(card_id: i64, what: &str, err: sqlx::Error) -> Status {
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
async fn head_of(worktree: &Path) -> String {
    git::run(worktree, &["rev-parse", "HEAD"])
        .await
        .unwrap_or_else(|_| "HEAD".into())
}

/// The diff's files as a tree: one group per directory, in the order the diff
/// lists them.
fn group(
    files: &[Arc<ParsedFile>],
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

/// The comment counts a fragment has to report, without going near the diff.
///
/// Everything here comes out of the database and the card's turns, so a comment
/// landing costs no `git diff` and no delta — the point of answering with a
/// fragment rather than the pane.
struct Counts {
    here: Vec<Comment>,
    drafts: i64,
    stranded: i64,
    submitted: usize,
}

async fn counts_for(db: &Db, id: i64, scope_key: &str) -> Result<Counts, Status> {
    let scope = Scope::parse(Some(scope_key));
    let turns = Turn::for_card(db, id).await;
    let here = Comment::find_in_range(
        db,
        id,
        viewing_turn(&scope, &turns),
        scope.snapshot_turn().is_some(),
    )
    .await
    .map_err(|err| failed(id, "reading the comments", err))?;
    let drafts = Comment::draft_count(db, id)
        .await
        .map_err(|err| failed(id, "counting the drafts", err))?;

    let showing = here.iter().filter(|c| c.is_draft()).count();
    Ok(Counts {
        drafts,
        stranded: drafts - showing as i64,
        submitted: here.len() - showing,
        here,
    })
}

/// `<path>#<side>:<line>` split back into what the comment form posts.
fn split_key(key: &str) -> Option<(String, String, String)> {
    let (path, anchor) = key.rsplit_once('#')?;
    let (side, line) = anchor.split_once(':')?;
    Some((path.to_owned(), side.to_owned(), line.to_owned()))
}

/// One line's comments, and the box when it is being written in.
///
/// Rendered on its own so opening, saving, cancelling and removing each swap
/// the block they are about rather than the whole pane — see `_anchored.html`.
fn anchored(
    id: i64,
    key: &str,
    open: bool,
    scope: &str,
    expand: Option<&str>,
    counts: &Counts,
    source: Option<String>,
) -> minijinja::Value {
    let parts = split_key(key);
    let thread: Vec<Comment> = counts
        .here
        .iter()
        .filter(|c| c.anchor() == key)
        .cloned()
        .collect();
    let expand_key = expand.unwrap_or_default().to_owned();

    minijinja::context! {
        card => minijinja::context! { id => id },
        key => key,
        open => open,
        thread => thread,
        comment_file => parts.as_ref().map(|p| p.0.clone()),
        comment_side => parts.as_ref().map(|p| p.1.clone()),
        comment_line => parts.as_ref().map(|p| p.2.clone()),
        scope => scope,
        expand => expand_key,
        cancel_href => anchored_href(id, key, false, scope, expand),
        source => source,
        drafts => counts.drafts,
        stranded => counts.stranded,
        submitted => counts.submitted,
    }
}

/// What the pane refetches when the stream says the diff moved.
///
/// Carries the open box, so a redraw brings it back rather than dropping it —
/// which is why a fragment that opens or closes one sends a new copy.
fn pane_source(id: i64, scope: &str, expand: Option<&str>, comment: Option<&str>) -> String {
    rocket::uri!(diff_pane(
        id = id,
        scope = Some(scope),
        expand = Some(expand.unwrap_or_default()),
        comment = comment,
    ))
    .to_string()
}

/// Built here rather than in a template, like every other link out of the pane.
fn anchored_href(id: i64, key: &str, open: bool, scope: &str, expand: Option<&str>) -> String {
    rocket::uri!(anchored_block(
        id = id,
        key = key,
        scope = Some(scope),
        expand = Some(expand.unwrap_or_default()),
        open = open.then_some(true),
    ))
    .to_string()
}

/// The box for one line, or the line's thread once it is closed again.
#[get("/cards/<id>/comments/at?<key>&<scope>&<expand>&<open>")]
pub async fn anchored_block(
    db: &State<Db>,
    id: i64,
    key: String,
    scope: Option<String>,
    expand: Option<String>,
    open: Option<bool>,
) -> Result<Tmpl, Status> {
    let scope = scope.unwrap_or_default();
    let open = open.unwrap_or(false);
    let counts = counts_for(db, id, &scope).await?;
    let source = pane_source(id, &scope, expand.as_deref(), open.then_some(key.as_str()));
    Ok(Tmpl(
        "_anchored_open.html",
        anchored(
            id,
            &key,
            open,
            &scope,
            expand.as_deref(),
            &counts,
            Some(source),
        ),
    ))
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
pub async fn add_comment(db: &State<Db>, id: i64, form: Form<CommentForm>) -> Result<Tmpl, Status> {
    let body = form.body.trim();
    if !body.is_empty() {
        let turns = Turn::for_card(db, id).await;
        Comment::create(
            db,
            id,
            viewing_turn(&Scope::parse(Some(&form.scope)), &turns),
            &form.file_path,
            form.line,
            Side::parse(&form.side),
            body,
        )
        .await
        .map_err(|_| Status::InternalServerError)?;
    }

    let key = format!("{}#{}:{}", form.file_path, form.side, form.line);
    let counts = counts_for(db, id, &form.scope).await?;
    let source = pane_source(id, &form.scope, form.expand.as_deref(), None);
    Ok(Tmpl(
        "_anchored_reply.html",
        anchored(
            id,
            &key,
            false,
            &form.scope,
            form.expand.as_deref(),
            &counts,
            Some(source),
        ),
    ))
}

#[derive(rocket::FromForm)]
pub struct DeleteCommentForm {
    key: String,
    #[field(default = String::new())]
    scope: String,
    expand: Option<String>,
}

#[post("/cards/<id>/comments/<comment_id>/delete", data = "<form>")]
pub async fn delete_comment(
    db: &State<Db>,
    id: i64,
    comment_id: i64,
    form: Form<DeleteCommentForm>,
) -> Result<Tmpl, Status> {
    Comment::delete_draft(db, id, comment_id).await;

    let counts = counts_for(db, id, &form.scope).await?;
    Ok(Tmpl(
        "_anchored_reply.html",
        anchored(
            id,
            &form.key,
            false,
            &form.scope,
            form.expand.as_deref(),
            &counts,
            None,
        ),
    ))
}

/// Throws away every comment not yet sent, for when a review is reconsidered
/// wholesale.
#[post("/cards/<id>/comments/discard", data = "<form>")]
pub async fn discard_comments(
    db: &State<Db>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    id: i64,
    form: Form<ViewForm>,
) -> Result<Tmpl, Status> {
    Comment::delete_drafts(db, id).await;
    Ok(Tmpl(
        "_review.html",
        pane(db, settings, cache, id, form.view()).await?,
    ))
}

#[derive(rocket::FromForm)]
pub struct ViewedForm {
    file_path: String,
    viewed: bool,
}

/// Records whether a file has been read.
///
/// Answers nothing on purpose. The fold already happened in the browser — this
/// is only what makes it outlive the page — so returning a pane would redraw
/// every line of every other file to say something the reader can already see.
#[post("/cards/<id>/viewed", data = "<form>")]
pub async fn toggle_viewed(db: &State<Db>, id: i64, form: Form<ViewedForm>) -> Status {
    Viewed::set(db, id, &form.file_path, form.viewed).await;
    Status::NoContent
}

/// Hands every draft comment to the agent as one message and marks them sent.
#[post("/cards/<id>/review", data = "<form>")]
pub async fn submit_review(
    db: &State<Db>,
    manager: &State<Arc<AgentManager>>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    id: i64,
    form: Form<ViewForm>,
) -> Result<Tmpl, Status> {
    let scope = Scope::parse(Some(&form.scope));
    let turns = Turn::for_card(db, id).await;
    let drafts = Comment::drafts(db, id)
        .await
        .map_err(|err| failed(id, "reading the drafts", err))?;
    let turn = viewing_turn(&scope, &turns);

    if !drafts.is_empty() {
        let inbox = manager
            .running(id)
            .and_then(|agent| agent.inbox())
            .ok_or(Status::Conflict)?;

        // NB: a dialog holding the terminal is no longer a reason this fails —
        // the session reads its inbox between tool calls. What is left is the
        // socket itself, so leave the drafts alone to be retried.
        //
        // NB: on the blocking pool. The inbox is a unix socket written under a
        // timeout, which blocks the thread it is on however short it is.
        let message = format_review(&drafts, &scope.label());
        let sent = spawn_blocking(move || messaging::send(&inbox, &message))
            .await
            .expect("sending a review panicked");
        if let Err(err) = sent {
            warn!("card {id}: sending the review failed: {err:#}");
            return Err(Status::Conflict);
        }

        Comment::mark_submitted(db, id, turn).await;
    }

    Ok(Tmpl(
        "_review.html",
        pane(db, settings, cache, id, form.view()).await?,
    ))
}
