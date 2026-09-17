use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

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
#[derive(Default)]
pub struct DiffCache {
    entries: Mutex<Vec<(Key, Arc<Vec<ParsedFile>>)>>,
    stats: Mutex<Vec<(Key, Stat)>>,
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
    /// touch delta. Unlike [`DiffCache::get`] the key is taken as given: callers
    /// pass a card's base ref, which is written once and never moves, and a
    /// turn's sha, so the pair already names an immutable range.
    pub fn stat(&self, repo: &Path, from: &str, to: &str) -> Stat {
        let key = Key {
            repo: repo.to_path_buf(),
            from: from.to_owned(),
            to: to.to_owned(),
        };

        if let Some(hit) = self
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

        if let Ok(mut stats) = self.stats.lock() {
            stats.push((key, stat));
            let overflow = stats.len().saturating_sub(CAPACITY);
            stats.drain(..overflow);
        }
        stat
    }

    fn lookup(&self, key: &Key) -> Option<Arc<Vec<ParsedFile>>> {
        let entries = self.entries.lock().ok()?;
        entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, parsed)| parsed.clone())
    }

    fn insert(&self, key: Key, parsed: Arc<Vec<ParsedFile>>) {
        let Ok(mut entries) = self.entries.lock() else {
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
        if let Ok(mut entries) = self.entries.lock() {
            entries.retain(|(k, _)| k.repo != repo);
        }
        if let Ok(mut stats) = self.stats.lock() {
            stats.retain(|(k, _)| k.repo != repo);
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
    fn reinserting_a_key_does_not_duplicate_it() {
        let cache = DiffCache::default();
        cache.insert(key("/srv/repo", "aaa", "bbb"), parsed());
        cache.insert(key("/srv/repo", "aaa", "bbb"), parsed());

        assert_eq!(cache.entries.lock().unwrap().len(), 1);
    }

    #[test]
    fn the_oldest_entries_are_dropped_once_full() {
        let cache = DiffCache::default();
        for i in 0..CAPACITY + 10 {
            cache.insert(key("/srv/repo", "base", &i.to_string()), parsed());
        }

        let entries = cache.entries.lock().unwrap();
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
