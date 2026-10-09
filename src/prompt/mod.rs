//! Templates and actions: Jinja prompts the user edits in the settings modal.

mod prompt;

pub use prompt::{render, Kind, Prompt, Vars};

use minijinja::context;
use rocket::form::Form;
use rocket::http::Status;
use rocket::response::Redirect;
use rocket::{get, post, State};

use crate::config::Settings;
use crate::db::DB;
use crate::project::board::{self, Shell, SETTINGS};
use crate::review::DiffCache;
use crate::tmpl::Tmpl;

pub fn routes() -> Vec<rocket::Route> {
    rocket::routes![show, create, update, delete]
}

#[derive(rocket::FromForm)]
pub struct PromptForm {
    name: String,
    body: String,
    lands: bool,
}

fn url(kind: Kind, board: Option<i64>, id: Option<i64>) -> String {
    let mut url = format!("/settings/{}", kind.plural());
    let query: Vec<String> = [("board", board), ("id", id)]
        .into_iter()
        .filter_map(|(key, value)| value.map(|v| format!("{key}={v}")))
        .collect();
    if !query.is_empty() {
        url = format!("{url}?{}", query.join("&"));
    }
    url
}

fn failed(err: sqlx::Error) -> Status {
    error!("prompts: {err}");
    Status::InternalServerError
}

/// Renders the modal. `edited` holds a refused submission, so the user keeps
/// what they typed.
async fn modal(
    db: &DB,
    settings: &Settings,
    cache: &DiffCache,
    kind: Kind,
    board: Option<i64>,
    id: Option<i64>,
    edited: Option<(Prompt, String)>,
) -> Result<Tmpl, Status> {
    let prompts = Prompt::all(db, kind).await.map_err(failed)?;
    let (prompt, error) = match edited {
        Some((prompt, error)) => (Some(prompt), Some(error)),
        None => {
            let selected = prompts.iter().find(|p| Some(p.id) == id);
            (selected.or(prompts.first()).cloned(), None)
        }
    };
    let project = board::current(db, board).await;
    let base = format!("/settings/{}", kind.plural());
    let query = board.map(|b| format!("?board={b}")).unwrap_or_default();
    let prompt_id = prompt.as_ref().map(|p| p.id);
    let tabs: Vec<_> = Kind::ALL
        .iter()
        .map(|&tab| context! { label => tab.label(), href => url(tab, board, None), on => tab == kind })
        .collect();

    Ok(Shell {
        db,
        settings,
        cache,
    }
    .render(
        project,
        SETTINGS,
        context! {
            kind, prompts, prompt, error, board, tabs,
            label => kind.label(),
            base,
            new_href => format!("{base}{query}"),
            save_href => prompt_id.map(|id| format!("{base}/{id}{query}")),
            delete_href => prompt_id.map(|id| format!("{base}/{id}/delete{query}")),
        },
    )
    .await)
}

#[get("/settings/<kind>?<board>&<id>")]
pub async fn show(
    db: &State<DB>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    kind: Kind,
    board: Option<i64>,
    id: Option<i64>,
) -> Result<Tmpl, Status> {
    modal(db, settings, cache, kind, board, id, None).await
}

#[post("/settings/<kind>?<board>")]
pub async fn create(db: &State<DB>, kind: Kind, board: Option<i64>) -> Result<Redirect, Status> {
    let id = Prompt::create(db, kind).await.map_err(failed)?;
    Ok(Redirect::to(url(kind, board, Some(id))))
}

#[post("/settings/<kind>/<id>?<board>", data = "<form>")]
pub async fn update(
    db: &State<DB>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    kind: Kind,
    id: i64,
    board: Option<i64>,
    form: Form<PromptForm>,
) -> Result<Result<Redirect, (Status, Tmpl)>, Status> {
    let name = form.name.trim();
    let complaint = match name.is_empty() {
        true => Some("A name is required.".to_owned()),
        false => prompt::check(&form.body).err(),
    };

    if let Some(complaint) = complaint {
        let edited = Prompt {
            id,
            kind,
            name: form.name.clone(),
            body: form.body.clone(),
            lands: form.lands && kind == Kind::Action,
        };
        let page = modal(
            db,
            settings,
            cache,
            kind,
            board,
            Some(id),
            Some((edited, complaint)),
        )
        .await?;
        return Ok(Err((Status::UnprocessableEntity, page)));
    }

    let found = Prompt::update(db, kind, id, name, &form.body, form.lands)
        .await
        .map_err(failed)?;
    if !found {
        return Err(Status::NotFound);
    }
    Ok(Ok(Redirect::to(url(kind, board, Some(id)))))
}

#[post("/settings/<kind>/<id>/delete?<board>")]
pub async fn delete(
    db: &State<DB>,
    kind: Kind,
    id: i64,
    board: Option<i64>,
) -> Result<Redirect, Status> {
    if !Prompt::delete(db, kind, id).await.map_err(failed)? {
        return Err(Status::NotFound);
    }
    Ok(Redirect::to(url(kind, board, None)))
}
