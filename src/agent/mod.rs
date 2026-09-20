//! Running `claude` against a card: the pty, the registry that owns the live
//! agents and their state, and the hooks they call back on.

pub mod agent;
pub mod manager;
pub mod messaging;
pub mod terminal;
pub mod webhooks;

pub use agent::Agent;
pub use manager::AgentManager;

/// Every route this domain serves.
pub fn routes() -> Vec<rocket::Route> {
    rocket::routes![
        terminal::focus,
        terminal::state,
        terminal::agent_pane,
        terminal::start,
        terminal::stop,
        terminal::resize,
        terminal::merge,
        terminal::socket,
        webhooks::receive,
        webhooks::session_start,
    ]
}
