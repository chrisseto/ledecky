use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use minijinja::context;
use rocket::form::Form;
use rocket::http::Status;
use rocket::serde::Serialize;
use rocket::{delete, get, post, State};

use crate::agent::AgentManager;
use crate::config::Settings;
use crate::db::DB;
use crate::project::lifecycle::{self, Delivery};
use crate::project::{Card, Lane, Project};
use crate::review::comment::{format_review, Side};
use crate::review::diff::{Line, ParsedFile, Segment};
use crate::review::scope::Mode;
use crate::review::turn;
use crate::review::{Comment, DiffCache, Expansion, Scope, Turn, Viewed};
use crate::tmpl::Tmpl;
use crate::vcs;
use crate::vcs::git;
use crate::watch::Worktrees;

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
}

#[get("/cards/<id>/diff?<scope>&<expand>")]
pub async fn diff_pane(
    db: &State<DB>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    id: i64,
    scope: Option<&str>,
    expand: Option<&str>,
) -> Result<Tmpl, Status> {
    let view = View { scope, expand };
    Ok(Tmpl(
        "_review.html",
        pane(db, settings, cache, id, view).await?,
    ))
}

/// Everything `_review.html` needs, for the drawer's first render.
pub async fn initial(
    db: &DB,
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
    db: &DB,
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
    let turn = viewing_turn(&scope, &turns);
    let here = Comment::find_in_range(db, id, turn, scope.snapshot_turn().is_some())
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
        // NB: no head, no commits — not a literal `HEAD` fallback. These are
        // listed from the *repository*, where `HEAD` is whatever the main
        // checkout has, so the picker would offer the project's own upstream
        // commits as anchors for the card's work.
        Some(worktree) => match vcs::head(card.vcs, worktree).await {
            Some(head) => git::commits(&repo, &settings.base_ref(id), &head).await,
            None => Vec::new(),
        },
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
            expand = Some(expand)
        ))
        .to_string()
    };
    let opening = |expansion: &Expansion| link(&scope_key, &expansion.key());

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

    // Both maps use `<file>#<side>:<line>` as the key, so a template makes one
    // lookup. One map holds the sent comments of a line. The other holds its box.
    let mut sent: HashMap<String, Vec<&Comment>> = HashMap::new();
    let mut boxes: HashMap<String, minijinja::Value> = HashMap::new();
    for comment in &here {
        let key = comment.anchor();
        match comment.is_draft() {
            true => {
                let body = Some(comment.body.as_str());
                let item = box_item(id, &key, body, turn, &scope_key, Some(&expand_key));
                boxes.insert(key, item);
            }
            false => sent.entry(key).or_default().push(comment),
        }
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
        card, tree, sent, boxes, scopes, modes, submitted, stranded,
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
        // What the pane refetches. It names the range and the expansion only,
        // so no fragment has to correct it.
        source => rocket::uri!(diff_pane(
            id = id,
            scope = Some(&scope_key),
            expand = Some(&expand_key)
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
    /// The turn that a new draft on this range belongs to.
    turn: Option<i64>,
}

async fn counts_for(db: &DB, id: i64, scope_key: &str) -> Result<Counts, Status> {
    let scope = Scope::parse(Some(scope_key));
    let turns = Turn::for_card(db, id).await;
    let turn = viewing_turn(&scope, &turns);
    let here = Comment::find_in_range(db, id, turn, scope.snapshot_turn().is_some())
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
        turn,
    })
}

/// Splits `<path>#<side>:<line>` into the line that a comment belongs to.
fn split_key(key: &str) -> Option<(&str, Side, i64)> {
    let (path, anchor) = key.rsplit_once('#')?;
    let (side, line) = anchor.split_once(':')?;
    Some((path, Side::parse(side), line.parse().ok()?))
}

/// The comments of one line, and the box for the next one.
///
/// The server renders this block alone. A save, a close and a delete therefore
/// each replace only the block that they apply to. See `_anchored.html`.
fn anchored(
    id: i64,
    key: &str,
    open: bool,
    scope: &str,
    expand: Option<&str>,
    counts: &Counts,
) -> minijinja::Value {
    // A line has one draft, and the box is that draft. The other comments went
    // to the agent, so the template renders them as text.
    let here = counts.here.iter().filter(|c| c.anchor() == key);
    let (writing, sent): (Vec<_>, Vec<_>) = here.partition(|c| c.is_draft());
    let expand_key = expand.unwrap_or_default().to_owned();

    minijinja::context! {
        card => minijinja::context! { id => id },
        key => key,
        sent => sent,
        // The server renders a box for the draft of the line, or because the
        // user opened the line. The control below the box is the same for both.
        box => (!writing.is_empty() || open).then(|| {
            let body = writing.first().map(|c| c.body.as_str());
            box_item(id, key, body, counts.turn, scope, expand)
        }),
        scope => scope,
        expand => expand_key,
        drafts => counts.drafts,
        stranded => counts.stranded,
        submitted => counts.submitted,
    }
}

/// The box of one line: its text, and the link that deletes the draft.
///
/// The pane and the block both use this function, so a box is the same in
/// each. The URL also stays out of the template, as all other URLs do.
fn box_item(
    id: i64,
    key: &str,
    body: Option<&str>,
    turn: Option<i64>,
    scope: &str,
    expand: Option<&str>,
) -> minijinja::Value {
    minijinja::context! {
        // NB: do not pass the `Option`. minijinja renders a none value as the
        // word "none", and an empty box then contains that word.
        body => body.unwrap_or_default(),
        // The turn that the server rendered the box for. The draft belongs to
        // that turn for as long as the box is open. See `save_comment`.
        turn => turn,
        remove_href => rocket::uri!(discard_draft(
            id = id,
            key = key,
            turn = turn,
            scope = Some(scope),
            expand = Some(expand.unwrap_or_default()),
        ))
        .to_string(),
    }
}

/// The box for one line, or the comments of the line after the box closes.
#[get("/cards/<id>/comments/at?<key>&<scope>&<expand>&<open>")]
pub async fn anchored_block(
    db: &State<DB>,
    id: i64,
    key: String,
    scope: Option<String>,
    expand: Option<String>,
    open: Option<bool>,
) -> Result<Tmpl, Status> {
    let scope = scope.unwrap_or_default();
    let open = open.unwrap_or(false);
    let counts = counts_for(db, id, &scope).await?;
    Ok(Tmpl(
        "_anchored.html",
        anchored(id, &key, open, &scope, expand.as_deref(), &counts),
    ))
}

#[derive(rocket::FromForm)]
pub struct SaveForm {
    /// The line the box belongs to, as `<path>#<side>:<line>`.
    key: String,
    /// The turn that the server rendered the box for. Absent if the card has none.
    turn: Option<i64>,
    body: String,
    #[field(default = String::new())]
    scope: String,
    expand: Option<String>,
}

/// Writes the text of a line's box.
///
/// Sends back the comment count only, as an out-of-band update. It must not
/// send the box: the box saves its text while the user types, so a replacement
/// of the form also replaces the textarea and moves the cursor. The area below
/// the box does not change, so the response omits it too.
#[post("/cards/<id>/comments", data = "<form>")]
pub async fn save_comment(db: &State<DB>, id: i64, form: Form<SaveForm>) -> Result<Tmpl, Status> {
    let (file_path, side, line) = split_key(&form.key).ok_or(Status::BadRequest)?;
    let turns = Turn::for_card(db, id).await;

    // NB: the turn that the server rendered the box for, not the current turn
    // of the card. A box stays open while its draft exists. If a new turn
    // starts, the current turn does not identify the row of that draft.
    // `save_draft` then finds no row, and writes a second comment.
    let turn = turn_of(form.turn, &turns, &form.scope);

    // An empty box is not a comment, so this save deletes the draft.
    Comment::save_draft(db, id, turn, file_path, line, side, form.body.trim())
        .await
        .map_err(|err| failed(id, "saving the comment", err))?;

    let counts = counts_for(db, id, &form.scope).await?;
    Ok(Tmpl(
        "_batch_state.html",
        minijinja::context! {
            card => minijinja::context! { id => id },
            oob => true,
            scope => &form.scope,
            expand => form.expand.clone().unwrap_or_default(),
            drafts => counts.drafts,
            stranded => counts.stranded,
            submitted => counts.submitted,
        },
    ))
}

/// The turn that a box gives for its draft, tested against the card's turns.
///
/// The function tests the value instead of accepting it. An old or incorrect
/// id therefore cannot attach a comment to a turn of a different card.
fn turn_of(claimed: Option<i64>, turns: &[Turn], scope: &str) -> Option<i64> {
    match claimed {
        Some(turn) if turns.iter().any(|t| t.id == turn) => Some(turn),
        // A box that the server rendered before the first turn gives no value.
        // A value that is not a turn of this card is also not usable.
        _ => viewing_turn(&Scope::parse(Some(scope)), turns),
    }
}

/// Deletes the draft on one line before the user sends the batch.
///
/// The URL identifies the line, not the comment. A line has one draft, so the
/// browser does not need the comment id. The box can therefore save its text to
/// a URL that does not change.
///
/// NB: the line and the range are query parameters, not a body. htmx puts the
/// parameters of a `DELETE` in the URL, as it does for a `GET`. It also ignores
/// the form around the button.
#[delete("/cards/<id>/comments/at?<key>&<turn>&<scope>&<expand>")]
pub async fn discard_draft(
    db: &State<DB>,
    id: i64,
    key: &str,
    turn: Option<i64>,
    scope: Option<&str>,
    expand: Option<&str>,
) -> Result<Tmpl, Status> {
    let (file_path, side, line) = split_key(key).ok_or(Status::BadRequest)?;
    let scope_key = scope.unwrap_or_default();
    let turns = Turn::for_card(db, id).await;

    // The turn of the box, for the reason that `save_comment` gives. The draft
    // to delete is the draft that the box shows.
    let turn = turn_of(turn, &turns, scope_key);
    Comment::save_draft(db, id, turn, file_path, line, side, "")
        .await
        .map_err(|err| failed(id, "withdrawing the comment", err))?;

    let counts = counts_for(db, id, scope_key).await?;
    Ok(Tmpl(
        "_anchored_reply.html",
        anchored(id, key, false, scope_key, expand, &counts),
    ))
}

/// Throws away every comment not yet sent, for when a review is reconsidered
/// wholesale.
#[post("/cards/<id>/comments/discard", data = "<form>")]
pub async fn discard_comments(
    db: &State<DB>,
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
pub async fn toggle_viewed(db: &State<DB>, id: i64, form: Form<ViewedForm>) -> Status {
    Viewed::set(db, id, &form.file_path, form.viewed).await;
    Status::NoContent
}

/// What a batch that did not go anywhere says, in the pane it came from.
const BLOCKED: &str =
    "The agent is waiting on you. Answer what is on its terminal, then send again.";
const OCCUPIED: &str =
    "There is unsent text in the agent's terminal. Send or clear it, then send this.";
const UNCONFIRMED: &str = "The agent never acknowledged this. Check its terminal, then send again.";
const DISPLACED: &str =
    "The agent's terminal submitted something else that was in it. This is still a draft.";
/// Not a refusal: this one was delivered, and says what went with it.
const MERGED: &str = "Sent — and unsent text that was in the agent's terminal went with it.";
const NO_AGENT: &str = "No agent could be started to take this, so it is still a draft.";

/// Hands every draft comment to the agent as one message and marks them sent.
///
/// A stopped card starts an agent to take the batch rather than turning the send
/// down — see [`lifecycle::deliver`], which hands it over the way an opening task
/// arrives and so needs no agent on screen yet.
///
/// NB: answers with the pane either way, and only the status differs. A bare
/// `409` is swapped into the target like any other response, so returning one
/// without a body replaced the batch the user was looking at with an error page.
#[post("/cards/<id>/review", data = "<form>")]
pub async fn submit_review(
    db: &State<DB>,
    manager: &State<Arc<AgentManager>>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    worktrees: &State<Worktrees>,
    id: i64,
    form: Form<ViewForm>,
) -> Result<(Status, Tmpl), Status> {
    let scope = Scope::parse(Some(&form.scope));
    let turns = Turn::for_card(db, id).await;
    let drafts = Comment::drafts(db, id)
        .await
        .map_err(|err| failed(id, "reading the drafts", err))?;
    let turn = viewing_turn(&scope, &turns);

    // What the pane says about the send, and whether the send was turned down.
    // Not the same question: a batch can land and still be worth a note.
    let mut note = None;
    let mut refused = false;

    if !drafts.is_empty() {
        let message = format_review(&drafts, &scope.label());

        match lifecycle::deliver(manager, settings, worktrees, id, &message).await {
            Delivery::Pasted | Delivery::Opened => Comment::mark_submitted(db, id, turn).await,
            // Delivered, so the batch is marked sent either way; the note is
            // about what travelled with it, which is the user's to see on the
            // terminal rather than something to guess at from a status.
            Delivery::Merged => {
                Comment::mark_submitted(db, id, turn).await;
                warn!("card {id}: the review went with unsent text from the terminal");
                note = Some(MERGED);
            }
            // The rest leave the batch as drafts, so it goes once whatever is in
            // the way has been dealt with.
            Delivery::Blocked => {
                warn!("card {id}: the review could not be pasted");
                (note, refused) = (Some(BLOCKED), true);
            }
            // Writing into a box that already holds something would send that
            // with it, and the box cannot say what it holds: a paste collapses
            // to a placeholder that names nobody.
            Delivery::Occupied => (note, refused) = (Some(OCCUPIED), true),
            Delivery::Unconfirmed => {
                warn!("card {id}: the review was written but never acknowledged");
                (note, refused) = (Some(UNCONFIRMED), true);
            }
            // The box held a draft the cursor did not give away, and the submit
            // key sent that instead. Nothing can take it back; saying so beats
            // reporting a message that merely never arrived.
            Delivery::Displaced => {
                warn!("card {id}: the terminal submitted something other than the review");
                (note, refused) = (Some(DISPLACED), true);
            }
            Delivery::Failed => (note, refused) = (Some(NO_AGENT), true),
        }
    }

    let status = if refused {
        Status::Conflict
    } else {
        Status::Ok
    };
    let pane = pane(db, settings, cache, id, form.view()).await?;
    Ok((
        status,
        Tmpl("_review.html", context! { review_note => note, ..pane }),
    ))
}
