use std::path::{Path, PathBuf};
use std::time::Duration;

use rocket::serde::Serialize;
use sqlx::sqlite::SqliteRow;
use sqlx::{FromRow, Row};

use crate::config::Settings;
use crate::db::{sql, Db};
use crate::git;
use crate::project::{Card, Project};
use crate::review::{Comment, DiffCache};

/// A snapshot of the worktree at the end of one agent turn.
///
/// The agent may or may not commit its own work, so the server commits the
/// working tree itself; these refs are what make the diff scopes possible.
#[derive(Debug, Clone, Serialize)]
#[serde(crate = "rocket::serde")]
pub struct Turn {
    pub id: i64,
    pub n: i64,
    pub commit_sha: String,
    pub parent_sha: String,
    pub last_assistant_message: Option<String>,
    pub created_at: String,
    /// `created_at` in unix seconds, which is what orders a turn against one of
    /// the agent's own commits in the picker.
    pub at: i64,
}

impl Turn {
    const COLUMNS: &'static str = "id, n, commit_sha, parent_sha, last_assistant_message, \
         created_at, CAST(strftime('%s', created_at) AS INTEGER) AS at";

    pub async fn for_card(db: &Db, card_id: i64) -> Vec<Self> {
        sqlx::query_as(sql(format!(
            "SELECT {} FROM turns WHERE card_id = ?1 ORDER BY n",
            Self::COLUMNS
        )))
        .bind(card_id)
        .fetch_all(db.pool())
        .await
        .unwrap_or_default()
    }

    pub async fn latest(db: &Db, card_id: i64) -> Option<Self> {
        sqlx::query_as(sql(format!(
            "SELECT {} FROM turns WHERE card_id = ?1 ORDER BY n DESC LIMIT 1",
            Self::COLUMNS
        )))
        .bind(card_id)
        .fetch_optional(db.pool())
        .await
        .ok()
        .flatten()
    }

    async fn next_number(db: &Db, card_id: i64) -> i64 {
        sqlx::query_scalar("SELECT COALESCE(MAX(n), 0) + 1 FROM turns WHERE card_id = ?1")
            .bind(card_id)
            .fetch_one(db.pool())
            .await
            .unwrap_or(1)
    }

    /// NB: the error is returned rather than dropped. `turns` is
    /// `UNIQUE (card_id, n)`, and the number was read before the git below
    /// ran — so two snapshots racing on one card now collide here, where under
    /// one connection held for the whole sequence they could not. Losing that
    /// quietly would lose a turn; losing it loudly is `on_stop` logging it.
    async fn record(
        db: &Db,
        settings: &Settings,
        card_id: i64,
        n: i64,
        commit_sha: &str,
        parent_sha: &str,
        message: &str,
    ) -> sqlx::Result<()> {
        let inserted = sqlx::query(
            "INSERT INTO turns
                 (card_id, n, ref_name, commit_sha, parent_sha, last_assistant_message)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(card_id)
        .bind(n)
        .bind(settings.turn_ref(card_id, n))
        .bind(commit_sha)
        .bind(parent_sha)
        .bind(message)
        .execute(db.pool())
        .await?;

        // NB: a review sent while this turn was still running had no turn to be
        // pinned to; this one is the record of what it was written against.
        Comment::adopt_orphans(db, card_id, inserted.last_insert_rowid()).await;

        Ok(())
    }

    /// Snapshots `worktree` as the card's next turn.
    ///
    /// Returns `None` when nothing changed on disk, which is how a chat-only
    /// turn avoids piling up an empty ref.
    pub async fn snapshot(
        db: &Db,
        settings: &Settings,
        card_id: i64,
        repo: &Path,
        worktree: &Path,
        message: &str,
    ) -> anyhow::Result<Option<Self>> {
        let n = Self::next_number(db, card_id).await;
        let parent = match Self::latest(db, card_id).await {
            Some(turn) => turn.commit_sha,
            None => settings.base_ref(card_id),
        };
        let parent_sha = git::run(repo, &["rev-parse", &parent]).await?;

        let Some(sha) =
            git::snapshot_turn(settings, repo, worktree, card_id, n, &parent_sha).await?
        else {
            return Ok(None);
        };

        Self::record(db, settings, card_id, n, &sha, &parent_sha, message).await?;
        Ok(Self::latest(db, card_id).await)
    }
}

impl<'r> FromRow<'r, SqliteRow> for Turn {
    fn from_row(row: &'r SqliteRow) -> sqlx::Result<Self> {
        Ok(Self {
            id: row.try_get("id")?,
            n: row.try_get("n")?,
            commit_sha: row.try_get("commit_sha")?,
            parent_sha: row.try_get("parent_sha")?,
            last_assistant_message: row.try_get("last_assistant_message")?,
            created_at: row.try_get("created_at")?,
            at: row.try_get("at")?,
        })
    }
}

/// Whether a caller is willing to stage the worktree to get an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Whatever is already known, and the last turn if nothing is. Never
    /// shells out.
    ///
    /// What the board reads. It draws one chip per card, so computing here
    /// costs an `add -A` per card per render — on every navigation, and on
    /// every event that moves any of them. The watcher produces the head when
    /// the worktree moves and announces afterwards, so what is known here is
    /// current except in the gap between a write and that announcement, which
    /// the announcement closes.
    Cached,
    /// Stage it if what is known has expired.
    ///
    /// What the review pane reads. One card, and the reader is looking
    /// straight at it.
    Fresh,
}

/// The revision the live scopes end at: the worktree exactly as it stands.
///
/// This is what makes work visible before the turn that would have captured it.
/// Falls back to the last turn once the worktree is gone, so a merged card
/// still shows its history, and returns `None` only when the card has neither.
pub async fn live_head(
    cache: &DiffCache,
    settings: &Settings,
    repo: &Path,
    worktree: Option<&Path>,
    card: &Card,
    turns: &[Turn],
    freshness: Freshness,
) -> Option<String> {
    let settled = || turns.last().map(|turn| turn.commit_sha.clone());

    if freshness == Freshness::Cached {
        // NB: not even the `.git` stat below. This runs once per card per
        // render and the answer it would change is the fallback, which the
        // watcher has already made right by forgetting the head of a worktree
        // that has gone.
        return cache.known_head(card.id).or_else(settled);
    }

    // NB: `.git` rather than the directory, matching how a session decides a
    // worktree is real. `teardown` removes it from the hook thread, so this can
    // lose the race and find a half-removed one either way.
    let Some(live) = worktree.filter(|path| path.join(".git").exists()) else {
        return settled();
    };

    cache
        .head(
            card.id,
            Duration::from_millis(settings.head_ttl),
            || async {
                stage(cache, settings, repo, live, card)
                    .await
                    .or_else(settled)
            },
        )
        .await
        .or_else(settled)
}

/// Stages a card's worktree from somewhere that is not a request, replacing
/// what is memoised and priming the stat the board reads from it.
///
/// This is the whole of the change of direction: the watcher knows the worktree
/// moved, so it produces the new head and announces afterwards. Announcing
/// first and leaving the memo empty — which is what forgetting it amounted to —
/// told every fragment on every open board to go and stage the same card at the
/// same moment, and it is a reader saying "the diff moved" that the reader was
/// then made to wait for.
///
/// Returns what it staged, or nothing if the card has no worktree left.
pub async fn restage(
    db: &Db,
    cache: &DiffCache,
    settings: &Settings,
    card_id: i64,
) -> Option<String> {
    let card = Card::find(db, card_id).await?;
    let project = Project::find(db, card.project_id).await?;

    // NB: checked here rather than left to `working_tree` to fail, and checked
    // again by `forget_head` removing the slot: a teardown can land while this
    // is staging, and a head written for a worktree that has gone outlives the
    // refs it names.
    let worktree = card.worktree_path.as_ref().map(PathBuf::from)?;
    if !worktree.join(".git").exists() {
        cache.forget_head(card_id);
        return None;
    }

    let repo = project.repo();
    let head = cache
        .refresh_head(card_id, || async {
            stage(cache, settings, &repo, &worktree, &card).await
        })
        .await?;

    // The board asks for this stat and no longer computes anything to get it,
    // so the thread that made the head it is measured to makes it too.
    cache.stat(&repo, &settings.base_ref(card_id), &head).await;

    Some(head)
}

/// Stages a card's worktree and says what tree that made.
///
/// The one place the staging happens, so the watcher and a reader that found
/// nothing memoised do the same thing in the same order.
async fn stage(
    cache: &DiffCache,
    settings: &Settings,
    repo: &Path,
    worktree: &Path,
    card: &Card,
) -> Option<String> {
    // Before anything measures from it: the pane reads `base..HEAD` for its
    // commit list and the board takes a stat from the same ref, both after
    // this returns.
    reconcile(cache, settings, repo, worktree, card).await;

    match git::working_tree(settings, repo, worktree, card.id).await {
        Ok(tree) => Some(tree),
        Err(err) => {
            warn!("card {}: staging the worktree: {err:#}", card.id);
            None
        }
    }
}

/// Keeps the card's base ref pointing at whatever its worktree branches from.
///
/// Rides inside [`DiffCache::head`]'s memo, so it costs at most one `merge-base`
/// per card per head TTL rather than one per request.
async fn reconcile(
    cache: &DiffCache,
    settings: &Settings,
    repo: &Path,
    worktree: &Path,
    card: &Card,
) {
    // NB: not while a merge is outstanding. The agent has been asked to land its
    // commits on the base branch, and once that ff-merge goes in the merge base
    // *is* the card's own head — a poll arriving before `check_merge` tears the
    // card down would advance the base to the tip and blank the diff for good.
    // A merge exists to put this work on the base branch; measuring from the
    // base branch afterwards would swallow the very thing under review.
    if card.merge_requested {
        return;
    }

    let base_ref = settings.base_ref(card.id);
    let Some(moved) = git::reconcile_base(repo, worktree, &base_ref, &card.base_branch).await
    else {
        return;
    };

    info!("card {}: base moved to {moved}", card.id);
    cache.forget_stats(repo, &base_ref);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tests::memory_db;
    use crate::project::{Card, NewCard, Project};

    async fn card(db: &Db) -> i64 {
        let project = Project::upsert(db, Path::new("/srv/repo")).await.unwrap();
        Card::create(
            db,
            NewCard {
                project_id: project,
                task: "work",
                base_branch: "main",
                permission_mode: "acceptEdits",
                model: None,
            },
        )
        .await
        .unwrap()
    }

    fn settings() -> Settings {
        use rocket::figment::providers::Serialized;
        Settings::from(
            &rocket::figment::Figment::new()
                .merge(Serialized::default("app_slug", "ledecky"))
                .merge(Serialized::default("data_dir", "/tmp/ledecky-turn-test")),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn turns_number_from_one_and_climb() {
        let db = memory_db().await;
        let card_id = card(&db).await;
        let settings = settings();

        assert_eq!(Turn::next_number(&db, card_id).await, 1);
        Turn::record(&db, &settings, card_id, 1, "sha1", "base", "first")
            .await
            .unwrap();
        assert_eq!(Turn::next_number(&db, card_id).await, 2);
        Turn::record(&db, &settings, card_id, 2, "sha2", "sha1", "second")
            .await
            .unwrap();
        assert_eq!(Turn::next_number(&db, card_id).await, 3);
    }

    #[tokio::test]
    async fn the_ref_name_follows_the_configured_slug() {
        let db = memory_db().await;
        let card_id = card(&db).await;

        use rocket::figment::providers::Serialized;
        let settings = Settings::from(
            &rocket::figment::Figment::new()
                .merge(Serialized::default("app_slug", "planner"))
                .merge(Serialized::default("data_dir", "/tmp/x")),
        )
        .unwrap();

        Turn::record(&db, &settings, card_id, 1, "sha1", "base", "")
            .await
            .unwrap();

        let ref_name: String = sqlx::query_scalar("SELECT ref_name FROM turns WHERE card_id = ?1")
            .bind(card_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(ref_name, format!("refs/planner/{card_id}/turn-1"));
    }

    #[tokio::test]
    async fn latest_is_the_highest_numbered_turn() {
        let db = memory_db().await;
        let card_id = card(&db).await;
        let settings = settings();

        assert!(Turn::latest(&db, card_id).await.is_none());

        Turn::record(&db, &settings, card_id, 1, "sha1", "base", "first")
            .await
            .unwrap();
        Turn::record(&db, &settings, card_id, 2, "sha2", "sha1", "second")
            .await
            .unwrap();

        let latest = Turn::latest(&db, card_id).await.unwrap();
        assert_eq!(latest.n, 2);
        assert_eq!(latest.commit_sha, "sha2");
        assert_eq!(latest.last_assistant_message.as_deref(), Some("second"));

        assert_eq!(Turn::for_card(&db, card_id).await.len(), 2);
    }

    #[tokio::test]
    async fn turns_are_scoped_to_their_card() {
        let db = memory_db().await;
        let settings = settings();

        let a = card(&db).await;
        let b = Card::create(
            &db,
            NewCard {
                project_id: Project::upsert(&db, Path::new("/srv/repo")).await.unwrap(),
                task: "other",
                base_branch: "main",
                permission_mode: "acceptEdits",
                model: None,
            },
        )
        .await
        .unwrap();

        Turn::record(&db, &settings, a, 1, "sha-a", "base", "")
            .await
            .unwrap();

        assert_eq!(Turn::for_card(&db, a).await.len(), 1);
        assert!(Turn::for_card(&db, b).await.is_empty());
        assert_eq!(Turn::next_number(&db, b).await, 1);
    }
}
