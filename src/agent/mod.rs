//! Running `claude` against a card: the pty, the session lifecycle, and the
//! hooks it calls back on.

pub mod agent;
pub mod session;
pub mod terminal;
pub mod webhooks;

pub use agent::{Agent, Agents};

/// Every route this domain serves.
pub fn routes() -> Vec<rocket::Route> {
    rocket::routes![
        terminal::focus,
        terminal::state,
        terminal::start,
        terminal::stop,
        terminal::resize,
        terminal::merge,
        terminal::socket,
        webhooks::receive,
    ]
}
