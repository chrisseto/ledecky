//! Projects, the cards on their boards, and the board view itself.

pub mod board;
pub mod card;
pub mod lifecycle;
pub mod project;

pub use card::{AgentState, Card, CardEdit, Lane, NewCard};
pub use project::Project;

/// Every route this domain serves.
pub fn routes() -> Vec<rocket::Route> {
    rocket::routes![
        project::new,
        project::create,
        project::complete,
        board::index,
        board::board,
        board::switcher,
        board::new_card,
        board::create_card,
        board::edit_card,
        board::update_card,
        board::move_card,
        board::move_card_to_lane,
        board::delete_card,
        board::collect_garbage,
    ]
}
