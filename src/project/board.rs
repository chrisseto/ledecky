use minijinja::context;
use rocket::form::Form;
use rocket::http::Status;
use rocket::response::Redirect;
use rocket::{get, post, State};

use crate::agent::{session, Agents};
use crate::config::Settings;
use crate::db::Db;
use crate::git;
use crate::hooks::HookAuth;
use crate::project::{AgentState, Card, Lane, NewCard, Project};
use crate::review::{DiffCache, Turn};
use crate::tmpl::Tmpl;

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

/// A card's title is the first line of its task, cut to something that fits on
/// a card.
const TITLE_MAX: usize = 60;

/// What is open over the board.
///
/// The board is the only page: projects, a card and both forms are drawers and
/// modals on top of it, each with its own URL so the overlay survives a reload
/// and the back button closes it.
pub const NOTHING: &str = "";
pub const PROJECTS: &str = "projects";
pub const CARD: &str = "card";
pub const NEW_CARD: &str = "newcard";
pub const ADD_PROJECT: &str = "addproject";

/// Renders the board with an overlay over it.
pub struct Shell<'a> {
    pub db: &'a Db,
    pub settings: &'a Settings,
    pub cache: &'a DiffCache,
}

impl Shell<'_> {
    pub fn render(&self, project: Option<Project>, overlay: &str, extra: minijinja::Value) -> Tmpl {
        let conn = self.db.lock();
        let projects = Project::all(&conn);
        let counts: Vec<usize> = projects
            .iter()
            .map(|p| Card::for_project(&conn, p.id).len())
            .collect();

        let cards = match &project {
            Some(project) => Card::for_project(&conn, project.id),
            None => Vec::new(),
        };
        // The stats below shell out to git, so the turns come out of the
        // database first and the lock goes back before any of that happens.
        let turns: Vec<Option<Turn>> = cards
            .iter()
            .map(|card| Turn::latest(&conn, card.id))
            .collect();
        drop(conn);

        let repo = project.as_ref().map(|p| p.repo());
        let rows: Vec<(Lane, minijinja::Value)> = cards
            .iter()
            .zip(turns)
            .map(|(card, turn)| {
                let stat = repo.as_ref().zip(turn).map(|(repo, turn)| {
                    self.cache
                        .stat(repo, &self.settings.base_ref(card.id), &turn.commit_sha)
                });
                (
                    card.lane,
                    context! {
                        stat => stat.filter(|s| s.additions + s.deletions > 0),
                        ..minijinja::Value::from_serialize(card)
                    },
                )
            })
            .collect();

        let lanes: Vec<_> = Lane::ALL
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
pub fn board(
    db: &State<Db>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    id: i64,
) -> Result<Tmpl, Status> {
    let project = Project::find(&db.lock(), id).ok_or(Status::NotFound)?;
    Ok(Shell {
        db,
        settings,
        cache,
    }
    .render(Some(project), NOTHING, context! {}))
}

/// The board with nothing selected — whichever project was added first, or an
/// empty shell asking for one.
#[get("/")]
pub fn index(db: &State<Db>, settings: &State<Settings>, cache: &State<DiffCache>) -> Tmpl {
    let project = Project::all(&db.lock()).into_iter().next();
    Shell {
        db,
        settings,
        cache,
    }
    .render(project, NOTHING, context! {})
}

/// The project switcher, over whichever board it was opened from.
#[get("/projects?<board>")]
pub fn switcher(
    db: &State<Db>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    board: Option<i64>,
) -> Tmpl {
    let project = current(db, board);
    Shell {
        db,
        settings,
        cache,
    }
    .render(project, PROJECTS, context! {})
}

#[get("/projects/<id>/cards/new")]
pub fn new_card(
    db: &State<Db>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    id: i64,
) -> Result<Tmpl, Status> {
    let project = Project::find(&db.lock(), id).ok_or(Status::NotFound)?;
    let branches = git::branches(&project.repo());

    Ok(Shell {
        db,
        settings,
        cache,
    }
    .render(
        Some(project),
        NEW_CARD,
        context! {
            branches,
            permission_modes => PERMISSION_MODES,
            models => MODELS,
            task => "",
            error => Option::<String>::None,
        },
    ))
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

#[post("/projects/<id>/cards", data = "<form>")]
pub fn create_card(
    db: &State<Db>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    id: i64,
    form: Form<CardForm>,
) -> Result<Redirect, Tmpl> {
    let project = match Project::find(&db.lock(), id) {
        Some(project) => project,
        None => return Ok(Redirect::to("/")),
    };

    let (title, description) = split_task(&form.task);
    if title.is_empty() {
        let branches = git::branches(&project.repo());
        return Err(Shell {
            db,
            settings,
            cache,
        }
        .render(
            Some(project),
            NEW_CARD,
            context! {
                branches,
                permission_modes => PERMISSION_MODES,
                models => MODELS,
                task => form.task.clone(),
                error => Some("A card needs a task for the agent."),
            },
        ));
    }

    let created = Card::create(
        &db.lock(),
        NewCard {
            project_id: id,
            title: &title,
            description: &description,
            base_branch: form.base_branch.trim(),
            permission_mode: permission_mode(&form.permission_mode),
            model: Some(form.model.trim()).filter(|m| !m.is_empty()),
        },
    );

    if created.is_err() {
        return Ok(Redirect::to(format!("/projects/{id}")));
    }

    Ok(match form.more {
        Some(_) => Redirect::to(format!("/projects/{id}/cards/new")),
        None => Redirect::to(format!("/projects/{id}")),
    })
}

/// The card's title and the agent's opening prompt, out of the one field the
/// form offers.
fn split_task(task: &str) -> (String, String) {
    let task = task.trim();
    let (first, rest) = task.split_once('\n').unwrap_or((task, ""));
    let first = first.trim();

    let title: String = match first.chars().count() > TITLE_MAX {
        true => first.chars().take(TITLE_MAX).collect::<String>() + "…",
        false => first.to_owned(),
    };

    // A shortened title is not the task any more, so the prompt carries the
    // whole thing rather than the remainder.
    let description = match title == first {
        true => rest.trim().to_owned(),
        false => task.to_owned(),
    };

    (title, description)
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
pub fn current(db: &State<Db>, id: Option<i64>) -> Option<Project> {
    let conn = db.lock();
    id.and_then(|id| Project::find(&conn, id))
        .or_else(|| Project::all(&conn).into_iter().next())
}

#[derive(rocket::FromForm)]
pub struct MoveForm {
    lane: String,
    #[field(default = 0)]
    index: usize,
}

#[post("/cards/<id>/move", data = "<form>")]
pub fn move_card(
    db: &State<Db>,
    agents: &State<Agents>,
    auth: &State<HookAuth>,
    settings: &State<Settings>,
    id: i64,
    form: Form<MoveForm>,
) -> Result<Status, Status> {
    relane(db, agents, auth, settings, id, &form.lane, form.index)?;
    Ok(Status::NoContent)
}

/// The same move from the card drawer, which stays open around it.
#[post("/cards/<id>/lane", data = "<form>")]
pub fn move_card_to_lane(
    db: &State<Db>,
    agents: &State<Agents>,
    auth: &State<HookAuth>,
    settings: &State<Settings>,
    id: i64,
    form: Form<MoveForm>,
) -> Result<Redirect, Status> {
    relane(db, agents, auth, settings, id, &form.lane, form.index)?;
    Ok(Redirect::to(format!("/cards/{id}")))
}

fn relane(
    db: &State<Db>,
    agents: &State<Agents>,
    auth: &State<HookAuth>,
    settings: &State<Settings>,
    id: i64,
    lane: &str,
    index: usize,
) -> Result<(), Status> {
    let lane = Lane::parse(lane);

    let conn = db.lock();
    let card = Card::find(&conn, id).ok_or(Status::NotFound)?;
    Card::reorder(&conn, id, card.project_id, lane, index);
    drop(conn);

    // Entering In Progress is what creates the worktree and starts the agent.
    // Re-entering it with a live agent is a no-op.
    if lane == Lane::InProgress && card.lane != Lane::InProgress {
        if let Err(err) = session::start(db, agents, auth, settings, id) {
            error!("card {id}: {err:#}");
            session::set_state(db, id, AgentState::Error);
        }
    }

    Ok(())
}

#[post("/cards/<id>/delete")]
pub fn delete_card(
    db: &State<Db>,
    agents: &State<Agents>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    id: i64,
) -> Result<Redirect, Status> {
    let project_id = {
        let conn = db.lock();
        Card::find(&conn, id).ok_or(Status::NotFound)?.project_id
    };

    session::teardown(db, agents, settings, cache, id);
    Card::delete(&db.lock(), id).map_err(|_| Status::InternalServerError)?;

    Ok(Redirect::to(format!("/projects/{project_id}")))
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

    #[test]
    fn a_one_line_task_is_the_whole_title() {
        assert_eq!(
            split_task("  Teach it to whistle  "),
            ("Teach it to whistle".to_owned(), String::new())
        );
    }

    #[test]
    fn the_rest_of_the_task_becomes_the_prompt() {
        let (title, description) = split_task("Teach it to whistle\n\nOn startup, in C.");
        assert_eq!(title, "Teach it to whistle");
        assert_eq!(description, "On startup, in C.");
    }

    #[test]
    fn a_long_first_line_is_shortened_but_not_lost() {
        let task = "x".repeat(TITLE_MAX + 20);
        let (title, description) = split_task(&task);

        assert_eq!(title.chars().count(), TITLE_MAX + 1);
        assert!(title.ends_with('…'));
        // The agent is still told the whole thing.
        assert_eq!(description, task);
    }

    #[test]
    fn an_empty_task_has_no_title_to_file_it_under() {
        assert_eq!(split_task("   \n  ").0, "");
    }
}
