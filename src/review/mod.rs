//! Reviewing an agent's work: turn snapshots, the diff they scope, and the
//! comments that go back to the agent.

pub mod ansi;
pub mod cache;
pub mod comment;
pub mod diff;
pub mod expand;
pub mod routes;
pub mod scope;
pub mod turn;
pub mod viewed;

pub use cache::DiffCache;
pub use comment::Comment;
pub use expand::Expansion;
pub use scope::Scope;
pub use turn::Turn;
pub use viewed::Viewed;

/// Every route this domain serves.
pub fn routes() -> Vec<rocket::Route> {
    rocket::routes![
        routes::diff_pane,
        routes::add_comment,
        routes::delete_comment,
        routes::discard_comments,
        routes::toggle_viewed,
        routes::submit_review,
    ]
}
