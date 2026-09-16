#[macro_use]
extern crate rocket;

mod agent;
mod config;
mod db;
mod diff;
mod git;
mod hooks;
mod models;
mod queries;
mod routes;
mod session;
mod tmpl;

use anyhow::Context;
use rocket::fairing::AdHoc;
use rocket::fs::FileServer;

#[launch]
fn rocket() -> _ {
    let db = db::Db::open()
        .context("opening the database")
        .unwrap_or_else(|e| panic!("{e:#}"));

    let templates = tmpl::Templates::load()
        .context("loading templates")
        .unwrap_or_else(|e| panic!("{e:#}"));

    let rocket = rocket::build();
    let port: u16 = rocket.figment().extract_inner("port").unwrap_or(8000);

    rocket
        .manage(db)
        .manage(templates)
        .manage(agent::Agents::default())
        .manage(hooks::HookAuth::new(port))
        .mount("/static", FileServer::from("static"))
        .mount(
            "/",
            routes![
                routes::projects::index,
                routes::projects::new,
                routes::projects::create,
                routes::projects::complete,
                routes::board::board,
                routes::board::new_card,
                routes::board::create_card,
                routes::board::move_card,
                routes::board::delete_card,
                routes::card::focus,
                routes::card::state,
                routes::card::merge,
                routes::card::start,
                routes::card::stop,
                routes::card::resize,
                routes::card::terminal,
                routes::hooks::receive,
                routes::review::diff_pane,
                routes::review::add_comment,
                routes::review::delete_comment,
                routes::review::submit_review,
            ],
        )
        // Live agents are children of this process; leaving them behind on exit
        // would strand worktrees with nothing driving them.
        .attach(AdHoc::on_shutdown("kill agents", |rocket| {
            Box::pin(async move {
                if let Some(agents) = rocket.state::<agent::Agents>() {
                    agents.shutdown();
                }
            })
        }))
}
