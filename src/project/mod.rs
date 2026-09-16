//! Projects, the cards on their boards, and the board view itself.

pub mod board;
pub mod card;
pub mod project;

pub use card::{AgentState, Card, Lane, NewCard};
pub use project::Project;

/// Every route this domain serves.
pub fn routes() -> Vec<rocket::Route> {
    rocket::routes![
        project::index,
        project::new,
        project::create,
        project::complete,
        board::board,
        board::new_card,
        board::create_card,
        board::move_card,
        board::delete_card,
    ]
}
