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
mod gzip;
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
    if let Err(err) = launch().await {
        eprintln!("ledecky: {err}");
        std::process::exit(1);
    }
}

async fn launch() -> anyhow::Result<()> {
    rocket().await?.launch().await?;
    Ok(())
}

/// NB: `async`, and so not `#[launch]`, which hands Rocket a synchronous
/// builder and runs it for us. Opening the database awaits, and so does the
/// orphan sweep behind it.
async fn rocket() -> anyhow::Result<rocket::Rocket<rocket::Build>> {
    // Rocket's own figment reads `Rocket.toml` and `ROCKET_*`; layering
    // `LEDECKY_*` on top gives this app's keys an override that reads naturally
    // and does not collide with Rocket's.
    let figment = rocket::Config::figment().merge(Env::prefixed("LEDECKY_").global());
    let rocket = rocket::custom(&figment);

    let settings = Settings::from(&figment).context("reading settings")?;
    let db = db::Db::open(&settings)
        .await
        .context("opening the database")?;
    let templates = tmpl::Templates::load().context("loading templates")?;

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
    manager.sweep_orphans().await;
    let worktrees = watch::Worktrees::new(
        db.clone(),
        settings.clone(),
        cache.clone(),
        changes.clone(),
        std::time::Duration::from_millis(settings.watch_debounce),
    );

    let rocket = if settings.gzip {
        rocket.attach(gzip::Gzip)
    } else {
        rocket
    };

    Ok(rocket
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
        })))
}
