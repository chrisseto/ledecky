use crate::config::Settings;
use crate::review::Turn;

/// What slice of a card's history the diff pane is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Everything the agent has done: base -> latest turn.
    All,
    /// One turn in isolation: turn n-1 -> turn n.
    Turn(i64),
    /// Everything after a turn: turn n -> latest.
    Since(i64),
}

impl Scope {
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.and_then(|s| s.split_once('-')) {
            Some(("turn", n)) => n.parse().map(Self::Turn).unwrap_or(Self::All),
            Some(("since", n)) => n.parse().map(Self::Since).unwrap_or(Self::All),
            _ => Self::All,
        }
    }

    pub fn key(self) -> String {
        match self {
            Self::All => "all".into(),
            Self::Turn(n) => format!("turn-{n}"),
            Self::Since(n) => format!("since-{n}"),
        }
    }

    pub fn label(self) -> String {
        match self {
            Self::All => "All changes".into(),
            Self::Turn(n) => format!("Turn {n}"),
            Self::Since(n) => format!("Since turn {n}"),
        }
    }

    /// The pair of revisions to diff, or `None` when there is nothing yet.
    pub fn revisions(
        self,
        settings: &Settings,
        card_id: i64,
        turns: &[Turn],
    ) -> Option<(String, String)> {
        let latest = turns.last()?;
        let base = settings.base_ref(card_id);
        let at = |n: i64| {
            turns
                .iter()
                .find(|t| t.n == n)
                .map(|t| t.commit_sha.clone())
        };

        match self {
            Self::All => Some((base, latest.commit_sha.clone())),
            Self::Turn(n) => {
                let to = at(n)?;
                // Turn 1 has no predecessor, so it is measured from the base.
                let from = at(n - 1).unwrap_or(base);
                Some((from, to))
            }
            Self::Since(n) => Some((at(n)?, latest.commit_sha.clone())),
        }
    }

    /// Every scope offered for a card, in menu order.
    pub fn menu(turns: &[Turn]) -> Vec<Self> {
        let mut out = vec![Self::All];
        for turn in turns {
            out.push(Self::Turn(turn.n));
            out.push(Self::Since(turn.n));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rocket::figment::providers::Serialized;

    fn settings() -> Settings {
        Settings::from(
            &rocket::figment::Figment::new()
                .merge(Serialized::default("app_slug", "ledecky"))
                .merge(Serialized::default("data_dir", "/tmp/x")),
        )
        .unwrap()
    }

    fn turn(n: i64, sha: &str) -> Turn {
        Turn {
            id: n,
            n,
            commit_sha: sha.into(),
            parent_sha: String::new(),
            last_assistant_message: None,
            created_at: String::new(),
        }
    }

    #[test]
    fn scopes_round_trip_through_their_key() {
        for scope in [Scope::All, Scope::Turn(3), Scope::Since(2)] {
            assert_eq!(Scope::parse(Some(&scope.key())), scope);
        }
    }

    #[test]
    fn unparseable_scopes_fall_back_to_all() {
        assert_eq!(Scope::parse(None), Scope::All);
        assert_eq!(Scope::parse(Some("nonsense")), Scope::All);
        assert_eq!(Scope::parse(Some("turn-x")), Scope::All);
        assert_eq!(Scope::parse(Some("since-")), Scope::All);
    }

    #[test]
    fn all_spans_base_to_the_latest_turn() {
        let turns = [turn(1, "sha1"), turn(2, "sha2")];
        assert_eq!(
            Scope::All.revisions(&settings(), 7, &turns),
            Some(("refs/ledecky/7/base".into(), "sha2".into()))
        );
    }

    #[test]
    fn the_first_turn_is_measured_from_the_base() {
        let turns = [turn(1, "sha1"), turn(2, "sha2")];
        assert_eq!(
            Scope::Turn(1).revisions(&settings(), 7, &turns),
            Some(("refs/ledecky/7/base".into(), "sha1".into()))
        );
    }

    #[test]
    fn a_later_turn_is_measured_from_its_predecessor() {
        let turns = [turn(1, "sha1"), turn(2, "sha2")];
        assert_eq!(
            Scope::Turn(2).revisions(&settings(), 7, &turns),
            Some(("sha1".into(), "sha2".into()))
        );
        assert_eq!(
            Scope::Since(1).revisions(&settings(), 7, &turns),
            Some(("sha1".into(), "sha2".into()))
        );
    }

    #[test]
    fn a_scope_naming_a_turn_that_does_not_exist_has_no_revisions() {
        let turns = [turn(1, "sha1")];
        assert_eq!(Scope::Turn(9).revisions(&settings(), 7, &turns), None);
        assert_eq!(Scope::Since(9).revisions(&settings(), 7, &turns), None);
    }

    #[test]
    fn a_card_with_no_turns_has_nothing_to_show() {
        assert_eq!(Scope::All.revisions(&settings(), 7, &[]), None);
    }

    #[test]
    fn the_menu_offers_both_views_of_every_turn() {
        let menu = Scope::menu(&[turn(1, "a"), turn(2, "b")]);
        let keys: Vec<_> = menu.iter().map(|s| s.key()).collect();
        assert_eq!(keys, ["all", "turn-1", "since-1", "turn-2", "since-2"]);

        assert_eq!(Scope::menu(&[]).len(), 1);
    }
}
