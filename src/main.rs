// Each entity lives in the file it is named for — `Card` in `card.rs`, `Project`
// in `project.rs`, `Agent` in `agent.rs` — which collides with the domain folder
// for the two whose name matches it. The convention is the point; see Layout in
// README.md.
#![allow(clippy::module_inception)]

#[macro_use]
extern crate rocket;

mod agent;
mod assets;
mod config;
mod db;
mod events;
mod git;
mod hooks;
mod project;
mod review;
mod tmpl;
mod watch;

use std::sync::Arc;

use anyhow::Context;
use rocket::fairing::AdHoc;
use rocket::figment::providers::Env;

use crate::config::Settings;

/// NB: hooks are checked before anything else runs. The `SessionStart` hook
/// re-executes this binary (`hooks::dispatch`), and that process must not open
/// the database, sweep orphans or bind a port on its way to sending one request.
#[rocket::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(result) = hooks::dispatch(&args) {
        if let Err(err) = result {
            eprintln!("ledecky hook: {err:#}");
            std::process::exit(1);
        }
        return;
    }

    // NB: reported and exited rather than returned. `rocket::Error` is large
    // enough that returning it from `main` is a lint of its own, and the
    // `Debug` rendering a returned `Err` gets is worse than this anyway.
    if let Err(err) = rocket().launch().await {
        eprintln!("ledecky: {err}");
        std::process::exit(1);
    }
}

fn rocket() -> rocket::Rocket<rocket::Build> {
    // Rocket's own figment reads `Rocket.toml` and `ROCKET_*`; layering
    // `LEDECKY_*` on top gives this app's keys an override that reads naturally
    // and does not collide with Rocket's.
    let figment = rocket::Config::figment().merge(Env::prefixed("LEDECKY_").global());
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

    // The watcher and the manager need their own handles on these, so they are
    // built here rather than inline in `manage`.
    let cache = review::DiffCache::default();
    let changes = events::Changes::default();

    let manager = agent::AgentManager::new(
        db.clone(),
        changes.clone(),
        hooks::HookAuth::new(),
        settings.clone(),
    );

    // A previous run may have been killed without getting to its shutdown hook.
    manager.sweep_orphans();
    let worktrees = watch::Worktrees::new(
        cache.clone(),
        changes.clone(),
        std::time::Duration::from_millis(settings.watch_debounce),
    );

    rocket
        .manage(settings)
        .manage(db)
        .manage(templates)
        .manage(Arc::clone(&manager))
        .manage(cache)
        .manage(changes)
        .manage(worktrees)
        .mount("/static", assets::routes())
        .mount("/", rocket::routes![events::stream])
        .mount("/", project::routes())
        .mount("/", agent::routes())
        .mount("/", review::routes())
        // Binds the hook port on liftoff and takes the agents down on shutdown.
        .attach(manager)
        .attach(AdHoc::on_liftoff("banner", |rocket| {
            Box::pin(async move {
                let config = rocket.config();
                println!(
                    "ledecky listening on http://{}:{}",
                    config.address, config.port
                );
            })
        }))
}
