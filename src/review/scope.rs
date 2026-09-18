use crate::config::Settings;
use crate::git::Commit;
use crate::review::Turn;

/// The tree with nothing in it, which is what a root commit is measured against.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// A point in a card's history the diff is measured from.
///
/// The picker lists these newest first, and the mode beside it decides which
/// side of the chosen one is on screen — so N points and one toggle stand in
/// for the 2N+1 ranges they describe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Anchor {
    /// The worktree as it stands, committed or not.
    Live,
    /// One of the agent's own commits.
    Commit(String),
    /// One turn snapshot.
    Turn(i64),
    /// Where the card branched off.
    Base,
}

/// Which side of the anchor is being read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// What this point alone changed.
    Just,
    /// Everything from this point to the live head.
    Since,
}

/// What slice of a card's history the diff pane is showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub anchor: Anchor,
    pub mode: Mode,
}

/// One row of the picker.
pub struct Entry {
    pub anchor: Anchor,
    pub label: String,
    /// Names the colour the row is tagged with: `live`, `commit`, `turn`, `base`.
    pub kind: &'static str,
}

impl Mode {
    pub const ALL: &'static [Self] = &[Self::Just, Self::Since];

    pub fn key(self) -> &'static str {
        match self {
            Self::Just => "just",
            Self::Since => "since",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Just => "Just this",
            Self::Since => "Since this",
        }
    }

    /// The other half of the toggle, for an anchor that does not offer this one.
    pub fn other(self) -> Self {
        match self {
            Self::Just => Self::Since,
            Self::Since => Self::Just,
        }
    }
}

impl Anchor {
    pub fn key(&self) -> String {
        match self {
            Self::Live => "live".into(),
            Self::Commit(sha) => format!("commit-{sha}"),
            Self::Turn(n) => format!("turn-{n}"),
            Self::Base => "base".into(),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Commit(_) => "commit",
            Self::Turn(_) => "turn",
            Self::Base => "base",
        }
    }

    /// Whether a mode says anything about this anchor.
    ///
    /// Nothing comes after the worktree, and the commit the card branched from
    /// is not part of the card's work.
    pub fn offers(&self, mode: Mode) -> bool {
        !matches!(
            (self, mode),
            (Self::Live, Mode::Since) | (Self::Base, Mode::Just)
        )
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw.split_once('-') {
            Some(("commit", sha)) => is_sha(sha).then(|| Self::Commit(sha.to_owned())),
            Some(("turn", n)) => n.parse().ok().map(Self::Turn),
            _ => match raw {
                "live" => Some(Self::Live),
                "base" => Some(Self::Base),
                _ => None,
            },
        }
    }
}

/// A rev we are willing to hand to `git diff`.
///
/// NB: not cosmetic. The key comes off the query string, and a rev beginning
/// with `-` is read by git as a flag rather than a revision.
fn is_sha(raw: &str) -> bool {
    (4..=64).contains(&raw.len()) && raw.chars().all(|c| c.is_ascii_hexdigit())
}

impl Default for Scope {
    /// Everything the agent has done, up to and including what it has not
    /// committed.
    fn default() -> Self {
        Self {
            anchor: Anchor::Base,
            mode: Mode::Since,
        }
    }
}

impl Scope {
    pub fn parse(raw: Option<&str>) -> Self {
        let parsed = raw.and_then(|raw| raw.split_once('-')).and_then(|split| {
            let mode = match split.0 {
                "just" => Mode::Just,
                "since" => Mode::Since,
                _ => return None,
            };
            let anchor = Anchor::parse(split.1)?;
            anchor.offers(mode).then_some(Self { anchor, mode })
        });

        parsed.unwrap_or_default()
    }

    pub fn key(&self) -> String {
        format!("{}-{}", self.mode.key(), self.anchor.key())
    }

    /// What the picker's chip reads, and the header of the review a batch of
    /// comments is sent to the agent as — so it has to be a phrase a person
    /// recognises, not a key.
    pub fn label(&self) -> String {
        match (&self.anchor, self.mode) {
            (Anchor::Base, _) => "All changes".into(),
            (Anchor::Live, _) => "Uncommitted".into(),
            (Anchor::Turn(n), Mode::Just) => format!("Turn {n}"),
            (Anchor::Turn(n), Mode::Since) => format!("Since turn {n}"),
            (Anchor::Commit(sha), Mode::Just) => short(sha),
            (Anchor::Commit(sha), Mode::Since) => format!("Since {}", short(sha)),
        }
    }

    /// The pair of revisions to diff, or `None` when there is nothing yet.
    ///
    /// Pure: `head` is resolved by the caller, which is the only part of this
    /// that has to touch git.
    pub fn revisions(
        &self,
        settings: &Settings,
        card_id: i64,
        turns: &[Turn],
        commits: &[Commit],
        head: Option<&str>,
    ) -> Option<(String, String)> {
        let base = settings.base_ref(card_id);
        let head = head?;
        let at = |n: i64| {
            turns
                .iter()
                .find(|t| t.n == n)
                .map(|t| t.commit_sha.clone())
        };

        match (&self.anchor, self.mode) {
            (Anchor::Base, _) => Some((base, head.to_owned())),

            // The worktree against the last thing that was recorded of it.
            (Anchor::Live, _) => {
                let from = turns.last().map(|t| t.commit_sha.clone()).unwrap_or(base);
                Some((from, head.to_owned()))
            }

            (Anchor::Turn(n), Mode::Just) => {
                let to = at(*n)?;
                // Turn 1 has no predecessor, so it is measured from the base.
                Some((at(n - 1).unwrap_or(base), to))
            }
            (Anchor::Turn(n), Mode::Since) => Some((at(*n)?, head.to_owned())),

            (Anchor::Commit(sha), Mode::Just) => {
                let commit = commits.iter().find(|c| c.sha == *sha)?;
                // A root commit has nothing behind it but the empty tree.
                let from = commit.parent.clone().unwrap_or_else(|| EMPTY_TREE.into());
                Some((from, commit.sha.clone()))
            }
            (Anchor::Commit(sha), Mode::Since) => {
                let commit = commits.iter().find(|c| c.sha == *sha)?;
                Some((commit.sha.clone(), head.to_owned()))
            }
        }
    }

    /// Every anchor offered for a card, newest first.
    ///
    /// Turns and commits interleave by time rather than sitting in separate
    /// groups, because they describe the same history from two angles: a turn
    /// swallows whatever the agent committed during it, and reading them in one
    /// order is how that relationship shows.
    /// `settled` is the tree of the last thing recorded of the card — its
    /// newest turn, or its base when it has none. `head` is compared against it
    /// to decide whether the worktree holds anything not captured yet.
    pub fn menu(
        turns: &[Turn],
        commits: &[Commit],
        head: Option<&str>,
        settled: Option<&str>,
    ) -> Vec<Entry> {
        let mut out = Vec::new();

        // Only when the worktree holds something no turn has recorded;
        // otherwise this would just be another name for the top of the list.
        if head.is_some() && head != settled {
            out.push(Entry {
                anchor: Anchor::Live,
                label: "Uncommitted work".into(),
                kind: Anchor::Live.kind(),
            });
        }

        let mut points: Vec<(i64, Entry)> = turns
            .iter()
            .map(|turn| {
                (
                    turn.at,
                    Entry {
                        anchor: Anchor::Turn(turn.n),
                        label: format!("Turn {}", turn.n),
                        kind: "turn",
                    },
                )
            })
            .chain(commits.iter().map(|commit| {
                (
                    commit.at,
                    Entry {
                        anchor: Anchor::Commit(commit.sha.clone()),
                        label: format!("{} {}", short(&commit.sha), truncate(&commit.subject)),
                        kind: "commit",
                    },
                )
            }))
            .collect();

        // Newest first. Ties keep the order above, which puts a turn ahead of
        // the commit it captured.
        points.sort_by(|a, b| b.0.cmp(&a.0));
        out.extend(points.into_iter().map(|(_, entry)| entry));

        out.push(Entry {
            anchor: Anchor::Base,
            label: "Where this card started".into(),
            kind: "base",
        });
        out
    }
}

fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// Subjects go in a 230px column, and the menu cannot ellipsize them itself.
fn truncate(subject: &str) -> String {
    const MAX: usize = 42;

    match subject.char_indices().nth(MAX) {
        Some((cut, _)) => format!("{}…", &subject[..cut].trim_end()),
        None => subject.to_owned(),
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
            created_at: format!("2026-09-17 12:0{n}:00"),
            // Turn 1 at 20, turn 2 at 40 — so a commit at 30 falls between them.
            at: n * 20,
        }
    }

    fn commit(sha: &str, parent: Option<&str>, at: i64) -> Commit {
        Commit {
            sha: sha.into(),
            parent: parent.map(str::to_owned),
            subject: format!("work on {sha}"),
            at,
        }
    }

    fn scope(mode: Mode, anchor: Anchor) -> Scope {
        Scope { anchor, mode }
    }

    #[test]
    fn scopes_round_trip_through_their_key() {
        let scopes = [
            scope(Mode::Since, Anchor::Base),
            scope(Mode::Just, Anchor::Live),
            scope(Mode::Just, Anchor::Turn(3)),
            scope(Mode::Since, Anchor::Turn(2)),
            scope(Mode::Just, Anchor::Commit("abc1234".into())),
            scope(Mode::Since, Anchor::Commit("abc1234".into())),
        ];

        for expected in scopes {
            assert_eq!(Scope::parse(Some(&expected.key())), expected);
        }
    }

    #[test]
    fn the_default_is_everything_the_card_has_done() {
        let default = Scope::default();
        assert_eq!(default.anchor, Anchor::Base);
        assert_eq!(default.mode, Mode::Since);
        assert_eq!(default.key(), "since-base");
        assert_eq!(default.label(), "All changes");
    }

    #[test]
    fn unparseable_scopes_fall_back_to_the_default() {
        for raw in [
            None,
            Some("nonsense"),
            Some("turn-1"),        // a bare anchor, with no mode
            Some("just-turn-x"),   // an unparseable turn
            Some("sideways-live"), // an unknown mode
            Some("just-nowhere"),
        ] {
            assert_eq!(Scope::parse(raw), Scope::default());
        }
    }

    #[test]
    fn a_commit_key_that_is_not_a_sha_is_refused() {
        // NB: a leading `-` would reach `git diff` as a flag, not a revision.
        for raw in [
            "just-commit---output=/tmp/x",
            "just-commit-zzzz",
            "just-commit-a",
        ] {
            assert_eq!(Scope::parse(Some(raw)), Scope::default());
        }
        assert_eq!(
            Scope::parse(Some("just-commit-abc1234")),
            scope(Mode::Just, Anchor::Commit("abc1234".into()))
        );
    }

    #[test]
    fn the_dead_half_of_each_end_is_not_offered() {
        assert!(!Anchor::Live.offers(Mode::Since));
        assert!(Anchor::Live.offers(Mode::Just));
        assert!(!Anchor::Base.offers(Mode::Just));
        assert!(Anchor::Base.offers(Mode::Since));

        for anchor in [Anchor::Turn(1), Anchor::Commit("abc1234".into())] {
            assert!(Mode::ALL.iter().all(|mode| anchor.offers(*mode)));
        }

        // And a key naming one of them is not honoured.
        assert_eq!(Scope::parse(Some("since-live")), Scope::default());
        assert_eq!(Scope::parse(Some("just-base")), Scope::default());
    }

    #[test]
    fn everything_live_ends_at_the_head_rather_than_the_last_turn() {
        let settings = settings();
        let turns = [turn(1, "sha1"), turn(2, "sha2")];
        let commits = [commit("c0ffee1", Some("sha-parent"), 20)];
        let head = Some("worktree-tree");

        let range = |scope: Scope| scope.revisions(&settings, 7, &turns, &commits, head);

        assert_eq!(
            range(scope(Mode::Since, Anchor::Base)),
            Some(("refs/ledecky/7/base".into(), "worktree-tree".into()))
        );
        assert_eq!(
            range(scope(Mode::Just, Anchor::Live)),
            Some(("sha2".into(), "worktree-tree".into()))
        );
        assert_eq!(
            range(scope(Mode::Since, Anchor::Turn(1))),
            Some(("sha1".into(), "worktree-tree".into()))
        );
        assert_eq!(
            range(scope(Mode::Since, Anchor::Commit("c0ffee1".into()))),
            Some(("c0ffee1".into(), "worktree-tree".into()))
        );
    }

    #[test]
    fn uncommitted_work_is_measured_from_the_base_when_no_turn_has_landed() {
        assert_eq!(
            scope(Mode::Just, Anchor::Live).revisions(&settings(), 7, &[], &[], Some("tree")),
            Some(("refs/ledecky/7/base".into(), "tree".into()))
        );
    }

    #[test]
    fn a_turn_on_its_own_spans_its_predecessor_to_itself() {
        let settings = settings();
        let turns = [turn(1, "sha1"), turn(2, "sha2")];
        let range =
            |n| scope(Mode::Just, Anchor::Turn(n)).revisions(&settings, 7, &turns, &[], Some("h"));

        // Turn 1 has no predecessor, so it is measured from the base.
        assert_eq!(
            range(1),
            Some(("refs/ledecky/7/base".into(), "sha1".into()))
        );
        assert_eq!(range(2), Some(("sha1".into(), "sha2".into())));
    }

    #[test]
    fn a_commit_on_its_own_spans_its_parent_to_itself() {
        let commits = [commit("c0ffee1", Some("dad1234"), 10)];
        assert_eq!(
            scope(Mode::Just, Anchor::Commit("c0ffee1".into())).revisions(
                &settings(),
                7,
                &[],
                &commits,
                Some("h")
            ),
            Some(("dad1234".into(), "c0ffee1".into()))
        );
    }

    #[test]
    fn a_root_commit_is_measured_against_the_empty_tree() {
        // NB: `<root>^` does not resolve, and would 500 the whole pane.
        let commits = [commit("c0ffee1", None, 10)];
        assert_eq!(
            scope(Mode::Just, Anchor::Commit("c0ffee1".into())).revisions(
                &settings(),
                7,
                &[],
                &commits,
                Some("h")
            ),
            Some((EMPTY_TREE.into(), "c0ffee1".into()))
        );
    }

    #[test]
    fn an_anchor_that_does_not_exist_has_no_revisions() {
        let settings = settings();
        let turns = [turn(1, "sha1")];
        let head = Some("h");

        assert_eq!(
            scope(Mode::Just, Anchor::Turn(9)).revisions(&settings, 7, &turns, &[], head),
            None
        );
        assert_eq!(
            scope(Mode::Since, Anchor::Turn(9)).revisions(&settings, 7, &turns, &[], head),
            None
        );
        assert_eq!(
            scope(Mode::Just, Anchor::Commit("abc1234".into())).revisions(
                &settings,
                7,
                &turns,
                &[],
                head
            ),
            None
        );
    }

    #[test]
    fn a_card_with_no_head_has_nothing_to_show() {
        assert_eq!(
            Scope::default().revisions(&settings(), 7, &[], &[], None),
            None
        );
    }

    #[test]
    fn the_menu_runs_newest_first_and_interleaves_turns_with_commits() {
        let turns = [turn(1, "sha1"), turn(2, "sha2")];
        let commits = [
            commit("bbbbbbb", Some("x"), 30),
            commit("aaaaaaa", Some("x"), 10),
        ];

        let menu = Scope::menu(&turns, &commits, Some("live-tree"), Some("turn-2-tree"));
        let keys: Vec<_> = menu.iter().map(|e| e.anchor.key()).collect();

        // Turns sit at 20 and 40, commits at 30 and 10 — so the commits are not
        // grouped after the turns, they fall between them.
        assert_eq!(
            keys,
            [
                "live",
                "turn-2",
                "commit-bbbbbbb",
                "turn-1",
                "commit-aaaaaaa",
                "base"
            ]
        );
        assert_eq!(
            menu.iter().map(|e| e.kind).collect::<Vec<_>>(),
            ["live", "turn", "commit", "turn", "commit", "base"]
        );
    }

    /// NB: both sides are trees. The head is a `write-tree` of the worktree, so
    /// comparing it against a turn's *commit* id would never match and the row
    /// would be offered forever.
    #[test]
    fn the_live_row_is_dropped_once_a_turn_has_captured_it() {
        let turns = [turn(1, "sha1")];

        let captured = Scope::menu(&turns, &[], Some("shared-tree"), Some("shared-tree"));
        assert!(captured.iter().all(|e| e.anchor != Anchor::Live));

        let dirty = Scope::menu(&turns, &[], Some("other-tree"), Some("shared-tree"));
        assert_eq!(dirty.first().map(|e| &e.anchor), Some(&Anchor::Live));
    }

    #[test]
    fn a_clean_worktree_with_no_turns_is_not_uncommitted_work() {
        // Nothing has happened yet: the worktree still matches the base.
        let menu = Scope::menu(&[], &[], Some("base-tree"), Some("base-tree"));
        assert_eq!(menu.len(), 1);
        assert_eq!(menu[0].anchor, Anchor::Base);
    }

    #[test]
    fn a_card_with_nothing_at_all_still_offers_where_it_started() {
        let menu = Scope::menu(&[], &[], None, None);
        assert_eq!(menu.len(), 1);
        assert_eq!(menu[0].anchor, Anchor::Base);
    }

    #[test]
    fn a_long_commit_subject_is_cut_rather_than_wrapped() {
        let commits = [Commit {
            sha: "abc1234def".into(),
            parent: None,
            subject: "review: stack every file in the diff and evict the agent's message".into(),
            at: 1,
        }];

        let menu = Scope::menu(&[], &commits, None, None);
        assert_eq!(
            menu[0].label,
            "abc1234 review: stack every file in the diff and e…"
        );
    }
}
