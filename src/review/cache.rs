use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use rocket::serde::Serialize;

use crate::git;
use crate::review::diff::{self, ParsedFile};

/// How many parses to keep before dropping the oldest.
const CAPACITY: usize = 64;

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
    heads: Mutex<HashMap<i64, (Instant, String)>>,
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

impl DiffCache {
    /// The parsed diff between two revisions, computing it on a miss.
    pub fn get(&self, repo: &Path, from: &str, to: &str) -> Result<Arc<Vec<ParsedFile>>> {
        // Resolving first is what makes the key stable: `refs/.../turn-2` and the
        // sha it points at are the same entry.
        let key = Key {
            repo: repo.to_path_buf(),
            from: git::run(repo, &["rev-parse", from])?,
            to: git::run(repo, &["rev-parse", to])?,
        };

        if let Some(hit) = self.lookup(&key) {
            return Ok(hit);
        }

        let parsed = Arc::new(diff::between(repo, &key.from, &key.to)?);
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
    pub fn stat(&self, repo: &Path, from: &str, to: &str) -> Stat {
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

        let (additions, deletions) = git::diff_stat(repo, from, to).unwrap_or_default();
        let stat = Stat {
            additions,
            deletions,
        };

        if let Ok(mut stats) = self.0.stats.lock() {
            stats.push((key, stat));
            let overflow = stats.len().saturating_sub(CAPACITY);
            stats.drain(..overflow);
        }
        stat
    }

    /// A card's live head, recomputing at most once per `ttl`.
    ///
    /// NB: `compute` runs under the lock on purpose. Staging the worktree is
    /// what produces the head, and the pane's poll, the board's poll and every
    /// navigation can all ask for the same card at once — concurrently they
    /// would collide on the scratch index's `.lock` file, and one `git add -A`
    /// per card per *request* is not a cost the board can carry.
    pub fn head(
        &self,
        card_id: i64,
        ttl: Duration,
        compute: impl FnOnce() -> Option<String>,
    ) -> Option<String> {
        let Ok(mut heads) = self.0.heads.lock() else {
            return compute();
        };

        if let Some((at, head)) = heads.get(&card_id) {
            if at.elapsed() < ttl {
                return Some(head.clone());
            }
        }

        let head = compute()?;
        heads.insert(card_id, (Instant::now(), head.clone()));
        Some(head)
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
    pub fn forget_head(&self, card_id: i64) {
        if let Ok(mut heads) = self.0.heads.lock() {
            heads.remove(&card_id);
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
