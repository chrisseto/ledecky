use minijinja::context;
use rocket::form::Form;
use rocket::futures::{SinkExt, StreamExt};
use rocket::http::Status;
use rocket::response::Redirect;
use rocket::{get, post, State};
use rocket_ws as ws;
use tokio::sync::broadcast::error::RecvError;

use crate::agent::{session, Agents};
use crate::config::Settings;
use crate::db::Db;
use crate::hooks::HookAuth;
use crate::project::board::{self, Shell};
use crate::project::{Card, Project};
use crate::review::{self, DiffCache};
use crate::tmpl::Tmpl;

#[get("/cards/<id>?<scope>")]
pub fn focus(
    db: &State<Db>,
    agents: &State<Agents>,
    settings: &State<Settings>,
    cache: &State<DiffCache>,
    id: i64,
    scope: Option<&str>,
) -> Result<Tmpl, Status> {
    let conn = db.lock();
    let card = Card::find(&conn, id).ok_or(Status::NotFound)?;
    let project = Project::find(&conn, card.project_id).ok_or(Status::NotFound)?;
    drop(conn);

    let live = agents.get(id).is_some_and(|a| a.is_running());
    let review = review::routes::initial(db, settings, cache, id, scope)?;

    Ok(Shell {
        db,
        settings,
        cache,
    }
    .render(Some(project), board::CARD, context! { live, ..review }))
}

/// Just the agent-state chip, so the focus view can poll it without re-running a
/// diff every few seconds.
#[get("/cards/<id>/state")]
pub fn state(db: &State<Db>, settings: &State<Settings>, id: i64) -> Result<Tmpl, Status> {
    let card = Card::find(&db.lock(), id).ok_or(Status::NotFound)?;
    Ok(Tmpl(
        "_state.html",
        context! { card, poll_interval => settings.poll_interval },
    ))
}

#[post("/cards/<id>/start")]
pub fn start(
    db: &State<Db>,
    agents: &State<Agents>,
    auth: &State<HookAuth>,
    settings: &State<Settings>,
    id: i64,
) -> Result<Redirect, Status> {
    match session::start(db, agents, auth, settings, id) {
        // The drawer is what asked, and it has a terminal to put up now.
        Ok(_) => Ok(Redirect::to(format!("/cards/{id}"))),
        Err(err) => {
            error!("card {id}: {err:#}");
            Err(Status::InternalServerError)
        }
    }
}

#[post("/cards/<id>/stop")]
pub fn stop(db: &State<Db>, agents: &State<Agents>, id: i64) -> Redirect {
    session::stop(db, agents, id);
    Redirect::to(format!("/cards/{id}"))
}

#[post("/cards/<id>/merge")]
pub fn merge(db: &State<Db>, agents: &State<Agents>, id: i64) -> Result<Redirect, Status> {
    match session::request_merge(db, agents, id) {
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
pub fn resize(agents: &State<Agents>, id: i64, form: Form<ResizeForm>) -> Status {
    match agents.get(id) {
        Some(agent) => {
            agent.resize(form.rows.max(1), form.cols.max(1));
            Status::NoContent
        }
        None => Status::NotFound,
    }
}

/// Raw pty bytes in both directions. Everything else — resize, injection, merge —
/// goes over ordinary HTTP so this socket stays a dumb pipe. `rows` is the one
/// exception: the scrollback sent ahead of the snapshot has to be sized to the
/// client's screen, and that has to be known before the first byte goes out.
#[get("/cards/<id>/terminal?<rows>")]
pub fn socket(
    agents: &State<Agents>,
    id: i64,
    rows: Option<u16>,
    socket: ws::WebSocket,
) -> ws::Channel<'static> {
    let agent = agents.get(id);

    socket.channel(move |mut stream| {
        Box::pin(async move {
            let Some(agent) = agent else {
                let _ = stream.close(None).await;
                return Ok(());
            };

            // Subscribe before snapshotting so no output slips through the gap.
            let mut rx = agent.subscribe();
            stream
                .send(ws::Message::Binary(agent.history(rows)))
                .await?;
            stream.send(ws::Message::Binary(agent.snapshot())).await?;

            loop {
                tokio::select! {
                    incoming = stream.next() => match incoming {
                        Some(Ok(ws::Message::Binary(bytes))) => agent.write_input(&bytes),
                        Some(Ok(ws::Message::Text(text))) => agent.write_input(text.as_bytes()),
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
