use std::path::PathBuf;

use std::sync::Arc;

use minijinja::context;
use rocket::form::Form;
use rocket::http::Status;
use rocket::response::Redirect;
use rocket::serde::Serialize;
use rocket::{get, post, State};

use crate::agent::AgentManager;
use crate::config::Settings;
use crate::db::DB;
use crate::events::{Changes, Kind};
use crate::project::lifecycle;
use crate::project::{Card, CardEdit, Lane, NewCard, Project};
use crate::review::{self, DiffCache, Turn};
use crate::tmpl::Tmpl;
use crate::vcs::git;
use crate::vcs::{self, VCS};
use crate::watch::Worktrees;

pub const PERMISSION_MODES: &[(&str, &str)] = &[
    ("plan", "Plan"),
    ("auto", "Auto Mode"),
    ("acceptEdits", "Accept Edits"),
    ("default", "Manual"),
];

pub const MODELS: &[(&str, &str)] = &[
    ("", "Default"),
    ("opus", "Opus"),
    ("sonnet", "Sonnet"),
    ("haiku", "Haiku"),
];

const EMPTY_TASK: &str = "A card needs a task for the agent.";

/// Said when the base does not resolve. The picker will not offer one that
/// does not, so this is a base that went away while the form was open, or a
/// post that never came from the form at all.
const BAD_BASE: &str = "That base is not something git can resolve.";

/// How many rows the picker will offer before it asks for more typing.
const MAX_CHOICES: usize = 50;

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
    pub db: &'a DB,
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
    db: &State<DB>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    id: i64,
) -> Result<Tmpl, Status> {
    let project = Project::find(db, id).await.ok_or(Status::NotFound)?;
    Ok(Shell {
        db,
        settings,
        cache,
    }
    .render(Some(project), NOTHING, context! {})
    .await)
}

/// The board with nothing selected — whichever project was added first, or an
/// empty shell asking for one.
#[get("/")]
pub async fn index(db: &State<DB>, settings: &State<Settings>, cache: &State<DiffCache>) -> Tmpl {
    let project = Project::all(db).await.into_iter().next();
    Shell {
        db,
        settings,
        cache,
    }
    .render(project, NOTHING, context! {})
    .await
}

/// The project switcher, over whichever board it was opened from.
#[get("/projects?<board>")]
pub async fn switcher(
    db: &State<DB>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    board: Option<i64>,
) -> Tmpl {
    let project = current(db, board).await;
    Shell {
        db,
        settings,
        cache,
    }
    .render(project, PROJECTS, context! {})
    .await
}

#[get("/projects/<id>/cards/new")]
pub async fn new_card(
    db: &State<DB>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    id: i64,
) -> Result<Tmpl, Status> {
    let project = Project::find(db, id).await.ok_or(Status::NotFound)?;
    let form = form_context(&project, None, Fields::default(), None).await;

    Ok(Shell {
        db,
        settings,
        cache,
    }
    .render(Some(project), NEW_CARD, form)
    .await)
}

/// The card form, on a card that has not been started yet.
#[get("/cards/<id>/edit")]
pub async fn edit_card(
    db: &State<DB>,
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
        db,
        settings,
        cache,
    }
    .render(Some(project), EDIT_CARD, form)
    .await)
}

/// The card drawer over its board, which is what `/cards/<id>` is and what a
/// write from inside the drawer answers with.
///
/// `error` is a complaint to show above the panes. The drawer's own controls
/// can be refused — the chip asking for a branch that has gone, a merge already
/// out — and a refusal nobody can read is a control that looks broken.
pub async fn card_view(
    db: &DB,
    manager: &AgentManager,
    settings: &Settings,
    cache: &DiffCache,
    id: i64,
    scope: Option<&str>,
    error: Option<&str>,
) -> Result<Tmpl, Status> {
    let card = Card::find(db, id).await.ok_or(Status::NotFound)?;
    let project = Project::find(db, card.project_id)
        .await
        .ok_or(Status::NotFound)?;

    let live = manager.running(id).is_some();
    let review = review::routes::initial(db, settings, cache, id, scope).await?;
    // The chip's menu opens on what the page already holds, so there is nothing
    // to wait for; the search box re-renders it from there.
    let picker = picker(&project, "", &card.base_branch, Some(id)).await;

    Ok(Shell {
        db,
        settings,
        cache,
    }
    .render(
        Some(project),
        CARD,
        context! { live, picker, error, editable => card.editable(), ..review },
    )
    .await)
}

#[derive(rocket::FromForm)]
pub struct CardForm {
    task: String,
    base_branch: String,
    permission_mode: String,
    model: String,
    /// Absent when the project offers only one, in which case the form renders
    /// no chooser at all.
    vcs: Option<String>,
    /// Set by the second submit button, which keeps the form open for the next
    /// card rather than returning to the board.
    more: Option<String>,
}

/// What the card form holds, wherever it came from: a card being edited, a
/// submission that came back with an error, or the defaults.
struct Fields {
    task: String,
    base_branch: String,
    permission_mode: String,
    model: String,
    vcs: VCS,
}

impl Default for Fields {
    fn default() -> Self {
        Self {
            task: String::new(),
            base_branch: String::new(),
            permission_mode: "plan".into(),
            model: String::new(),
            vcs: VCS::Git,
        }
    }
}

impl Fields {
    fn of(card: &Card) -> Self {
        Self {
            task: card.task.clone(),
            base_branch: card.base_branch.clone(),
            permission_mode: card.permission_mode.clone(),
            model: card.model.clone().unwrap_or_default(),
            vcs: card.vcs,
        }
    }

    fn submitted(form: &CardForm) -> Self {
        Self {
            task: form.task.clone(),
            base_branch: form.base_branch.clone(),
            permission_mode: form.permission_mode.clone(),
            model: form.model.clone(),
            vcs: form.vcs.as_deref().map_or(VCS::Git, VCS::parse),
        }
    }
}

/// One row of the picker's list.
#[derive(Serialize)]
#[serde(crate = "rocket::serde")]
struct Choice {
    /// The base this sets, or — for a remote — the prefix it fills the search
    /// box with.
    value: String,
    /// What the value is, when its name does not say: `commit a1b2c3d`.
    note: Option<String>,
    /// A remote to look inside rather than a base to pick.
    fill: bool,
}

/// How a resolved revision reads beside its name.
fn describe(resolved: &git::Resolved) -> String {
    let short = resolved.sha.get(..7).unwrap_or(&resolved.sha);
    match resolved.kind {
        git::RefKind::Branch => format!("branch {short}"),
        git::RefKind::Remote => format!("remote branch {short}"),
        git::RefKind::Tag => format!("tag {short}"),
        git::RefKind::Commit => format!("commit {short}"),
    }
}

/// Everything the branch picker renders from, wherever it is drawn: the chip,
/// what it says about the base, the rows under the search box, and the verdict
/// on what has been typed.
///
/// One helper for all four sites — both fragment routes and both pages — so
/// that what is chosen and what is said about it cannot disagree. Rendering a
/// page and rendering the fragment it will later swap used to be two different
/// contexts, and the half the pages left out is what put "No branch matches"
/// above a full list on every first paint.
///
/// NB: git on every keystroke, the way `project::complete` lists a directory on
/// every one. Up to six short-lived processes now — the remotes, one ref
/// listing, and a resolve each for what is typed and what is chosen — against a
/// repository that is almost certainly warm. The resolves only run when there
/// is something to resolve.
async fn picker(project: &Project, q: &str, base: &str, card: Option<i64>) -> minijinja::Value {
    let repo = project.repo();
    let wanted = q.trim();
    let lowered = wanted.to_lowercase();
    let remotes = git::remotes(&repo).await;

    // `origin/` reads the way a trailing slash reads in the directory
    // completion: the prefix says where to look, and the whole of it still
    // filters inside. Only a name git knows drills in — a branch called
    // `feature/x` is not a remote called `feature`.
    let drilled = wanted
        .split_once('/')
        .map(|(remote, _)| remote)
        .filter(|remote| remotes.iter().any(|known| known == remote));

    let listed = match drilled {
        Some(remote) => git::remote_branches(&repo, remote).await,
        None => git::branches(&repo).await,
    };

    let mut names: Vec<String> = listed
        .into_iter()
        .filter(|name| name.to_lowercase().contains(&lowered))
        .collect();

    // A remote is somewhere to look, not something to base a card on — and
    // only offered when the search is not already inside one.
    let stepping: Vec<String> = match drilled {
        Some(_) => Vec::new(),
        None => remotes
            .iter()
            .filter(|remote| remote.to_lowercase().contains(&lowered))
            .map(|remote| format!("{remote}/"))
            .collect(),
    };

    // Whether the *search* found anything, which is a different question once
    // the chosen base is in the list regardless. A remote counts: `ori` is a
    // search that got somewhere, whether or not it is itself a revision.
    let matched = !names.is_empty() || !stepping.is_empty();

    // NB: the cap falls on the branches alone. The chosen base goes on after it
    // and the remotes are a handful, so what a long list drops is only ever a
    // branch nobody has narrowed down to yet.
    let truncated = names.len() > MAX_CHOICES;
    names.truncate(MAX_CHOICES);

    // The base stays on offer however little it matches, so a re-filter never
    // reads as having dropped the choice.
    if !base.is_empty() && !names.iter().any(|name| name == base) {
        names.push(base.to_owned());
    }

    let typed = match wanted.is_empty() {
        true => None,
        false => git::resolve(&repo, wanted).await,
    };

    let mut choices = Vec::new();

    // What was typed, when git resolves it and it is not already on offer. This
    // is the whole of pasting a sha: the server says what it is, and the row
    // carries it verbatim.
    if let Some(resolved) = &typed {
        if !names.iter().any(|name| name == wanted) {
            choices.push(Choice {
                value: wanted.to_owned(),
                note: Some(describe(resolved)),
                fill: false,
            });
        }
    }

    choices.extend(names.into_iter().map(|value| Choice {
        value,
        note: None,
        fill: false,
    }));

    choices.extend(stepping.into_iter().map(|value| Choice {
        value,
        note: None,
        fill: true,
    }));

    // Said only when it is news. A search that resolves is worth confirming; a
    // prefix that matches branches is working, whether or not it is itself a
    // revision; and nothing matching is the one thing that needs explaining.
    let status = match (&typed, matched) {
        (Some(resolved), _) => Some(describe(resolved)),
        (None, true) => None,
        (None, false) if wanted.is_empty() => None,
        (None, false) => Some(format!("Nothing in {} resolves “{wanted}”", project.name)),
    };
    let status_ok = typed.is_some();

    // The chip's own marker. A base that does not resolve can never start a
    // card, and that is worth seeing before Start rather than after it.
    let chosen = match base == wanted {
        true => typed,
        false => match base.is_empty() {
            true => None,
            false => git::resolve(&repo, base).await,
        },
    };

    let search_to = match card {
        Some(card) => format!("/projects/{}/branches?card={card}", project.id),
        None => format!("/projects/{}/branches", project.id),
    };

    context! {
        choices, truncated, status_ok,
        q => wanted,
        // A complaint is the one thing worth staying open for: it is about what
        // was typed, and closing the menu would take both away.
        open => status.is_some() && !status_ok,
        status,
        base_branch => base,
        base_valid => chosen.is_some(),
        // Said on the chip only when the name does not already say it: a local
        // branch is what the icon beside it means.
        base_note => chosen
            .as_ref()
            .filter(|resolved| resolved.kind != git::RefKind::Branch)
            .map(describe),
        search_to,
        // A card's rows post themselves at the card. The form has nothing to
        // post to, so a row re-renders the picker instead and the server is
        // what puts the choice in the field.
        post_to => card.map(|card| format!("/cards/{card}/base")),
        pick_to => format!("/projects/{}/branch-picker", project.id),
    }
}

/// The rows matching `q`, which is what the search box re-renders.
#[get("/projects/<id>/branches?<q>&<card>&<base_branch>")]
pub async fn branch_menu(
    db: &State<DB>,
    id: i64,
    q: Option<&str>,
    card: Option<i64>,
    base_branch: Option<&str>,
) -> Result<Tmpl, Status> {
    let project = Project::find(db, id).await.ok_or(Status::NotFound)?;
    let context = picker(
        &project,
        q.unwrap_or_default(),
        base_branch.unwrap_or_default(),
        card,
    )
    .await;

    Ok(Tmpl("_branch_menu.html", context! { picker => context }))
}

/// The whole picker, which is how the card form adopts a choice.
///
/// There is nothing to post to before the card exists, so a row asks for the
/// picker back with its value as the base: the chip, the verdict on it and the
/// hidden field the form submits are all re-rendered together, and so cannot
/// disagree. A base git will not resolve is not adopted — the previous one
/// stands and the status line says why, which keeps the field always valid.
#[get("/projects/<id>/branch-picker?<base_branch>&<chosen>")]
pub async fn branch_picker(
    db: &State<DB>,
    id: i64,
    base_branch: Option<&str>,
    chosen: Option<&str>,
) -> Result<Tmpl, Status> {
    let project = Project::find(db, id).await.ok_or(Status::NotFound)?;
    let repo = project.repo();

    let asked = base_branch.unwrap_or_default().trim();
    let standing = chosen.unwrap_or_default().trim();
    let taken = git::resolve(&repo, asked).await.is_some();
    let base = match taken {
        true => asked,
        false => standing,
    };

    // The ask is dropped from the search box once it lands: the box has done its
    // job, and leaving it filled would re-open on a filtered list. A refusal
    // keeps it, because that is what the complaint is about — and a row that
    // went stale is a refusal nobody typed, so the box is where its name
    // surfaces.
    //
    // NB: on whether it resolved, not on whether the base changed. Re-sending a
    // base that is already standing and already broken is a refusal too, and
    // reading it as a no-op would shut the menu saying nothing.
    let typed = match taken {
        true => "",
        false => asked,
    };

    let context = picker(&project, typed, base, None).await;
    Ok(Tmpl("_branch_picker.html", context! { picker => context }))
}

/// Everything `_modal_card.html` renders from. `card` is what makes it an edit
/// rather than a new card.
async fn form_context(
    project: &Project,
    card: Option<&Card>,
    fields: Fields,
    error: Option<&str>,
) -> minijinja::Value {
    // `branches` puts the checked-out one first, which is what makes it the
    // default a new card opens on.
    let base_branch = match fields.base_branch.is_empty() {
        true => git::branches(&project.repo())
            .await
            .first()
            .cloned()
            .unwrap_or_default(),
        false => fields.base_branch,
    };

    // Per project rather than a const beside `MODELS`: what a repo can make a
    // workspace with is a property of the repo. One option is git and needs no
    // chooser, which is what the template keys off.
    let detected = vcs::detect(&project.repo());
    let vcs_options: Vec<_> = detected
        .iter()
        .map(|vcs| (vcs.as_str(), vcs.label()))
        .collect();
    let vcs = match detected.contains(&fields.vcs) {
        true => fields.vcs,
        false => VCS::Git,
    };

    context! {
        card, error, vcs_options,
        picker => picker(project, "", &base_branch, None).await,
        task => fields.task,
        permission_mode => fields.permission_mode,
        model => fields.model,
        vcs => vcs.as_str(),
        permission_modes => PERMISSION_MODES,
        models => MODELS,
    }
}

/// Only a VCS the project actually has is accepted.
///
/// Checked against the repository rather than against the list the form
/// rendered: a stale form, or someone poking at the endpoint, would otherwise
/// create a card whose workspace cannot be made at all. Git is the safe reading
/// — it is what `detect` offers wherever there is anything to offer, and what an
/// absent field means, the form having had nothing to ask.
fn chosen_vcs(project: &Project, requested: Option<&str>) -> VCS {
    let wanted = requested.map_or(VCS::Git, VCS::parse);
    match vcs::detect(&project.repo()).contains(&wanted) {
        true => wanted,
        false => VCS::Git,
    }
}

#[post("/projects/<id>/cards", data = "<form>")]
pub async fn create_card(
    db: &State<DB>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    changes: &State<Changes>,
    id: i64,
    form: Form<CardForm>,
) -> Result<Redirect, Tmpl> {
    create(db, settings, cache, changes, id, form.into_inner()).await
}

async fn create(
    db: &DB,
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
    let base_branch = form.base_branch.trim();
    // Validate, then mutate. A base nothing resolves is a card that can never
    // be started, and `relane` answers 204 whether or not the worktree it then
    // asks for could be made — so refused here is the only place it is said.
    let complaint = match task.is_empty() {
        true => Some(EMPTY_TASK),
        false => match git::resolve(&project.repo(), base_branch).await {
            Some(_) => None,
            None => Some(BAD_BASE),
        },
    };

    if let Some(complaint) = complaint {
        let context = form_context(&project, None, Fields::submitted(&form), Some(complaint)).await;
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
            base_branch,
            permission_mode: permission_mode(&form.permission_mode),
            model: Some(form.model.trim()).filter(|m| !m.is_empty()),
            vcs: chosen_vcs(&project, form.vcs.as_deref()),
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
    db: &State<DB>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    changes: &State<Changes>,
    id: i64,
    form: Form<CardForm>,
) -> Result<Result<Redirect, Tmpl>, Status> {
    update(db, settings, cache, changes, id, form.into_inner()).await
}

async fn update(
    db: &DB,
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
    let base_branch = form.base_branch.trim();
    let complaint = match task.is_empty() {
        true => Some(EMPTY_TASK),
        false => match git::resolve(&project.repo(), base_branch).await {
            Some(_) => None,
            None => Some(BAD_BASE),
        },
    };

    if let Some(complaint) = complaint {
        let context = form_context(
            &project,
            Some(&card),
            Fields::submitted(&form),
            Some(complaint),
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
            base_branch,
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

#[derive(rocket::FromForm)]
pub struct BaseForm {
    base_branch: String,
}

/// Re-points a card at another base, from the chip in its drawer.
///
/// Anything git resolves will do — a branch, a tag, a remote-tracking ref, a
/// pasted sha — because everything downstream hands the base to git as a bare
/// revision. The name is resolved against git rather than against the list the
/// page was drawn from, which can be stale by the time the form comes back.
/// Nothing is written until it resolves.
///
/// A refusal is the drawer again with the reason on it, under the status it
/// deserves; htmx swaps a 4xx body, so the chip says what went wrong instead of
/// quietly springing back.
///
/// The worktree keeps its root and `base_ref` its value: this re-aims
/// `reconcile_base` and the merge, it does not move anything. It does restage,
/// though, because reconciliation only runs while a head is being produced — so
/// without it the redirect below would re-render inside a memo younger than
/// `head_ttl` and show the branch the card used to have.
#[post("/cards/<id>/base", data = "<form>")]
pub async fn set_base(
    db: &State<DB>,
    manager: &State<Arc<AgentManager>>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    changes: &State<Changes>,
    id: i64,
    form: Form<BaseForm>,
) -> Result<Result<Redirect, (Status, Tmpl)>, Status> {
    let card = Card::find(db, id).await.ok_or(Status::NotFound)?;
    let project = Project::find(db, card.project_id)
        .await
        .ok_or(Status::NotFound)?;

    let refused = async |status: Status, error: String| {
        let page = card_view(db, manager, settings, cache, id, None, Some(&error)).await?;
        Ok(Err((status, page)))
    };

    // The agent has been told to land on the branch this card had, and
    // `check_merge` is watching that one for the work. Moving the name now would
    // have it read another branch's history as the merge.
    if card.merge_requested {
        return refused(
            Status::Conflict,
            format!(
                "This card is already waiting to land on {}. Stop the merge before moving it.",
                card.base_branch
            ),
        )
        .await;
    }

    let branch = form.base_branch.trim();
    if git::resolve(&project.repo(), branch).await.is_none() {
        return refused(
            Status::UnprocessableEntity,
            format!("Nothing in {} resolves {branch}.", project.name),
        )
        .await;
    }

    Card::set_base_branch(db, id, branch).await.map_err(|err| {
        error!("card {id}: re-pointing at {branch}: {err}");
        Status::InternalServerError
    })?;

    // After the write, so this reconciles against the branch just chosen rather
    // than the one it replaced. `restage` answers `None` for a card with no
    // worktree — one that has not started, or one torn down — which is also
    // every card that has no base ref to re-aim.
    review::turn::restage(db, cache, settings, id).await;

    changes.card_in(card.project_id, id, Kind::Diff);
    changes.project(card.project_id, Kind::Board);
    Ok(Ok(Redirect::to(format!("/cards/{id}"))))
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
pub async fn current(db: &DB, id: Option<i64>) -> Option<Project> {
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
    db: &State<DB>,
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
    db: &State<DB>,
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
    db: &State<DB>,
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
    db: &State<DB>,
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
    db: &DB,
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
    db: &State<DB>,
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
        assert_eq!(permission_mode("auto"), "auto");
        assert_eq!(permission_mode("plan"), "plan");
        assert_eq!(permission_mode("bypassPermissions"), "acceptEdits");
        // Anything unrecognised lands on the conservative default.
        assert_eq!(permission_mode("rm -rf"), "acceptEdits");
        assert_eq!(permission_mode(""), "acceptEdits");
    }
}
