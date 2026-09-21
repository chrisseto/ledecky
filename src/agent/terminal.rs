use std::sync::Arc;

use minijinja::context;
use rocket::form::Form;
use rocket::futures::{SinkExt, StreamExt};
use rocket::http::Status;
use rocket::response::Redirect;
use rocket::{get, post, State};
use rocket_ws as ws;
use tokio::sync::broadcast::error::RecvError;

use crate::agent::AgentManager;
use crate::config::Settings;
use crate::db::Db;
use crate::project::board::{self, Shell};
use crate::project::lifecycle;
use crate::project::{Card, Project};
use crate::review::{self, DiffCache};
use crate::tmpl::Tmpl;
use crate::watch::Worktrees;

#[get("/cards/<id>?<scope>")]
pub async fn focus(
    db: &State<Db>,
    manager: &State<Arc<AgentManager>>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    id: i64,
    scope: Option<&str>,
) -> Result<Tmpl, Status> {
    let card = Card::find(db, id).await.ok_or(Status::NotFound)?;
    let project = Project::find(db, card.project_id)
        .await
        .ok_or(Status::NotFound)?;

    let live = manager.running(id).is_some();
    let review = review::routes::initial(db, settings, cache, id, scope).await?;

    Ok(Shell {
        db,
        settings,
        cache,
    }
    .render(
        Some(project),
        board::CARD,
        context! { live, editable => card.editable(), ..review },
    )
    .await)
}

/// Just the agent-state chip, so a state change redraws it without re-running a
/// diff behind it.
#[get("/cards/<id>/state")]
pub async fn state(db: &State<Db>, id: i64) -> Result<Tmpl, Status> {
    let card = Card::find(db, id).await.ok_or(Status::NotFound)?;
    Ok(Tmpl("_state.html", context! { card }))
}

/// Just the drawer's agent pane, for the same reason as [`state`].
///
/// Whether a terminal belongs on screen turns on the agent being up, which is
/// not something the card records — so this is the one question the pane has,
/// and the only one it asks. It used to be answered by rendering `/cards/<id>`
/// and selecting out of it, which is a board and a review pane's worth of git.
#[get("/cards/<id>/agent")]
pub async fn agent_pane(
    db: &State<Db>,
    manager: &State<Arc<AgentManager>>,
    id: i64,
) -> Result<Tmpl, Status> {
    let card = Card::find(db, id).await.ok_or(Status::NotFound)?;
    let live = manager.running(id).is_some();
    Ok(Tmpl("_pane_agent.html", context! { card, live }))
}

#[post("/cards/<id>/start")]
pub async fn start(
    manager: &State<Arc<AgentManager>>,
    settings: &State<Settings>,
    worktrees: &State<Worktrees>,
    id: i64,
) -> Result<Redirect, Status> {
    match lifecycle::start(manager, settings, worktrees, id).await {
        // The drawer is what asked, and it has a terminal to put up now.
        Ok(_) => Ok(Redirect::to(format!("/cards/{id}"))),
        Err(err) => {
            error!("card {id}: {err:#}");
            Err(Status::InternalServerError)
        }
    }
}

#[post("/cards/<id>/stop")]
pub async fn stop(manager: &State<Arc<AgentManager>>, id: i64) -> Redirect {
    manager.stop(id).await;
    Redirect::to(format!("/cards/{id}"))
}

#[post("/cards/<id>/merge")]
pub async fn merge(manager: &State<Arc<AgentManager>>, id: i64) -> Result<Redirect, Status> {
    match manager.request_merge(id).await {
        Ok(()) => Ok(Redirect::to(format!("/cards/{id}"))),
        Err(err) => {
            warn!("card {id}: merge request failed: {err:#}");
            Err(Status::Conflict)
        }
    }
}

#[derive(rocket::FromForm)]
pub struct ResizeForm {
    rows: u16,
    cols: u16,
}

#[post("/cards/<id>/resize", data = "<form>")]
pub async fn resize(manager: &State<Arc<AgentManager>>, id: i64, form: Form<ResizeForm>) -> Status {
    match manager.running(id) {
        Some(agent) => {
            agent.resize(form.rows.max(1), form.cols.max(1)).await;
            Status::NoContent
        }
        None => Status::NotFound,
    }
}

/// Raw pty bytes in both directions, and the only thing that writes to the pty
/// at all — a keystroke here is the user's. Everything else — resize, merge, a
/// review — goes over ordinary HTTP so this socket stays a dumb pipe. The
/// screen's size is the one exception: the replay ahead of the stream is
/// rendered at the pty's width, so it has to be the client's before the first
/// byte goes out.
#[get("/cards/<id>/terminal?<rows>&<cols>")]
pub fn socket(
    manager: &State<Arc<AgentManager>>,
    id: i64,
    rows: u16,
    cols: u16,
    socket: ws::WebSocket,
) -> ws::Channel<'static> {
    let agent = manager.get(id);

    socket.channel(move |mut stream| {
        Box::pin(async move {
            let Some(agent) = agent else {
                let _ = stream.close(None).await;
                return Ok(());
            };

            agent.resize(rows.max(1), cols.max(1)).await;

            // Subscribe before snapshotting so no output slips through the gap.
            let mut rx = agent.subscribe();
            stream
                .send(ws::Message::Binary(agent.history(Some(rows))))
                .await?;
            stream.send(ws::Message::Binary(agent.snapshot())).await?;

            loop {
                tokio::select! {
                    incoming = stream.next() => match incoming {
                        Some(Ok(ws::Message::Binary(bytes))) => agent.write_input(&bytes).await,
                        Some(Ok(ws::Message::Text(text))) => agent.write_input(text.as_bytes()).await,
                        Some(Ok(_)) => {}
                        Some(Err(_)) | None => break,
                    },
                    chunk = rx.recv() => match chunk {
                        Ok(bytes) => stream.send(ws::Message::Binary(bytes.to_vec())).await?,
                        // Dropped bytes would desync the client, so repaint instead.
                        Err(RecvError::Lagged(_)) => {
                            stream.send(ws::Message::Binary(agent.snapshot())).await?;
                        }
                        Err(RecvError::Closed) => break,
                    },
                }
            }

            Ok(())
        })
    })
}
