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
mod gzip;
mod hooks;
mod listener;
mod project;
mod review;
mod tmpl;
mod vcs;
mod watch;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use rocket::figment::Figment;
use rocket::listener::tcp::TcpListener;

use crate::config::Settings;
use crate::listener::All;

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

    // NB: reported and exited rather than returned: the `Debug` rendering a
    // returned `Err` gets is worse than this.
    if let Err(err) = launch(&args).await {
        eprintln!("ledecky: {err:#}");
        std::process::exit(1);
    }
}

/// One Rocket on two sockets bound here: the board on the configured address,
/// and the hooks agents call back on over loopback only.
async fn launch(args: &[String]) -> anyhow::Result<()> {
    let figment = config::figment(config::config_file(args)?.as_deref());
    let settings = Settings::from(&figment).context("reading settings")?;
    // NB: Rocket's own keys ride the same figment, and the error it raises for
    // a bad one names no key and no value.
    figment
        .extract::<rocket::Config>()
        .context("reading Rocket's settings")?;

    let board = TcpListener::bind((settings.address, settings.port))
        .await
        .with_context(|| format!("binding {}:{}", settings.address, settings.port))?;
    let hook_addr = (settings.hook_address, settings.hook_port);
    let hooks = TcpListener::bind(hook_addr)
        .await
        .with_context(|| format!("binding {}:{}", hook_addr.0, hook_addr.1))?;

    // Where hook callbacks are served, which only the bound listener knows: a
    // configured port of 0 asks for a free one.
    let hooks_at = hooks.local_addr()?;

    rocket(figment, settings, hooks_at)
        .await?
        .launch_on(All::new(vec![board, hooks]))
        .await
        // NB: `rocket::Error` is not `Sync`, and so is not an `anyhow::Error`.
        // Rocket has already logged whatever detail it carries.
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    Ok(())
}

/// NB: `async`, and so not `#[launch]`, which hands Rocket a synchronous
/// builder and runs it for us. Opening the database awaits, and so does the
/// orphan sweep behind it.
async fn rocket(
    figment: Figment,
    settings: Settings,
    hooks_at: SocketAddr,
) -> anyhow::Result<rocket::Rocket<rocket::Build>> {
    let db = db::DB::open(&settings)
        .await
        .context("opening the database")?;
    let templates = tmpl::Templates::load().context("loading templates")?;

    // The watcher and the manager need their own handles on these, so they are
    // built here rather than inline in `manage`.
    let cache = review::DiffCache::default();
    let changes = events::Changes::default();

    let auth = hooks::HookAuth::new(hooks_at);
    let manager = agent::AgentManager::new(db.clone(), changes.clone(), auth, settings.clone());

    // A previous run may have been killed without getting to its shutdown hook.
    manager.sweep_orphans().await;
    let worktrees = watch::Worktrees::new(
        db.clone(),
        settings.clone(),
        cache.clone(),
        changes.clone(),
        std::time::Duration::from_millis(settings.watch_debounce),
    );

    let rocket = rocket::custom(&figment);
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
        .mount("/", agent::webhooks::routes())
        .attach(manager))
}
