use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use rocket::serde::Serialize;

use crate::git;
use crate::review::diff::{self, ParsedFile};

/// How many parses to keep before dropping the oldest.
const CAPACITY: usize = 64;

/// How many stats to keep, which is a different question.
///
/// NB: eviction here is by age, not by use, and every head a card's worktree
/// passes through leaves one behind — so a board being worked on churns through
/// these far faster than it does parses. Sized so a card's current stat cannot
/// be evicted by other cards' history, which would put a `diff --numstat` per
/// card back on the board render this cache exists to keep off git.
const STAT_CAPACITY: usize = 1024;

/// Memoises parsed diffs so selecting a file, opening a hunk and every comment
/// action do not re-run git and delta over a whole file.
///
/// Keyed on resolved commit shas rather than ref names: turn refs point at
/// immutable commits, so an entry can never go stale and there is no
/// invalidation to get wrong. Entries are dropped when a card is torn down, and
/// otherwise when the cache is full.
/// Cloned into the worktree watcher, which invalidates a card's head from a
/// thread of its own. Otherwise shared exactly as Rocket managed state.
#[derive(Default, Clone)]
pub struct DiffCache(Arc<Store>);

#[derive(Default)]
struct Store {
    entries: Mutex<Vec<(Key, Arc<Vec<ParsedFile>>)>>,
    stats: Mutex<Vec<(Key, Stat)>>,
    /// A lock per card rather than one over all of them — see [`DiffCache::head`].
    heads: Mutex<HashMap<i64, SlotRef>>,
}

type SlotRef = Arc<Slot>;

/// One card's memoised head, and the right to go and produce it.
///
/// NB: two locks, deliberately. `staging` is held across the git that produces
/// a head — seven subprocesses, all of them awaited — so it has to be a lock
/// that can be held across an await. `head` is the value, touched for as long
/// as a clone takes and never across one; keeping it a plain mutex is what lets
/// [`DiffCache::known_head`] and [`DiffCache::forget_head`] stay synchronous,
/// and they are called once per card per board render and from the watcher
/// respectively.
#[derive(Default)]
struct Slot {
    staging: tokio::sync::Mutex<()>,
    head: Mutex<Option<(Instant, String)>>,
}

/// How much a range changed, for the cards on the board.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(crate = "rocket::serde")]
pub struct Stat {
    pub additions: u32,
    pub deletions: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Key {
    repo: PathBuf,
    from: String,
    to: String,
}

impl Slot {
    fn read(&self) -> Option<(Instant, String)> {
        self.head.lock().ok()?.clone()
    }

    fn write(&self, head: Option<String>) {
        if let Ok(mut cell) = self.head.lock() {
            *cell = head.map(|head| (Instant::now(), head));
        }
    }
}

impl DiffCache {
    /// The parsed diff between two revisions, computing it on a miss.
    pub async fn get(&self, repo: &Path, from: &str, to: &str) -> Result<Arc<Vec<ParsedFile>>> {
        // Resolving first is what makes the key stable: `refs/.../turn-2` and the
        // sha it points at are the same entry.
        let key = Key {
            repo: repo.to_path_buf(),
            from: git::run(repo, &["rev-parse", from]).await?,
            to: git::run(repo, &["rev-parse", to]).await?,
        };

        if let Some(hit) = self.lookup(&key) {
            return Ok(hit);
        }

        let parsed = Arc::new(diff::between(repo, &key.from, &key.to).await?);
        self.insert(key, parsed.clone());
        Ok(parsed)
    }

    /// Line counts for a range, computing them on a miss.
    ///
    /// The board asks for one of these per card on every poll, so it must not
    /// touch delta. Unlike [`DiffCache::get`] the key is taken as given, ref
    /// name and all — resolving here would cost a `rev-parse` per card per
    /// render, including on a hit. The one ref that moves under it is a card's
    /// base, and [`DiffCache::forget_stats`] is how that says so.
    pub async fn stat(&self, repo: &Path, from: &str, to: &str) -> Stat {
        let key = Key {
            repo: repo.to_path_buf(),
            from: from.to_owned(),
            to: to.to_owned(),
        };

        if let Some(hit) = self
            .0
            .stats
            .lock()
            .ok()
            .and_then(|stats| stats.iter().find(|(k, _)| *k == key).map(|(_, s)| *s))
        {
            return hit;
        }

        let (additions, deletions) = git::diff_stat(repo, from, to).await.unwrap_or_default();
        let stat = Stat {
            additions,
            deletions,
        };

        if let Ok(mut stats) = self.0.stats.lock() {
            stats.push((key, stat));
            let overflow = stats.len().saturating_sub(STAT_CAPACITY);
            stats.drain(..overflow);
        }
        stat
    }

    /// A card's live head, recomputing at most once per `ttl`.
    ///
    /// NB: `compute` runs under the card's own staging lock on purpose. Staging
    /// the worktree is what produces the head, and the pane, the board and every
    /// navigation can all ask for the same card at once — concurrently they
    /// would collide on its scratch index's `.lock` file, and one `git add -A`
    /// per card per *request* is not a cost the board can carry. The lock is
    /// per card because the index it protects is: two cards stage into
    /// different files and have no reason to wait for each other, and one cold
    /// card with a large checkout used to hold up every request in the process.
    pub async fn head<F, Fut>(&self, card_id: i64, ttl: Duration, compute: F) -> Option<String>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Option<String>>,
    {
        let slot = self.slot(card_id);
        let _staging = slot.staging.lock().await;

        if let Some((at, head)) = slot.read() {
            if at.elapsed() < ttl {
                return Some(head);
            }
        }

        let head = compute().await;
        slot.write(head.clone());
        head
    }

    /// The same, for the watcher: no `ttl`, because it is the thing the `ttl`
    /// exists to back up.
    ///
    /// Staging belongs to whoever knows the worktree moved. Running it here and
    /// announcing afterwards is what keeps every reader off git: they find the
    /// head already made rather than each making it again.
    pub async fn refresh_head<F, Fut>(&self, card_id: i64, compute: F) -> Option<String>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Option<String>>,
    {
        let slot = self.slot(card_id);
        let _staging = slot.staging.lock().await;

        // NB: written even when it failed. Leaving the previous head standing
        // would hand readers a tree the worktree has moved off, with nothing
        // left to say so.
        let head = compute().await;
        slot.write(head.clone());
        head
    }

    /// What is already known of a card's head, without going and finding out.
    ///
    /// NB: no `ttl`. The watcher is the invalidator; the `ttl` on [`head`] is a
    /// backstop for the path that computes. Expiring here would flip a quiet
    /// card's board chip from its worktree's numbers to its last turn's for no
    /// reason other than the clock. The cost is that a missed inotify event
    /// leaves the chip stale until the next write, or until the drawer — which
    /// does compute — corrects it.
    ///
    /// [`head`]: DiffCache::head
    pub fn known_head(&self, card_id: i64) -> Option<String> {
        // NB: [`slot`] would mint an entry for a card that has never been
        // staged, which this is asked for once per card per board render — and
        // would put back the very entry `forget_head` removes to orphan an
        // in-flight producer.
        let slot = self.0.heads.lock().ok()?.get(&card_id).cloned()?;
        slot.read().map(|(_, head)| head)
    }

    /// The lock covering one card's head, minting one for a card that has none.
    ///
    /// NB: the map lock is taken to find it and dropped before it is used.
    /// Holding both would put every card back behind one lock, which is the
    /// whole thing this arrangement exists to avoid.
    fn slot(&self, card_id: i64) -> SlotRef {
        let Ok(mut heads) = self.0.heads.lock() else {
            return SlotRef::default();
        };
        heads.entry(card_id).or_default().clone()
    }

    fn lookup(&self, key: &Key) -> Option<Arc<Vec<ParsedFile>>> {
        let entries = self.0.entries.lock().ok()?;
        entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, parsed)| parsed.clone())
    }

    fn insert(&self, key: Key, parsed: Arc<Vec<ParsedFile>>) {
        let Ok(mut entries) = self.0.entries.lock() else {
            return;
        };

        entries.retain(|(k, _)| *k != key);
        entries.push((key, parsed));

        let overflow = entries.len().saturating_sub(CAPACITY);
        entries.drain(..overflow);
    }

    /// Drops everything belonging to a repository, for when a card's worktree and
    /// refs go away.
    pub fn forget(&self, repo: &Path) {
        if let Ok(mut entries) = self.0.entries.lock() {
            entries.retain(|(k, _)| k.repo != repo);
        }
        if let Ok(mut stats) = self.0.stats.lock() {
            stats.retain(|(k, _)| k.repo != repo);
        }
    }

    /// Drops the stats measured from `from`, for when the ref naming it moved.
    ///
    /// NB: [`DiffCache::stat`] keys on the ref name it is handed rather than the
    /// sha behind it, so a ref that moves has to say so. A rebase usually
    /// changes the worktree's tree and so misses on the other half of the key
    /// anyway; this is for the rebase that does not — upstream landing exactly
    /// what the card already carried, where the chip would otherwise keep
    /// reporting its pre-rebase numbers until teardown.
    pub fn forget_stats(&self, repo: &Path, from: &str) {
        if let Ok(mut stats) = self.0.stats.lock() {
            stats.retain(|(k, _)| !(k.repo == repo && k.from == from));
        }
    }

    /// Drops a card's memoised head, so the next read sees the worktree it
    /// actually has — or notices that it no longer has one.
    ///
    /// NB: the entry is removed rather than emptied, and the map lock goes back
    /// before the slot's is taken. A producer that is still staging holds the
    /// old slot, so what it eventually writes lands somewhere nobody can read —
    /// which is what keeps a teardown racing an in-flight restage from leaving
    /// a head behind for a worktree that has gone.
    pub fn forget_head(&self, card_id: i64) {
        let orphan = match self.0.heads.lock() {
            Ok(mut heads) => heads.remove(&card_id),
            Err(_) => return,
        };

        if let Some(slot) = orphan {
            slot.write(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(repo: &str, from: &str, to: &str) -> Key {
        Key {
            repo: PathBuf::from(repo),
            from: from.into(),
            to: to.into(),
        }
    }

    fn parsed() -> Arc<Vec<ParsedFile>> {
        Arc::new(Vec::new())
    }

    #[test]
    fn an_inserted_entry_is_found_again() {
        let cache = DiffCache::default();
        cache.insert(key("/srv/repo", "aaa", "bbb"), parsed());

        assert!(cache.lookup(&key("/srv/repo", "aaa", "bbb")).is_some());
        assert!(cache.lookup(&key("/srv/repo", "aaa", "ccc")).is_none());
        assert!(cache.lookup(&key("/other", "aaa", "bbb")).is_none());
    }

    #[test]
    fn moving_a_ref_drops_only_the_stats_measured_from_it() {
        let cache = DiffCache::default();
        let push = |repo: &str, from: &str, to: &str| {
            cache
                .0
                .stats
                .lock()
                .unwrap()
                .push((key(repo, from, to), Stat::default()));
        };
        push("/srv/repo", "refs/ledecky/1/base", "aaa");
        push("/srv/repo", "refs/ledecky/2/base", "bbb");
        push("/other", "refs/ledecky/1/base", "ccc");

        cache.forget_stats(Path::new("/srv/repo"), "refs/ledecky/1/base");

        let stats = cache.0.stats.lock().unwrap();
        let left: Vec<_> = stats.iter().map(|(k, _)| k.to.as_str()).collect();
        // Another card in the same repo, and the same card in another, both stay.
        assert_eq!(left, ["bbb", "ccc"]);
    }

    #[test]
    fn reinserting_a_key_does_not_duplicate_it() {
        let cache = DiffCache::default();
        cache.insert(key("/srv/repo", "aaa", "bbb"), parsed());
        cache.insert(key("/srv/repo", "aaa", "bbb"), parsed());

        assert_eq!(cache.0.entries.lock().unwrap().len(), 1);
    }

    #[test]
    fn the_oldest_entries_are_dropped_once_full() {
        let cache = DiffCache::default();
        for i in 0..CAPACITY + 10 {
            cache.insert(key("/srv/repo", "base", &i.to_string()), parsed());
        }

        let entries = cache.0.entries.lock().unwrap();
        assert_eq!(entries.len(), CAPACITY);
        // The earliest are gone; the most recent survive.
        assert_eq!(entries.first().unwrap().0.to, "10");
        assert_eq!(entries.last().unwrap().0.to, (CAPACITY + 9).to_string());
    }

    #[tokio::test]
    async fn one_card_staging_does_not_hold_up_another() {
        let cache = DiffCache::default();
        let staging = cache.slot(1);
        let _held = staging.staging.lock().await;

        // NB: a regression here is a hang rather than a wrong answer — one lock
        // over every card would put this behind card 1 — so the timeout is what
        // the assertion is made of.
        let answered = tokio::time::timeout(
            Duration::from_secs(5),
            cache.head(2, Duration::from_secs(60), || async {
                Some("bbb".to_owned())
            }),
        )
        .await;

        assert_eq!(answered.ok().flatten(), Some("bbb".to_owned()));
    }

    #[tokio::test]
    async fn a_forgotten_head_cannot_be_resurrected() {
        let cache = DiffCache::default();
        cache
            .head(7, Duration::from_secs(60), || async {
                Some("aaa".to_owned())
            })
            .await;

        // What a producer that started before the teardown is holding.
        let in_flight = cache.slot(7);
        cache.forget_head(7);
        in_flight.write(Some("bbb".to_owned()));

        assert_eq!(cache.known_head(7), None);
    }

    #[tokio::test]
    async fn a_failed_restage_leaves_no_head_behind() {
        let cache = DiffCache::default();
        cache
            .head(7, Duration::from_secs(60), || async {
                Some("aaa".to_owned())
            })
            .await;

        assert_eq!(cache.refresh_head(7, || async { None }).await, None);
        assert_eq!(cache.known_head(7), None);
    }

    #[test]
    fn forgetting_a_repository_leaves_the_others_alone() {
        let cache = DiffCache::default();
        cache.insert(key("/srv/a", "x", "y"), parsed());
        cache.insert(key("/srv/b", "x", "y"), parsed());

        cache.forget(Path::new("/srv/a"));

        assert!(cache.lookup(&key("/srv/a", "x", "y")).is_none());
        assert!(cache.lookup(&key("/srv/b", "x", "y")).is_some());
    }
}
