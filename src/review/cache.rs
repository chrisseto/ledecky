use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use rocket::serde::Serialize;
use tokio::sync::Semaphore;

use crate::git;
use crate::review::diff::{self, Change, ParsedFile};

/// Bytes of parsed files to keep before dropping the least recently used.
const BUDGET: usize = 256 << 20;

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
/// Kept per file and keyed on the blobs at either end rather than on the range,
/// so a worktree write re-highlights the files it touched and nothing else.
/// Content addressed, so an entry can never go stale and there is no
/// invalidation to get wrong. Entries are dropped when a card is torn down, and
/// otherwise once [`BUDGET`] is spent.
/// Cloned into the worktree watcher, which invalidates a card's head from a
/// thread of its own. Otherwise shared exactly as Rocket managed state.
#[derive(Default, Clone)]
pub struct DiffCache(Arc<Store>);

struct Store {
    files: Mutex<Files>,
    stats: Mutex<Vec<(Key, Stat)>>,
    /// A lock per card rather than one over all of them — see [`DiffCache::head`].
    heads: Mutex<HashMap<i64, SlotRef>>,
    /// The right to run one `git diff | delta`: one ration for the process, so
    /// two cards asking at once queue rather than each taking a core apiece.
    /// Taken per file, so a card wanting one is not stuck behind another's
    /// thirty.
    highlighting: Semaphore,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            files: Mutex::default(),
            stats: Mutex::default(),
            heads: Mutex::default(),
            highlighting: Semaphore::new(
                std::thread::available_parallelism().map_or(4, |cores| cores.get()),
            ),
        }
    }
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

struct Files {
    entries: HashMap<FileKey, Entry>,
    bytes: usize,
    budget: usize,
    /// Bumped on every touch, so eviction can find the least recently used.
    clock: u64,
}

impl Default for Files {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            bytes: 0,
            budget: BUDGET,
            clock: 0,
        }
    }
}

struct Entry {
    parsed: Vec<Arc<ParsedFile>>,
    bytes: usize,
    used: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FileKey {
    repo: PathBuf,
    change: Change,
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
    /// The parsed diff between two revisions, computing the files it misses.
    pub async fn get(&self, repo: &Path, from: &str, to: &str) -> Result<Vec<Arc<ParsedFile>>> {
        // NB: resolved once, so a ref moving part way through cannot pair one
        // file's blobs with another revision's diff of it.
        let resolved = git::run(repo, &["rev-parse", from, to]).await?;
        let Some((from, to)) = resolved.split_once('\n') else {
            anyhow::bail!("rev-parse {from} {to} answered {resolved:?}");
        };

        let keys: Vec<FileKey> = diff::changes(repo, from, to)
            .await?
            .into_iter()
            .map(|change| FileKey {
                repo: repo.to_path_buf(),
                change,
            })
            .collect();

        let mut found: Vec<Option<Vec<Arc<ParsedFile>>>> =
            keys.iter().map(|key| self.lookup(key)).collect();

        // NB: one pipeline per file, run against the process-wide ration of
        // them. Highlighting is single threaded inside delta and costs about
        // the same per line whatever else is in the diff, so this is where the
        // wall clock goes.
        let mut work = tokio::task::JoinSet::new();
        for (index, key) in keys.iter().enumerate() {
            if found[index].is_some() {
                continue;
            }
            let (key, from, to, store) =
                (key.clone(), from.to_owned(), to.to_owned(), self.0.clone());
            work.spawn(async move {
                let _permit = store.highlighting.acquire().await;
                let parsed = diff::between(&key.repo, &from, &to, &key.change.paths()).await;
                (index, key, parsed)
            });
        }

        while let Some(done) = work.join_next().await {
            let (index, key, parsed) = done?;
            let mut parsed = parsed?;
            // NB: the `diff --git` header is ambiguous once a path has a space
            // in it; `--raw -z` is not.
            if let [file] = parsed.as_mut_slice() {
                file.path = key.change.new_path.clone();
                file.old_path = Some(key.change.old_path.clone());
            }
            let parsed: Vec<Arc<ParsedFile>> = parsed.into_iter().map(Arc::new).collect();
            self.insert(key, parsed.clone());
            found[index] = Some(parsed);
        }

        Ok(found.into_iter().flatten().flatten().collect())
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

    fn lookup(&self, key: &FileKey) -> Option<Vec<Arc<ParsedFile>>> {
        let mut files = self.0.files.lock().ok()?;
        files.clock += 1;
        let clock = files.clock;
        let entry = files.entries.get_mut(key)?;
        entry.used = clock;
        Some(entry.parsed.clone())
    }

    fn insert(&self, key: FileKey, parsed: Vec<Arc<ParsedFile>>) {
        let Ok(mut files) = self.0.files.lock() else {
            return;
        };

        files.clock += 1;
        let entry = Entry {
            bytes: parsed.iter().map(|file| file.weight()).sum(),
            parsed,
            used: files.clock,
        };
        files.bytes += entry.bytes;
        if let Some(previous) = files.entries.insert(key, entry) {
            files.bytes -= previous.bytes;
        }

        if files.bytes <= files.budget {
            return;
        }
        let mut ages: Vec<(u64, FileKey)> = files
            .entries
            .iter()
            .map(|(key, entry)| (entry.used, key.clone()))
            .collect();
        ages.sort_unstable_by_key(|(used, _)| *used);
        for (_, key) in ages {
            if files.bytes <= files.budget {
                break;
            }
            if let Some(evicted) = files.entries.remove(&key) {
                files.bytes -= evicted.bytes;
            }
        }
    }

    /// Drops everything belonging to a repository, for when a card's worktree and
    /// refs go away.
    pub fn forget(&self, repo: &Path) {
        if let Ok(mut files) = self.0.files.lock() {
            let mut freed = 0;
            files.entries.retain(|key, entry| {
                let keep = key.repo != repo;
                if !keep {
                    freed += entry.bytes;
                }
                keep
            });
            files.bytes -= freed;
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

    fn file_key(repo: &str, blob: &str) -> FileKey {
        FileKey {
            repo: PathBuf::from(repo),
            change: Change {
                old_mode: "100644".into(),
                new_mode: "100644".into(),
                old_blob: "0".repeat(40),
                new_blob: blob.into(),
                old_path: "f.rs".into(),
                new_path: "f.rs".into(),
            },
        }
    }

    fn parsed() -> Vec<Arc<ParsedFile>> {
        diff::parse("diff --git a/f.rs b/f.rs\n@@ -1 +1 @@\n-a\n+b\n")
            .into_iter()
            .map(Arc::new)
            .collect()
    }

    fn weight() -> usize {
        parsed().iter().map(|file| file.weight()).sum()
    }

    #[test]
    fn an_inserted_entry_is_found_again() {
        let cache = DiffCache::default();
        cache.insert(file_key("/srv/repo", "aaa"), parsed());

        assert!(cache.lookup(&file_key("/srv/repo", "aaa")).is_some());
        assert!(cache.lookup(&file_key("/srv/repo", "bbb")).is_none());
        assert!(cache.lookup(&file_key("/other", "aaa")).is_none());
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
    fn reinserting_a_key_does_not_count_it_twice() {
        let cache = DiffCache::default();
        cache.insert(file_key("/srv/repo", "aaa"), parsed());
        cache.insert(file_key("/srv/repo", "aaa"), parsed());

        let files = cache.0.files.lock().unwrap();
        assert_eq!(files.entries.len(), 1);
        assert_eq!(files.bytes, weight());
    }

    #[test]
    fn the_least_recently_used_go_once_over_budget() {
        let cache = DiffCache::default();
        cache.0.files.lock().unwrap().budget = 3 * weight();
        for blob in ["a", "b", "c"] {
            cache.insert(file_key("/srv/repo", blob), parsed());
        }

        // Read since, so younger than `b` in every way that matters.
        cache.lookup(&file_key("/srv/repo", "a"));
        cache.insert(file_key("/srv/repo", "d"), parsed());

        assert!(cache.lookup(&file_key("/srv/repo", "b")).is_none());
        for blob in ["a", "c", "d"] {
            assert!(
                cache.lookup(&file_key("/srv/repo", blob)).is_some(),
                "{blob}"
            );
        }
        assert_eq!(cache.0.files.lock().unwrap().bytes, 3 * weight());
    }

    /// Two files, and a second commit that touches only one of them.
    fn scratch_repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ledecky-cache-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .expect("git ran");
            assert!(out.status.success(), "git {args:?}");
        };

        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "test@ledecky"]);
        git(&["config", "user.name", "ledecky test"]);
        std::fs::write(dir.join("keep.rs"), "fn keep() {}\n").unwrap();
        std::fs::write(dir.join("edit.rs"), "fn edit() {}\n").unwrap();
        std::fs::write(dir.join("old name.rs"), "fn a() {}\nfn b() {}\nfn c() {}\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "base"]);

        std::fs::write(dir.join("keep.rs"), "fn keep() { 1 }\n").unwrap();
        std::fs::write(dir.join("edit.rs"), "fn edit() { 1 }\n").unwrap();
        git(&["mv", "old name.rs", "new name.rs"]);
        std::fs::write(
            dir.join("new name.rs"),
            "fn a() {}\nfn b() {}\nfn c() { 1 }\n",
        )
        .unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "one"]);

        std::fs::write(dir.join("edit.rs"), "fn edit() { 2 }\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "two"]);

        dir
    }

    #[tokio::test]
    async fn a_range_reuses_the_files_it_shares_with_another() {
        let repo = scratch_repo("reuse");
        let cache = DiffCache::default();

        let before = cache.get(&repo, "HEAD~2", "HEAD~1").await.unwrap();
        let after = cache.get(&repo, "HEAD~2", "HEAD").await.unwrap();

        let paths = |files: &[Arc<ParsedFile>]| -> Vec<String> {
            files.iter().map(|file| file.path.clone()).collect()
        };
        assert_eq!(paths(&before), ["edit.rs", "keep.rs", "new name.rs"]);
        assert_eq!(paths(&after), paths(&before));

        // Untouched between the two: the very same parse, not a second one.
        assert!(!Arc::ptr_eq(&before[0], &after[0]));
        assert!(Arc::ptr_eq(&before[1], &after[1]));
        assert!(Arc::ptr_eq(&before[2], &after[2]));

        // The rename survives being diffed on its own.
        assert_eq!(after[2].old_path.as_deref(), Some("old name.rs"));
        assert_eq!((after[2].additions, after[2].deletions), (1, 1));
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
        cache.insert(file_key("/srv/a", "x"), parsed());
        cache.insert(file_key("/srv/b", "x"), parsed());

        cache.forget(Path::new("/srv/a"));

        assert!(cache.lookup(&file_key("/srv/a", "x")).is_none());
        assert!(cache.lookup(&file_key("/srv/b", "x")).is_some());
        assert_eq!(cache.0.files.lock().unwrap().bytes, weight());
    }
}
