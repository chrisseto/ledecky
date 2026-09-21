use std::path::PathBuf;

use std::sync::Arc;

use minijinja::context;
use rocket::form::Form;
use rocket::http::Status;
use rocket::response::Redirect;
use rocket::{get, post, State};

use crate::agent::AgentManager;
use crate::config::Settings;
use crate::db::Db;
use crate::events::{Changes, Kind};
use crate::git;
use crate::project::lifecycle;
use crate::project::{Card, CardEdit, Lane, NewCard, Project};
use crate::review::{self, DiffCache, Turn};
use crate::tmpl::Tmpl;
use crate::watch::Worktrees;

pub const PERMISSION_MODES: &[(&str, &str)] = &[
    (
        "acceptEdits",
        "Accept edits — prompts for Bash and other tools",
    ),
    ("bypassPermissions", "Bypass permissions — fully unattended"),
    ("default", "Manual — prompt for everything"),
    ("plan", "Plan — read-only until you approve"),
];

pub const MODELS: &[(&str, &str)] = &[
    ("", "Default"),
    ("opus", "Opus"),
    ("sonnet", "Sonnet"),
    ("haiku", "Haiku"),
];

const EMPTY_TASK: &str = "A card needs a task for the agent.";

/// What is open over the board.
///
/// The board is the only page: projects, a card and both forms are drawers and
/// modals on top of it, each with its own URL so the overlay survives a reload
/// and the back button closes it.
pub const NOTHING: &str = "";
pub const PROJECTS: &str = "projects";
pub const CARD: &str = "card";
pub const NEW_CARD: &str = "newcard";
pub const EDIT_CARD: &str = "editcard";
pub const ADD_PROJECT: &str = "addproject";

/// Renders the board with an overlay over it.
pub struct Shell<'a> {
    pub db: &'a Db,
    pub settings: &'a Settings,
    pub cache: &'a DiffCache,
}

impl Shell<'_> {
    pub async fn render(
        &self,
        project: Option<Project>,
        overlay: &str,
        extra: minijinja::Value,
    ) -> Tmpl {
        let projects = Project::all(self.db).await;

        // NB: loops rather than iterator chains, here and below. Every one of
        // these reads the database, and a closure cannot await.
        let mut counts: Vec<usize> = Vec::with_capacity(projects.len());
        for project in &projects {
            counts.push(Card::for_project(self.db, project.id).await.len());
        }

        let cards = match &project {
            Some(project) => Card::for_project(self.db, project.id).await,
            None => Vec::new(),
        };

        let repo = project.as_ref().map(|p| p.repo());
        let mut rows: Vec<(Lane, minijinja::Value)> = Vec::with_capacity(cards.len());
        for card in &cards {
            let turns = Turn::for_card(self.db, card.id).await;

            // The card counts what is in its worktree, not only what a turn has
            // captured — otherwise a card reads `+0 −0` for as long as its
            // agent is working. Cards without a worktree fall back to their
            // last turn and cost nothing.
            //
            // NB: `Cached`, so none of this shells out. `Shell::render` runs on
            // every navigation and on every event that moves any card, once per
            // card each time; staging here made a board render cost an
            // `add -A` per card and put every one of them behind the slowest.
            // The watcher stages and then announces, which is what keeps this
            // current.
            let mut stat = None;
            if let Some(repo) = repo.as_ref() {
                let worktree = card.worktree_path.as_ref().map(PathBuf::from);
                let head = review::turn::live_head(
                    self.cache,
                    self.settings,
                    repo,
                    worktree.as_deref(),
                    card,
                    &turns,
                    review::turn::Freshness::Cached,
                )
                .await;

                if let Some(head) = head {
                    stat = Some(
                        self.cache
                            .stat(repo, &self.settings.base_ref(card.id), &head)
                            .await,
                    );
                }
            }

            rows.push((
                card.lane,
                context! {
                    stat => stat.filter(|s: &crate::review::cache::Stat| s.additions + s.deletions > 0),
                    ..minijinja::Value::from_serialize(card)
                },
            ));
        }

        let lanes: Vec<_> = Lane::VISIBLE
            .iter()
            .map(|lane| {
                context! {
                    key => lane.as_str(),
                    label => lane.label(),
                    cards => rows.iter().filter(|(l, _)| l == lane)
                        .map(|(_, card)| card.clone()).collect::<Vec<_>>(),
                }
            })
            .collect();

        Tmpl(
            "board.html",
            context! {
                project, lanes, overlay,
                projects => projects.iter().zip(counts)
                    .map(|(project, cards)| context! { cards, ..minijinja::Value::from_serialize(project) })
                    .collect::<Vec<_>>(),
                ..extra
            },
        )
    }
}

#[get("/projects/<id>")]
pub async fn board(
    db: &State<Db>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    id: i64,
) -> Result<Tmpl, Status> {
    let project = Project::find(db, id).await.ok_or(Status::NotFound)?;
    Ok(Shell {
        db: &db,
        settings: &settings,
        cache: &cache,
    }
    .render(Some(project), NOTHING, context! {})
    .await)
}

/// The board with nothing selected — whichever project was added first, or an
/// empty shell asking for one.
#[get("/")]
pub async fn index(db: &State<Db>, settings: &State<Settings>, cache: &State<DiffCache>) -> Tmpl {
    let project = Project::all(db).await.into_iter().next();
    Shell {
        db: &db,
        settings: &settings,
        cache: &cache,
    }
    .render(project, NOTHING, context! {})
    .await
}

/// The project switcher, over whichever board it was opened from.
#[get("/projects?<board>")]
pub async fn switcher(
    db: &State<Db>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    board: Option<i64>,
) -> Tmpl {
    let project = current(&db, board).await;
    Shell {
        db: &db,
        settings: &settings,
        cache: &cache,
    }
    .render(project, PROJECTS, context! {})
    .await
}

#[get("/projects/<id>/cards/new")]
pub async fn new_card(
    db: &State<Db>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    id: i64,
) -> Result<Tmpl, Status> {
    let project = Project::find(db, id).await.ok_or(Status::NotFound)?;
    let form = form_context(&project, None, Fields::default(), None).await;

    Ok(Shell {
        db: &db,
        settings: &settings,
        cache: &cache,
    }
    .render(Some(project), NEW_CARD, form)
    .await)
}

/// The card form, on a card that has not been started yet.
#[get("/cards/<id>/edit")]
pub async fn edit_card(
    db: &State<Db>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    id: i64,
) -> Result<Tmpl, Status> {
    let card = Card::find(db, id).await.ok_or(Status::NotFound)?;
    let project = Project::find(db, card.project_id)
        .await
        .ok_or(Status::NotFound)?;

    if !card.editable() {
        return Err(Status::Conflict);
    }

    let form = form_context(&project, Some(&card), Fields::of(&card), None).await;
    Ok(Shell {
        db: &db,
        settings: &settings,
        cache: &cache,
    }
    .render(Some(project), EDIT_CARD, form)
    .await)
}

#[derive(rocket::FromForm)]
pub struct CardForm {
    task: String,
    base_branch: String,
    permission_mode: String,
    model: String,
    /// Set by the second submit button, which keeps the form open for the next
    /// card rather than returning to the board.
    more: Option<String>,
}

/// What the card form holds, wherever it came from: a card being edited, a
/// submission that came back with an error, or the defaults.
#[derive(Default)]
struct Fields {
    task: String,
    base_branch: String,
    permission_mode: String,
    model: String,
}

impl Fields {
    fn of(card: &Card) -> Self {
        Self {
            task: card.task.clone(),
            base_branch: card.base_branch.clone(),
            permission_mode: card.permission_mode.clone(),
            model: card.model.clone().unwrap_or_default(),
        }
    }

    fn submitted(form: &CardForm) -> Self {
        Self {
            task: form.task.clone(),
            base_branch: form.base_branch.clone(),
            permission_mode: form.permission_mode.clone(),
            model: form.model.clone(),
        }
    }
}

/// Everything `_modal_card.html` renders from. `card` is what makes it an edit
/// rather than a new card.
async fn form_context(
    project: &Project,
    card: Option<&Card>,
    fields: Fields,
    error: Option<&str>,
) -> minijinja::Value {
    let branches = git::branches(&project.repo()).await;
    let base_branch = match fields.base_branch.is_empty() {
        true => branches.first().cloned().unwrap_or_default(),
        false => fields.base_branch,
    };

    context! {
        card, error, branches, base_branch,
        task => fields.task,
        permission_mode => fields.permission_mode,
        model => fields.model,
        permission_modes => PERMISSION_MODES,
        models => MODELS,
    }
}

#[post("/projects/<id>/cards", data = "<form>")]
pub async fn create_card(
    db: &State<Db>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    changes: &State<Changes>,
    id: i64,
    form: Form<CardForm>,
) -> Result<Redirect, Tmpl> {
    create(db, settings, cache, changes, id, form.into_inner()).await
}

async fn create(
    db: &Db,
    settings: &Settings,
    cache: &DiffCache,
    changes: &Changes,
    id: i64,
    form: CardForm,
) -> Result<Redirect, Tmpl> {
    let project = match Project::find(db, id).await {
        Some(project) => project,
        None => return Ok(Redirect::to("/")),
    };

    let task = form.task.trim();
    if task.is_empty() {
        let context =
            form_context(&project, None, Fields::submitted(&form), Some(EMPTY_TASK)).await;
        return Err(Shell {
            db,
            settings,
            cache,
        }
        .render(Some(project), NEW_CARD, context)
        .await);
    }

    let created = Card::create(
        db,
        NewCard {
            project_id: id,
            task,
            base_branch: form.base_branch.trim(),
            permission_mode: permission_mode(&form.permission_mode),
            model: Some(form.model.trim()).filter(|m| !m.is_empty()),
        },
    )
    .await;

    if created.is_err() {
        return Ok(Redirect::to(format!("/projects/{id}")));
    }

    changes.project(id, Kind::Board);

    Ok(match form.more {
        Some(_) => Redirect::to(format!("/projects/{id}/cards/new")),
        None => Redirect::to(format!("/projects/{id}")),
    })
}

/// Rewrites a card that has not been started. Everything the form sets is only
/// read when the session opens, so until then it is all still a draft.
#[post("/cards/<id>", data = "<form>")]
pub async fn update_card(
    db: &State<Db>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    changes: &State<Changes>,
    id: i64,
    form: Form<CardForm>,
) -> Result<Result<Redirect, Tmpl>, Status> {
    update(db, settings, cache, changes, id, form.into_inner()).await
}

async fn update(
    db: &Db,
    settings: &Settings,
    cache: &DiffCache,
    changes: &Changes,
    id: i64,
    form: CardForm,
) -> Result<Result<Redirect, Tmpl>, Status> {
    let card = Card::find(db, id).await.ok_or(Status::NotFound)?;
    let project = Project::find(db, card.project_id)
        .await
        .ok_or(Status::NotFound)?;

    let task = form.task.trim();
    if task.is_empty() {
        let context = form_context(
            &project,
            Some(&card),
            Fields::submitted(&form),
            Some(EMPTY_TASK),
        )
        .await;
        return Ok(Err(Shell {
            db,
            settings,
            cache,
        }
        .render(Some(project), EDIT_CARD, context)
        .await));
    }

    let edited = Card::update(
        db,
        id,
        CardEdit {
            task,
            base_branch: form.base_branch.trim(),
            permission_mode: permission_mode(&form.permission_mode),
            model: Some(form.model.trim()).filter(|m| !m.is_empty()),
        },
    )
    .await;

    match edited {
        Ok(true) => {
            changes.project(card.project_id, Kind::Board);
            Ok(Ok(Redirect::to(format!("/cards/{id}"))))
        }
        // The card was started while the form was open: the agent has the old
        // task already, so this edit would only pretend to have changed it.
        Ok(false) => Err(Status::Conflict),
        Err(_) => Err(Status::InternalServerError),
    }
}

/// Only modes the form offers are accepted; anything else is someone poking at
/// the endpoint, and `acceptEdits` is the safe reading.
fn permission_mode(requested: &str) -> &'static str {
    PERMISSION_MODES
        .iter()
        .find(|(mode, _)| *mode == requested)
        .map_or("acceptEdits", |(mode, _)| *mode)
}

/// The project a URL names, falling back to the first one so an overlay always
/// has a board behind it.
pub async fn current(db: &Db, id: Option<i64>) -> Option<Project> {
    if let Some(project) = id {
        if let Some(found) = Project::find(db, project).await {
            return Some(found);
        }
    }
    Project::all(db).await.into_iter().next()
}

#[derive(rocket::FromForm)]
pub struct MoveForm {
    lane: String,
    #[field(default = 0)]
    index: usize,
}

#[post("/cards/<id>/move", data = "<form>")]
pub async fn move_card(
    db: &State<Db>,
    manager: &State<Arc<AgentManager>>,
    settings: &State<Settings>,
    changes: &State<Changes>,
    worktrees: &State<Worktrees>,
    id: i64,
    form: Form<MoveForm>,
) -> Result<Status, Status> {
    relane(
        db, manager, settings, changes, worktrees, id, &form.lane, form.index,
    )
    .await?;
    Ok(Status::NoContent)
}

/// The same move from the card drawer, which stays open around it.
#[post("/cards/<id>/lane", data = "<form>")]
pub async fn move_card_to_lane(
    db: &State<Db>,
    manager: &State<Arc<AgentManager>>,
    settings: &State<Settings>,
    changes: &State<Changes>,
    worktrees: &State<Worktrees>,
    id: i64,
    form: Form<MoveForm>,
) -> Result<Redirect, Status> {
    relane(
        db, manager, settings, changes, worktrees, id, &form.lane, form.index,
    )
    .await?;
    Ok(Redirect::to(format!("/cards/{id}")))
}

/// Puts a card in a lane at an index. Entering In Progress starts its agent;
/// entering Done stops it.
///
/// Both movers land here: the board's drag handler, which answers 204, and the
/// drawer's, which redirects back to the card. Entering In Progress needs
/// everything a session does, which is why this takes more than a database.
///
/// NB: the arguments are its two callers' request guards, passed straight
/// through; see the note on `webhooks::receive`.
#[allow(clippy::too_many_arguments)]
async fn relane(
    db: &State<Db>,
    manager: &State<Arc<AgentManager>>,
    settings: &State<Settings>,
    changes: &State<Changes>,
    worktrees: &State<Worktrees>,
    id: i64,
    lane: &str,
    index: usize,
) -> Result<(), Status> {
    let lane = Lane::parse(lane);

    // Only `collect_garbage` may put a card here; arriving by drag or by a
    // hand-written POST would hide it with its worktree still on disk.
    if lane == Lane::GarbageCollected {
        return Err(Status::BadRequest);
    }

    let card = Card::find(db, id).await.ok_or(Status::NotFound)?;
    Card::reorder(db, id, card.project_id, lane, index).await;

    changes.project(card.project_id, Kind::Board);

    // Entering In Progress is what creates the worktree and starts the agent.
    // Re-entering it with a live agent is a no-op.
    if lane == Lane::InProgress && card.lane != Lane::InProgress {
        if let Err(err) = lifecycle::start(manager, settings, worktrees, id).await {
            error!("card {id}: {err:#}");
            manager.failed(id).await;
        }
    }

    // The worktree stays: Done is not a merge, and garbage collection reclaims it.
    if lane == Lane::Done && manager.running(id).is_some() {
        manager.stop(id).await;
    }

    Ok(())
}

#[post("/cards/<id>/delete")]
pub async fn delete_card(
    db: &State<Db>,
    manager: &State<Arc<AgentManager>>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    changes: &State<Changes>,
    worktrees: &State<Worktrees>,
    id: i64,
) -> Result<Redirect, Status> {
    remove(db, manager, settings, cache, changes, worktrees, id).await
}

async fn remove(
    db: &Db,
    manager: &Arc<AgentManager>,
    settings: &Settings,
    cache: &DiffCache,
    changes: &Changes,
    worktrees: &Worktrees,
    id: i64,
) -> Result<Redirect, Status> {
    let project_id = Card::find(db, id).await.ok_or(Status::NotFound)?.project_id;

    lifecycle::teardown(manager, settings, cache, worktrees, id).await;
    Card::delete(db, id)
        .await
        .map_err(|_| Status::InternalServerError)?;
    changes.project(project_id, Kind::Board);

    Ok(Redirect::to(format!("/projects/{project_id}")))
}

/// Reclaims the disk every card in Done is still holding.
///
/// The rows stay and the cards leave the board: what they own on disk is gone,
/// so there is nothing left to go back to.
#[post("/projects/<id>/cards/garbage")]
pub async fn collect_garbage(
    db: &State<Db>,
    manager: &State<Arc<AgentManager>>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    changes: &State<Changes>,
    worktrees: &State<Worktrees>,
    id: i64,
) -> Result<Redirect, Status> {
    let done: Vec<Card> = Card::for_project(db, id)
        .await
        .into_iter()
        .filter(|card| card.lane == Lane::Done)
        .collect();

    for card in &done {
        card.collect_garbage(manager, settings, cache, worktrees)
            .await;
    }

    // One event for the batch: the board refetches once, however many went.
    changes.project(id, Kind::Board);

    Ok(Redirect::to(format!("/projects/{id}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_offered_permission_modes_are_accepted() {
        assert_eq!(permission_mode("bypassPermissions"), "bypassPermissions");
        assert_eq!(permission_mode("plan"), "plan");
        // Anything unrecognised lands on the conservative default.
        assert_eq!(permission_mode("rm -rf"), "acceptEdits");
        assert_eq!(permission_mode(""), "acceptEdits");
    }
}
