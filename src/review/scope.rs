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
    /// This point and everything after it, to the live head.
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

    /// Where a range starting at `point` stops.
    fn end(self, point: String, head: &str) -> String {
        match self {
            Self::Just => point,
            Self::Since => head.to_owned(),
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

    /// The turn this range is a snapshot of, if it is one.
    ///
    /// Only one turn read on its own is a point in the past a comment can be
    /// named by, which is what decides whether a comment already sent to the
    /// agent belongs on screen — the difference between reading a record back
    /// and reviewing work still in front of you.
    ///
    /// NB: `Just <commit>` is fixed too — `parent..commit` moves no more than a
    /// turn does — but a commit is not what a comment is pinned to, so it is
    /// read here as the card as it stands. A comment left on one is filed under
    /// the turn that swallowed it and comes back on that turn's range, which is
    /// a wider diff than the commit it was written on.
    ///
    /// NB: this identifies the turn, not the range a comment was written on.
    /// One left on "All changes" spans `base..turn n` and comes back on
    /// `turn n-1..turn n` — a different diff with the same post-image, so its
    /// new-side line still lands while an old-side one describes a pre-image
    /// that is no longer there. Naming the pair exactly would mean recording
    /// both revisions and giving the picker an as-of to reach them by.
    pub fn snapshot_turn(&self) -> Option<i64> {
        match (&self.anchor, self.mode) {
            (Anchor::Turn(n), Mode::Just) => Some(*n),
            _ => None,
        }
    }

    /// Whether a point recorded against `base_sha` belongs to the era the card is
    /// in now.
    ///
    /// Unknown either side reads as "yes": a turn snapshotted before the base was
    /// kept places no era, and refusing its ranges on that would hide history
    /// nothing is wrong with.
    fn current_era(base_sha: Option<&str>, base_at: Option<&str>) -> bool {
        match (base_sha, base_at) {
            (Some(taken), Some(now)) => taken == now,
            _ => true,
        }
    }

    /// The turn a range beginning before `point` is measured from, if it is a
    /// turn rather than the card's base.
    fn measured_from<'a>(&self, turns: &'a [Turn]) -> Option<&'a Turn> {
        match (&self.anchor, self.mode) {
            (Anchor::Live, _) => turns.last(),
            (Anchor::Turn(n), Mode::Since) => turns.iter().find(|t| t.n == n - 1),
            _ => None,
        }
    }

    /// Whether this range reaches across a rebase: from a turn taken against a
    /// base the card has since left, to a head that is past it.
    ///
    /// Only the ranges ending at the live head can do this. The rest have both
    /// ends recorded at once, so they stay internally consistent whatever the
    /// base does afterwards.
    pub fn spans_a_rebase(&self, turns: &[Turn], base_at: Option<&str>) -> bool {
        self.measured_from(turns)
            .is_some_and(|turn| !Self::current_era(turn.base_sha.as_deref(), base_at))
    }

    /// Whether a mode says anything about this anchor *for this card* — the
    /// structural rule of [`Anchor::offers`], and then whether the range it names
    /// would straddle a rebase.
    pub fn offers(&self, turns: &[Turn], base_at: Option<&str>) -> bool {
        self.anchor.offers(self.mode) && !self.spans_a_rebase(turns, base_at)
    }

    /// The range a reader asking for this one should land on instead.
    ///
    /// A straddling `Since` has an exact `Just` beside it, so the toggle flips.
    /// `Live` has no such partner — it is the straddle — so it falls back to the
    /// whole card, which is also the row [`Scope::menu`] stops offering.
    pub fn settle(self, turns: &[Turn], base_at: Option<&str>) -> Self {
        if self.offers(turns, base_at) {
            return self;
        }

        let flipped = Self {
            anchor: self.anchor.clone(),
            mode: self.mode.other(),
        };
        match flipped.offers(turns, base_at) {
            true => flipped,
            false => Self::default(),
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
        base_at: Option<&str>,
    ) -> Option<(String, String)> {
        let base = settings.base_ref(card_id);
        let head = head?;
        let at = |n: i64| {
            turns
                .iter()
                .find(|t| t.n == n)
                .map(|t| t.commit_sha.clone())
        };

        // NB: the ranges ending at the live head measure from the point before
        // them, and a rebase can leave that point in an era the head has moved
        // past — where the diff would read the upstream delta as the card's own
        // additions. Measuring from the base instead says less than was asked
        // for, which is the honest answer; `settle` and `menu` are what keep a
        // reader from asking.
        let straddles = self.spans_a_rebase(turns, base_at);

        match (&self.anchor, self.mode) {
            (Anchor::Base, _) => Some((base, head.to_owned())),

            // The worktree against the last thing that was recorded of it.
            (Anchor::Live, _) => {
                let from = match straddles {
                    true => None,
                    false => turns.last().map(|t| t.commit_sha.clone()),
                };
                Some((from.unwrap_or(base), head.to_owned()))
            }

            (Anchor::Turn(n), Mode::Just) => {
                let turn = turns.iter().find(|t| t.n == *n)?;
                // NB: the parent the snapshot was taken against, not wherever
                // the card's base has since got to. A turn and its own parent
                // are the same era, so the range survives a rebase moving the
                // base out from under it — whereas the base would pair a
                // post-rebase tree with a pre-rebase one and render every
                // upstream file as a deletion. Empty for a turn recorded
                // without one; an empty rev reaches `git diff` as a bare
                // argument.
                let from = Some(turn.parent_sha.clone())
                    .filter(|sha| !sha.is_empty())
                    .unwrap_or(base);
                Some((from, turn.commit_sha.clone()))
            }

            // Ends at the live head, so both ends have to be the era the
            // worktree is in now — the card's base, not the turn's.
            (Anchor::Turn(n), Mode::Since) => {
                at(*n)?;
                let from = match straddles {
                    true => None,
                    false => at(n - 1),
                };
                Some((from.unwrap_or(base), head.to_owned()))
            }

            (Anchor::Commit(sha), mode) => {
                let commit = commits.iter().find(|c| c.sha == *sha)?;
                // A root commit has nothing behind it but the empty tree.
                let from = commit.parent.clone().unwrap_or_else(|| EMPTY_TREE.into());
                Some((from, mode.end(commit.sha.clone(), head)))
            }
        }
    }

    /// The agent's commits whose changes this range covers, oldest first.
    ///
    /// `commits` is newest first, as `git::commits` lists them.
    ///
    /// NB: turns are placed by time, not ancestry. A turn ref is a parallel
    /// chain that never contains the agent's commits, so the only thing
    /// relating the two is when each happened — as in `menu`.
    pub fn commits_in<'a>(&self, turns: &[Turn], commits: &'a [Commit]) -> Vec<&'a Commit> {
        let at = |n: i64| turns.iter().find(|t| t.n == n).map(|t| t.at);
        let after = |from: Option<i64>| {
            commits
                .iter()
                .filter(move |c| from.is_none_or(|from| c.at > from))
        };

        let mut out: Vec<&Commit> = match (&self.anchor, self.mode) {
            (Anchor::Base, _) => commits.iter().collect(),
            (Anchor::Live, _) => after(turns.last().map(|t| t.at)).collect(),
            (Anchor::Turn(n), Mode::Just) => match at(*n) {
                Some(to) => after(at(n - 1)).filter(|c| c.at <= to).collect(),
                None => Vec::new(),
            },
            (Anchor::Turn(n), Mode::Since) => match at(*n) {
                Some(from) => after(Some(from)).collect(),
                None => Vec::new(),
            },
            (Anchor::Commit(sha), Mode::Just) => commits.iter().filter(|c| c.sha == *sha).collect(),
            (Anchor::Commit(sha), Mode::Since) => {
                commits.iter().take_while(|c| c.sha != *sha).collect()
            }
        };
        out.reverse();
        out
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
    /// `base_at` is the card's base as it stands, which says whether its newest
    /// turn is still in the era the worktree is.
    pub fn menu(
        turns: &[Turn],
        commits: &[Commit],
        head: Option<&str>,
        settled: Option<&str>,
        base_at: Option<&str>,
    ) -> Vec<Entry> {
        let mut out = Vec::new();

        // Only when the worktree holds something no turn has recorded, and only
        // while there is a turn in this era to measure it from: across a rebase
        // the range falls back to the base, and the row would just be another
        // name for the bottom of the list. The next snapshot brings it back.
        let live = Scope {
            anchor: Anchor::Live,
            mode: Mode::Just,
        };
        if head.is_some() && head != settled && !live.spans_a_rebase(turns, base_at) {
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
        points.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
        out.extend(points.into_iter().map(|(_, entry)| entry));

        out.push(Entry {
            anchor: Anchor::Base,
            label: "What this card is based on".into(),
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
        Some((cut, _)) => format!("{}…", subject[..cut].trim_end()),
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

    /// Where the cards in these fixtures were cut from, as a resolved sha —
    /// which is what `turns.parent_sha` holds, not a ref name.
    const BASE_AT_TURN_1: &str = "base0ff";

    /// A base a card has since moved off, as a rebase leaves behind.
    const OLD_BASE: &str = "oldbase";

    /// One of a chain, parented the way `Turn::snapshot` records it: on the
    /// previous turn, or on the base the card was cut from for turn 1.
    fn turn(n: i64, sha: &str) -> Turn {
        let parent = match n {
            1 => BASE_AT_TURN_1.to_owned(),
            n => format!("sha{}", n - 1),
        };
        rooted(n, sha, &parent, Some(BASE_AT_TURN_1))
    }

    /// One of a chain taken before the card's base moved: same parentage, but an
    /// era the card has left.
    fn stale(n: i64, sha: &str) -> Turn {
        let parent = match n {
            1 => OLD_BASE.to_owned(),
            n => format!("sha{}", n - 1),
        };
        rooted(n, sha, &parent, Some(OLD_BASE))
    }

    /// The same, with the parent and the era named.
    fn rooted(n: i64, sha: &str, parent: &str, base: Option<&str>) -> Turn {
        Turn {
            id: n,
            n,
            commit_sha: sha.into(),
            parent_sha: parent.into(),
            base_sha: base.map(str::to_owned),
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
            message: format!("work on {sha}"),
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
    fn only_one_turn_on_its_own_is_a_fixed_snapshot() {
        assert_eq!(scope(Mode::Just, Anchor::Turn(3)).snapshot_turn(), Some(3));

        // Everything else names no turn, so it is read as the card as it stands.
        for other in [
            scope(Mode::Since, Anchor::Turn(3)),
            scope(Mode::Since, Anchor::Base),
            scope(Mode::Just, Anchor::Live),
            scope(Mode::Just, Anchor::Commit("abc1234".into())),
            scope(Mode::Since, Anchor::Commit("abc1234".into())),
        ] {
            assert_eq!(other.snapshot_turn(), None, "{}", other.key());
        }
    }

    #[test]
    fn everything_live_ends_at_the_head_rather_than_the_last_turn() {
        let settings = settings();
        let turns = [turn(1, "sha1"), turn(2, "sha2")];
        let commits = [commit("c0ffee1", Some("sha-parent"), 20)];
        let head = Some("worktree-tree");

        let range = |scope: Scope| {
            scope.revisions(&settings, 7, &turns, &commits, head, Some(BASE_AT_TURN_1))
        };

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
            Some(("refs/ledecky/7/base".into(), "worktree-tree".into()))
        );
        assert_eq!(
            range(scope(Mode::Since, Anchor::Commit("c0ffee1".into()))),
            Some(("sha-parent".into(), "worktree-tree".into()))
        );
    }

    #[test]
    fn since_a_point_includes_that_point() {
        let settings = settings();
        let turns = [turn(1, "sha1"), turn(2, "sha2")];
        let commits = [commit("c0ffee1", None, 10)];
        let range = |anchor| {
            scope(Mode::Since, anchor).revisions(
                &settings,
                7,
                &turns,
                &commits,
                Some("h"),
                Some(BASE_AT_TURN_1),
            )
        };

        assert_eq!(range(Anchor::Turn(2)), Some(("sha1".into(), "h".into())));
        assert_eq!(
            range(Anchor::Commit("c0ffee1".into())),
            Some((EMPTY_TREE.into(), "h".into()))
        );
    }

    #[test]
    fn uncommitted_work_is_measured_from_the_base_when_no_turn_has_landed() {
        assert_eq!(
            scope(Mode::Just, Anchor::Live).revisions(
                &settings(),
                7,
                &[],
                &[],
                Some("tree"),
                Some(BASE_AT_TURN_1)
            ),
            Some(("refs/ledecky/7/base".into(), "tree".into()))
        );
    }

    #[test]
    fn a_turn_on_its_own_spans_its_predecessor_to_itself() {
        let settings = settings();
        let turns = [turn(1, "sha1"), turn(2, "sha2")];
        let range = |n| {
            scope(Mode::Just, Anchor::Turn(n)).revisions(
                &settings,
                7,
                &turns,
                &[],
                Some("h"),
                Some(BASE_AT_TURN_1),
            )
        };

        // Turn 1 has no predecessor but the base it was cut from, which it
        // recorded at the time rather than reading back now.
        assert_eq!(range(1), Some((BASE_AT_TURN_1.into(), "sha1".into())));
        assert_eq!(range(2), Some(("sha1".into(), "sha2".into())));
    }

    /// The rebase case: the card's base has moved on, and the snapshot has not.
    /// Reaching for the card's base here would pair a post-rebase tree with a
    /// pre-rebase one and render every upstream file as a deletion.
    #[test]
    fn a_turn_is_measured_from_the_base_it_was_taken_against() {
        let turns = [rooted(1, "sha1", OLD_BASE, Some(OLD_BASE))];

        assert_eq!(
            scope(Mode::Just, Anchor::Turn(1)).revisions(
                &settings(),
                7,
                &turns,
                &[],
                Some("h"),
                Some(BASE_AT_TURN_1)
            ),
            Some((OLD_BASE.into(), "sha1".into()))
        );
    }

    /// `Since` ends at the live head, so both of its ends have to be the era the
    /// worktree is in now — the card's base, not the turn's.
    #[test]
    fn since_a_turn_keeps_measuring_from_the_cards_own_base() {
        let turns = [rooted(1, "sha1", OLD_BASE, Some(OLD_BASE))];

        assert_eq!(
            scope(Mode::Since, Anchor::Turn(1)).revisions(
                &settings(),
                7,
                &turns,
                &[],
                Some("h"),
                Some(BASE_AT_TURN_1)
            ),
            Some(("refs/ledecky/7/base".into(), "h".into()))
        );
    }

    /// An empty parent would reach `git diff` as a bare argument.
    #[test]
    fn a_turn_with_no_recorded_parent_falls_back_to_the_card_base() {
        let turns = [rooted(1, "sha1", "", None)];

        assert_eq!(
            scope(Mode::Just, Anchor::Turn(1)).revisions(
                &settings(),
                7,
                &turns,
                &[],
                Some("h"),
                Some(BASE_AT_TURN_1)
            ),
            Some(("refs/ledecky/7/base".into(), "sha1".into()))
        );
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
                Some("h"),
                Some(BASE_AT_TURN_1)
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
                Some("h"),
                Some(BASE_AT_TURN_1)
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
            scope(Mode::Just, Anchor::Turn(9)).revisions(
                &settings,
                7,
                &turns,
                &[],
                head,
                Some(BASE_AT_TURN_1)
            ),
            None
        );
        assert_eq!(
            scope(Mode::Since, Anchor::Turn(9)).revisions(
                &settings,
                7,
                &turns,
                &[],
                head,
                Some(BASE_AT_TURN_1)
            ),
            None
        );
        assert_eq!(
            scope(Mode::Just, Anchor::Commit("abc1234".into())).revisions(
                &settings,
                7,
                &turns,
                &[],
                head,
                Some(BASE_AT_TURN_1)
            ),
            None
        );
    }

    /// A rebase leaves every turn on the chain in the old era, because each one
    /// parents on its predecessor rather than on the base.
    #[test]
    fn uncommitted_work_falls_back_to_the_base_across_a_rebase() {
        let turns = [stale(1, "sha1"), stale(2, "sha2")];

        // In the era it was taken in, this reads from the newest turn.
        assert_eq!(
            scope(Mode::Just, Anchor::Live).revisions(
                &settings(),
                7,
                &turns,
                &[],
                Some("h"),
                Some(OLD_BASE)
            ),
            Some(("sha2".into(), "h".into()))
        );

        // Once the base has moved past it, measuring from it would read the
        // upstream delta as the card's own additions.
        assert_eq!(
            scope(Mode::Just, Anchor::Live).revisions(
                &settings(),
                7,
                &turns,
                &[],
                Some("h"),
                Some(BASE_AT_TURN_1)
            ),
            Some(("refs/ledecky/7/base".into(), "h".into()))
        );
    }

    /// `Since turn 1` is `base..head` whatever happens, so only the turns with a
    /// predecessor can straddle.
    #[test]
    fn since_a_turn_straddles_only_once_it_has_a_predecessor_to_miss() {
        let turns = [stale(1, "sha1"), stale(2, "sha2")];
        let spans =
            |n| scope(Mode::Since, Anchor::Turn(n)).spans_a_rebase(&turns, Some(BASE_AT_TURN_1));

        assert!(!spans(1));
        assert!(spans(2));

        // And nothing with both ends recorded at once ever does.
        for mode in Mode::ALL {
            assert!(!scope(*mode, Anchor::Base).spans_a_rebase(&turns, Some(BASE_AT_TURN_1)));
        }
        assert!(!scope(Mode::Just, Anchor::Turn(2)).spans_a_rebase(&turns, Some(BASE_AT_TURN_1)));
    }

    #[test]
    fn a_straddling_range_settles_onto_one_that_is_exact() {
        let turns = [stale(1, "sha1"), stale(2, "sha2")];
        let era = Some(BASE_AT_TURN_1);

        // The toggle has an exact partner, so it flips rather than falling back.
        assert_eq!(
            scope(Mode::Since, Anchor::Turn(2)).settle(&turns, era),
            scope(Mode::Just, Anchor::Turn(2))
        );

        // `Live` is the straddle itself; there is nothing beside it.
        assert_eq!(
            scope(Mode::Just, Anchor::Live).settle(&turns, era),
            Scope::default()
        );

        // And a range that was already exact is left alone.
        let fine = scope(Mode::Just, Anchor::Turn(1));
        assert_eq!(fine.clone().settle(&turns, era), fine);
    }

    #[test]
    fn the_live_row_goes_while_the_turn_behind_it_is_from_another_era() {
        let turns = [stale(1, "sha1")];
        let rows = |era| {
            Scope::menu(&turns, &[], Some("dirty-tree"), Some("clean-tree"), era)
                .first()
                .map(|e| e.anchor.clone())
        };

        assert_eq!(rows(Some(OLD_BASE)), Some(Anchor::Live));
        assert_eq!(rows(Some(BASE_AT_TURN_1)), Some(Anchor::Turn(1)));
    }

    /// Turns snapshotted before the base was recorded place no era, and refusing
    /// their ranges on that would hide history nothing is wrong with.
    #[test]
    fn a_turn_that_recorded_no_era_is_not_refused() {
        let turns = [rooted(1, "sha1", "base0ff", None)];
        let scoped = scope(Mode::Since, Anchor::Turn(1));

        assert!(!scoped.spans_a_rebase(&turns, Some("anything-at-all")));
        assert!(scoped.offers(&turns, Some("anything-at-all")));

        // The same when it is the card's base that cannot be resolved.
        assert!(!scope(Mode::Just, Anchor::Live).spans_a_rebase(&[stale(1, "sha1")], None));
    }

    #[test]
    fn a_card_with_no_head_has_nothing_to_show() {
        assert_eq!(
            Scope::default().revisions(&settings(), 7, &[], &[], None, Some(BASE_AT_TURN_1)),
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

        let menu = Scope::menu(
            &turns,
            &commits,
            Some("live-tree"),
            Some("turn-2-tree"),
            Some(BASE_AT_TURN_1),
        );
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

        let captured = Scope::menu(
            &turns,
            &[],
            Some("shared-tree"),
            Some("shared-tree"),
            Some(BASE_AT_TURN_1),
        );
        assert!(captured.iter().all(|e| e.anchor != Anchor::Live));

        let dirty = Scope::menu(
            &turns,
            &[],
            Some("other-tree"),
            Some("shared-tree"),
            Some(BASE_AT_TURN_1),
        );
        assert_eq!(dirty.first().map(|e| &e.anchor), Some(&Anchor::Live));
    }

    #[test]
    fn a_clean_worktree_with_no_turns_is_not_uncommitted_work() {
        // Nothing has happened yet: the worktree still matches the base.
        let menu = Scope::menu(
            &[],
            &[],
            Some("base-tree"),
            Some("base-tree"),
            Some(BASE_AT_TURN_1),
        );
        assert_eq!(menu.len(), 1);
        assert_eq!(menu[0].anchor, Anchor::Base);
    }

    #[test]
    fn a_card_with_nothing_at_all_still_offers_where_it_started() {
        let menu = Scope::menu(&[], &[], None, None, Some(BASE_AT_TURN_1));
        assert_eq!(menu.len(), 1);
        assert_eq!(menu[0].anchor, Anchor::Base);
    }

    #[test]
    fn a_long_commit_subject_is_cut_rather_than_wrapped() {
        let commits = [Commit {
            sha: "abc1234def".into(),
            parent: None,
            subject: "review: stack every file in the diff and evict the agent's message".into(),
            message: String::new(),
            at: 1,
        }];

        let menu = Scope::menu(&[], &commits, None, None, Some(BASE_AT_TURN_1));
        assert_eq!(
            menu[0].label,
            "abc1234 review: stack every file in the diff and e…"
        );
    }

    #[test]
    fn each_range_lists_the_commits_it_covers_oldest_first() {
        // Turns at 20 and 40; commits either side of each, newest first.
        let turns = [turn(1, "sha1"), turn(2, "sha2")];
        let commits = [
            commit("ddddddd", Some("x"), 50),
            commit("ccccccc", Some("x"), 40),
            commit("bbbbbbb", Some("x"), 30),
            commit("aaaaaaa", Some("x"), 10),
        ];
        let shas = |scope: Scope| {
            scope
                .commits_in(&turns, &commits)
                .into_iter()
                .map(|c| c.sha.as_str())
                .collect::<Vec<_>>()
        };

        assert_eq!(
            shas(scope(Mode::Since, Anchor::Base)),
            ["aaaaaaa", "bbbbbbb", "ccccccc", "ddddddd"]
        );
        assert_eq!(shas(scope(Mode::Just, Anchor::Live)), ["ddddddd"]);
        assert_eq!(shas(scope(Mode::Just, Anchor::Turn(1))), ["aaaaaaa"]);
        // A commit at the same second as the turn was captured by it.
        assert_eq!(
            shas(scope(Mode::Just, Anchor::Turn(2))),
            ["bbbbbbb", "ccccccc"]
        );
        assert_eq!(
            shas(scope(Mode::Since, Anchor::Turn(1))),
            ["bbbbbbb", "ccccccc", "ddddddd"]
        );
        assert_eq!(
            shas(scope(Mode::Just, Anchor::Commit("bbbbbbb".into()))),
            ["bbbbbbb"]
        );
        // `bbbbbbb..head` leaves the anchor itself out.
        assert_eq!(
            shas(scope(Mode::Since, Anchor::Commit("bbbbbbb".into()))),
            ["ccccccc", "ddddddd"]
        );
        assert!(shas(scope(Mode::Just, Anchor::Turn(9))).is_empty());
    }

    #[test]
    fn with_no_turns_every_commit_is_live() {
        let commits = [commit("aaaaaaa", None, 10)];
        assert_eq!(
            scope(Mode::Just, Anchor::Live)
                .commits_in(&[], &commits)
                .len(),
            1
        );
    }
}
