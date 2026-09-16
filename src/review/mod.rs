//! Reviewing an agent's work: turn snapshots, the diff they scope, and the
//! comments that go back to the agent.

pub mod comment;
pub mod diff;
pub mod routes;
pub mod scope;
pub mod turn;

pub use comment::Comment;
pub use scope::Scope;
pub use turn::Turn;

/// Every route this domain serves.
pub fn routes() -> Vec<rocket::Route> {
    rocket::routes![
        routes::diff_pane,
        routes::add_comment,
        routes::delete_comment,
        routes::submit_review,
    ]
}
