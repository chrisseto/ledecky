//! Reviewing an agent's work: turn snapshots, the diff they scope, and the
//! comments that go back to the agent.

pub mod ansi;
pub mod cache;
pub mod comment;
pub mod diff;
pub mod routes;
pub mod scope;
pub mod turn;

pub use cache::DiffCache;
pub use comment::Comment;
pub use scope::Scope;
pub use turn::Turn;

/// Context windows the diff pane offers, in lines either side of a change.
pub const CONTEXT_CHOICES: &[(usize, &str)] = &[
    (3, "3 lines"),
    (10, "10 lines"),
    (usize::MAX, "Whole file"),
];

pub const DEFAULT_CONTEXT: usize = 3;

/// Clamps a requested context to one we offer, so the query parameter cannot ask
/// for arbitrary work.
pub fn context_lines(requested: Option<&str>) -> usize {
    requested
        .and_then(|raw| raw.parse().ok())
        .filter(|n| CONTEXT_CHOICES.iter().any(|(choice, _)| choice == n))
        .unwrap_or(DEFAULT_CONTEXT)
}

/// Every route this domain serves.
pub fn routes() -> Vec<rocket::Route> {
    rocket::routes![
        routes::diff_pane,
        routes::add_comment,
        routes::delete_comment,
        routes::submit_review,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_or_unknown_context_falls_back_to_the_default() {
        assert_eq!(context_lines(None), DEFAULT_CONTEXT);
        assert_eq!(context_lines(Some("nonsense")), DEFAULT_CONTEXT);
        // Not one of the offered choices, so not honoured.
        assert_eq!(context_lines(Some("7")), DEFAULT_CONTEXT);
        assert_eq!(context_lines(Some("999999")), DEFAULT_CONTEXT);
    }

    #[test]
    fn offered_choices_are_honoured() {
        assert_eq!(context_lines(Some("3")), 3);
        assert_eq!(context_lines(Some("10")), 10);
        assert_eq!(context_lines(Some(&usize::MAX.to_string())), usize::MAX);
    }
}
