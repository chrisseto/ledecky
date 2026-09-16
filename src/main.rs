#[macro_use]
extern crate rocket;

mod agent;
mod config;
mod db;
mod git;
mod hooks;
mod project;
mod review;
mod tmpl;

use anyhow::Context;
use rocket::fairing::AdHoc;
use rocket::figment::providers::Env;
use rocket::fs::FileServer;

use crate::config::Settings;

#[launch]
fn rocket() -> _ {
    // Rocket's own figment reads `Rocket.toml` and `ROCKET_*`; layering
    // `KANBAN2_*` on top gives this app's keys an override that reads naturally
    // and does not collide with Rocket's.
    let figment = rocket::Config::figment().merge(Env::prefixed("KANBAN2_").global());
    let rocket = rocket::custom(&figment);

    let settings = Settings::from(&figment)
        .context("reading settings")
        .unwrap_or_else(|err| panic!("{err:#}"));

    let db = db::Db::open(&settings)
        .context("opening the database")
        .unwrap_or_else(|err| panic!("{err:#}"));

    let templates = tmpl::Templates::load()
        .context("loading templates")
        .unwrap_or_else(|err| panic!("{err:#}"));

    // A previous run may have been killed without getting to its shutdown hook.
    agent::session::sweep_orphans(&db, &settings);

    let port: u16 = figment.extract_inner("port").unwrap_or(8000);

    rocket
        .manage(settings)
        .manage(db)
        .manage(templates)
        .manage(agent::Agents::default())
        .manage(review::DiffCache::default())
        .manage(hooks::HookAuth::new(port))
        .mount("/static", FileServer::from("static"))
        .mount("/", project::routes())
        .mount("/", agent::routes())
        .mount("/", review::routes())
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
