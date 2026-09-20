//! Watching a card's worktree.
//!
//! An agent editing files is the one change the server never hears about: no
//! route runs, no hook fires, the bytes just land on disk. Polling used to
//! discover it by restaging the worktree every few seconds for every card.
//! Watching it instead means the work is noticed when it happens, and staged
//! only then.
//!
//! What git ignores is not watched at all. A recursive watch spends one inotify
//! descriptor per directory, so a worktree an agent has built in costs hundreds
//! of them against a repo's dozen — enough, where `fs.inotify.max_user_watches`
//! is the common 8192 rather than this machine's 524288, for one `node_modules`
//! to exhaust the limit on its own. So the descriptor set is maintained by hand
//! instead: walked once, pruned at every ignored directory, and kept up as
//! directories come and go.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use notify::event::{CreateKind, EventKind, ModifyKind};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};

use crate::events::{Changes, Kind};
use crate::git;
use crate::review::DiffCache;

/// How many distinct paths of a burst to look at before deciding what it was.
/// A build writes far more than this; nobody edits that many by hand.
const BURST_SAMPLE: usize = 2048;

/// One watch per live worktree, kept alive by being held here.
///
/// Watches are established lazily, by whoever first asks a card for its live
/// head, so they survive a restart without a startup sweep and cost nothing for
/// cards nobody is looking at.
/// Cloned into the thread that establishes watches, the same way [`Changes`]
/// and [`DiffCache`] are cloned into the threads that use them.
#[derive(Clone)]
pub struct Worktrees(Arc<Watched>);

struct Watched {
    /// NB: behind an `Arc<Mutex<_>>` because the worker thread has to add and
    /// drop descriptors as directories appear and go, and `Watcher::watch`
    /// takes `&mut self`. The worker holds only a [`Weak`] of it — see
    /// [`Worktrees::forget`].
    watches: Mutex<HashMap<i64, Arc<Mutex<RecommendedWatcher>>>>,
    cache: DiffCache,
    changes: Changes,
    /// How long a burst settles for before it is announced. Saving one file
    /// touches it several times, and a build touches thousands.
    debounce: Duration,
}

impl Worktrees {
    pub fn new(cache: DiffCache, changes: Changes, debounce: Duration) -> Self {
        Self(Arc::new(Watched {
            watches: Mutex::new(HashMap::new()),
            cache,
            changes,
            debounce,
        }))
    }

    /// Makes sure `worktree` is being watched for this card. Idempotent.
    ///
    /// Reports whether this call is what started watching it, so a caller can
    /// tell the difference between "already covered" and "covered from now on"
    /// — the latter leaves a window before it during which writes went unseen.
    pub fn ensure(&self, card_id: i64, project_id: i64, worktree: &Path) -> bool {
        let Ok(mut watches) = self.0.watches.lock() else {
            return false;
        };
        if watches.contains_key(&card_id) {
            return false;
        }

        // NB: the walk happens under this lock, as the recursive watch it
        // replaced also did. It is one `git status` and a read of the tracked
        // directories now, rather than a descriptor per directory of build
        // output, so it holds the lock for less than it used to.
        match self.spawn(card_id, project_id, worktree) {
            Ok(watcher) => {
                watches.insert(card_id, watcher);
                true
            }
            Err(err) => {
                warn!("card {card_id}: watching {}: {err}", worktree.display());
                false
            }
        }
    }

    /// Stops watching, for a worktree that has gone.
    pub fn forget(&self, card_id: i64) {
        if let Ok(mut watches) = self.0.watches.lock() {
            // Dropping the last strong reference closes the channel, which ends
            // the thread. The thread's own reference is weak for exactly this
            // reason: a strong one here would keep the watcher — and so its
            // thread — alive forever.
            watches.remove(&card_id);
        }
    }

    fn spawn(
        &self,
        card_id: i64,
        project_id: i64,
        worktree: &Path,
    ) -> notify::Result<Arc<Mutex<RecommendedWatcher>>> {
        let (tx, rx) = mpsc::channel();

        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let Ok(event) = event else { return };
                report(&tx, event);
            })?;

        // The root first and on its own: without it nothing below can be
        // discovered, so failing to watch it is failing to watch the card.
        watcher.watch(worktree, RecursiveMode::NonRecursive)?;
        let mut watched = HashSet::from([worktree.to_path_buf()]);
        walk(worktree, &[worktree], &mut |level| {
            install(&mut watcher, &mut watched, level)
        });

        let watcher = Arc::new(Mutex::new(watcher));
        let handle = Arc::downgrade(&watcher);

        let cache = self.0.cache.clone();
        let changes = self.0.changes.clone();
        let debounce = self.0.debounce;
        let worktree = worktree.to_path_buf();

        std::thread::spawn(move || {
            // Each pass waits for a first event, then swallows the rest of the
            // burst before saying anything.
            while let Ok(first) = rx.recv() {
                let mut burst = Burst::default();
                burst.absorb(first);

                loop {
                    match rx.recv_timeout(debounce) {
                        Ok(seen) => burst.absorb(seen),
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }

                // Before the burst is weighed, not after: under sampling a
                // burst that reads as a build can still have carried a
                // directory that is not, and a directory missed here is missed
                // for as long as the card lives.
                if !burst.settle(&handle, &mut watched, &worktree) {
                    return;
                }

                let touched: Vec<PathBuf> = burst.touched.into_iter().collect();
                if git::all_ignored(&worktree, &touched) {
                    continue;
                }

                // The staged head is what went stale; the parsed diffs are
                // keyed on content and stay valid.
                cache.forget_head(card_id);
                changes.project(project_id, Kind::Diff);
            }
        });

        Ok(watcher)
    }
}

/// What the callback passes to the worker: everything about an event the worker
/// cannot work out for itself once the burst has settled.
enum Seen {
    Changed {
        paths: Vec<PathBuf>,
        /// The subset that is a directory now. Carried separately because it is
        /// exempt from [`BURST_SAMPLE`]: dropping a path costs one announcement,
        /// dropping a directory costs every announcement that subtree will ever
        /// make.
        dirs: Vec<PathBuf>,
    },
    /// The kernel's queue overflowed and events were dropped. What is under the
    /// watch set is no longer known.
    Rescan,
}

/// Classifies one event and hands the worker what it needs, or nothing.
fn report(tx: &Sender<Seen>, event: notify::Event) {
    if event.need_rescan() {
        let _ = tx.send(Seen::Rescan);
        return;
    }
    if !changed_content(&event.kind) {
        return;
    }
    // Git's own bookkeeping is not work anyone is reviewing.
    if event.paths.iter().any(|path| is_git_internal(path)) {
        return;
    }

    let dirs = match event.kind {
        // Exact: inotify says so, and nothing has to be stat'd to believe it.
        EventKind::Create(CreateKind::Folder) => event.paths.clone(),
        // A rename carries no such flag, so a directory moved into the worktree
        // has to be recognised by looking.
        EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(_)) => event
            .paths
            .iter()
            .filter(|path| is_dir(path))
            .cloned()
            .collect(),
        _ => Vec::new(),
    };

    let _ = tx.send(Seen::Changed {
        paths: event.paths,
        dirs,
    });
}

/// A burst of events, as it accumulates.
#[derive(Default)]
struct Burst {
    touched: HashSet<PathBuf>,
    dirs: Vec<PathBuf>,
    rescan: bool,
}

impl Burst {
    fn absorb(&mut self, seen: Seen) {
        match seen {
            Seen::Changed { paths, dirs } => {
                // NB: a build emits these faster than they can be read. Past
                // the cap the burst is sampled rather than collected — enough
                // to tell a build from an edit, which is all this decides.
                if self.touched.len() < BURST_SAMPLE {
                    self.touched.extend(paths);
                }
                self.dirs.extend(dirs);
            }
            Seen::Rescan => self.rescan = true,
        }
    }

    /// Brings the watch set back in line with the tree the burst left behind.
    ///
    /// Reports whether the watcher is still alive; it having gone is how a
    /// forgotten card's thread finds out.
    fn settle(
        &mut self,
        handle: &Weak<Mutex<RecommendedWatcher>>,
        watched: &mut HashSet<PathBuf>,
        worktree: &Path,
    ) -> bool {
        let Some(watcher) = handle.upgrade() else {
            return false;
        };
        let Ok(mut watcher) = watcher.lock() else {
            return false;
        };

        // A watched directory that has gone takes its subtree with it, and the
        // subtree has to be pruned by prefix: notify drops the descendants of a
        // deleted watch for us, so an exact-match prune would leave ours
        // claiming descriptors that no longer exist. Only a path in the set can
        // matter — every ancestor of a watched directory is watched too, this
        // walking top-down — which is what keeps a burst of deleted files off
        // the prefix scan.
        //
        // A directory *created* at a path already in the set counts as gone the
        // same way: it is a new inode, so the descriptor we hold belongs to the
        // one it replaced and the kernel dropped it with the old directory.
        // `rm -rf pkg && mkdir pkg` inside one debounce window would otherwise
        // leave `pkg` claimed but unwatched for as long as the card lives, the
        // existence check below seeing only the replacement.
        let stale: Vec<PathBuf> = self
            .touched
            .iter()
            .filter(|path| watched.contains(*path) && !path.exists())
            .chain(self.dirs.iter().filter(|dir| watched.contains(*dir)))
            .cloned()
            .collect();
        for path in stale {
            for dir in watched
                .iter()
                .filter(|dir| dir.starts_with(&path))
                .cloned()
                .collect::<Vec<_>>()
            {
                // An error is the ordinary case: the kernel drops the
                // descriptor itself the moment the directory does.
                let _ = watcher.unwatch(&dir);
                watched.remove(&dir);
            }
        }

        // An overflow may have swallowed anything, and a rule change can make
        // an unwatched directory watchable — so both are answered by walking
        // again rather than by reading the burst.
        if self.rescan || self.touched.iter().any(|path| is_gitignore(path)) {
            // NB: deliberately not folded into `touched`. A `.gitignore` edit
            // is already a path nothing ignores, so the burst announces on its
            // own account; adding the tree to it would announce the tree.
            let mut fresh = HashSet::new();
            walk(worktree, &[worktree], &mut |level| {
                fresh.extend(level.iter().cloned());
                install(&mut watcher, watched, level);
            });
            for dir in watched.difference(&fresh).cloned().collect::<Vec<_>>() {
                if dir != worktree {
                    let _ = watcher.unwatch(&dir);
                    watched.remove(&dir);
                }
            }
            return true;
        }

        let fresh: Vec<&Path> = self
            .dirs
            .iter()
            .filter(|dir| !watched.contains(*dir))
            .map(PathBuf::as_path)
            .collect();
        if !fresh.is_empty() {
            let touched = &mut self.touched;
            walk(worktree, &fresh, &mut |level| {
                // The directories join the burst so that a build big enough to
                // be sampled cannot hide the one new directory in it that
                // nothing ignores. Whatever was written inside them before
                // their watches landed is picked up by the staging that
                // announcement asks for.
                touched.extend(level.iter().cloned());
                install(&mut watcher, watched, level);
            });
        }

        true
    }
}

/// Walks `roots` and everything beneath them git does not ignore, handing each
/// surviving level to `keep` *before* that level is read.
///
/// That order is the whole race. A directory is watched before it is listed, so
/// anything created inside it after the listing announces itself and anything
/// created before the listing is in the listing; nothing falls between the two.
/// Listing first would lose whatever arrived in the gap, and lose it for as
/// long as the card lives.
///
/// Breadth-first so one `check-ignore` answers a whole level, and so an ignored
/// directory is pruned before it is read rather than after. Pruning at the
/// directory is exactly git's own rule — it cannot re-include a file whose
/// parent directory is excluded — so a subtree dropped here can never hold a
/// path [`git::all_ignored`] would call work. Both ask the same question of the
/// same command, which is what keeps the watch set and the announcements from
/// disagreeing about what a build is.
///
/// NB: the rules, not the tree. `git status` would answer for the whole
/// worktree in one call, but it can only report a directory it has a file in —
/// and `mkdir` ahead of the first write is exactly how a build arrives, so an
/// empty `node_modules` would be walked into and watched.
fn walk(worktree: &Path, roots: &[&Path], keep: &mut impl FnMut(&[PathBuf])) {
    let mut level: Vec<PathBuf> = roots.iter().map(|root| root.to_path_buf()).collect();

    while !level.is_empty() {
        let ignored = git::ignored(worktree, &level);
        level.retain(|dir| !ignored.contains(dir));
        keep(&level);

        let mut next = Vec::new();
        for dir in &level {
            // A directory that cannot be read is still a directory whose own
            // comings and goings are worth hearing about, so it is kept above
            // whatever this finds.
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };

            for entry in entries.flatten() {
                // NB: `file_type` does not follow symlinks, and that is the
                // point rather than an accident. notify follows them, so a link
                // to a directory is watched wherever it points — outside the
                // worktree, or round in a cycle — and reports its events under
                // the link's path. Not following is a fix, not a simplification.
                if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    continue;
                }
                let path = entry.path();
                // A linked worktree's `.git` is a file, so this only bites in a
                // repository proper — where it is most of the saving, `objects`
                // alone being a 256-way fanout.
                if path.file_name() != Some(OsStr::new(".git")) {
                    next.push(path);
                }
            }
        }
        level = next;
    }
}

/// Watches each of `dirs` that is not watched already, non-recursively.
///
/// A directory that cannot be watched is left out rather than failing the card:
/// the rest of the tree still reports, which is worth more than all-or-nothing.
fn install(watcher: &mut RecommendedWatcher, watched: &mut HashSet<PathBuf>, dirs: &[PathBuf]) {
    let mut missed = 0;

    for dir in dirs {
        if watched.contains(dir) {
            continue;
        }
        // NB: non-recursive, so notify never adds a descriptor of its own
        // below this one — every directory in the set is one we put there.
        match watcher.watch(dir, RecursiveMode::NonRecursive) {
            Ok(()) => {
                watched.insert(dir.clone());
            }
            // A directory that went away between the walk and here is nothing
            // to report; anything else is.
            Err(err) if matches!(err.kind, notify::ErrorKind::PathNotFound) => {}
            Err(_) => missed += 1,
        }
    }

    if missed > 0 {
        // The limit is per user and shared with every other watcher on the
        // machine, so the number to raise is worth naming.
        warn!("{missed} directories could not be watched; fs.inotify.max_user_watches may be low");
    }
}

/// Whether an event means the tree now differs from what was last staged.
///
/// NB: reads have to be excluded, not merely tolerated. Staging the worktree
/// opens it, which inotify reports as `Access(Open)` — so relaying those would
/// have every diff request announce a change, and the client's answering
/// request announce another, forever.
fn changed_content(kind: &EventKind) -> bool {
    match kind {
        EventKind::Create(_) | EventKind::Remove(_) => true,
        EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Name(_) | ModifyKind::Any) => true,
        _ => false,
    }
}

fn is_git_internal(path: &Path) -> bool {
    path.components()
        .any(|part| part.as_os_str() == OsStr::new(".git"))
}

/// NB: `symlink_metadata`, so a link to a directory does not read as one. What
/// follows a yes here is a watch, and watching through a link leaves the
/// worktree.
fn is_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
}

/// A rule change cannot be read from the burst — only walked again. Nested
/// files count, being named the same.
///
/// NB: `.git/info/exclude` and `core.excludesFile` are not covered: the first
/// never reaches here, being filtered as git's own bookkeeping, and the second
/// lives outside the tree entirely. Neither can make the *wrong* thing
/// announce — that stays with the burst filter — only leave a directory
/// watched that no longer needs to be.
fn is_gitignore(path: &Path) -> bool {
    path.file_name() == Some(OsStr::new(".gitignore"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::run;

    /// A repository to walk, with the ignore rules already committed.
    fn scratch(name: &str, ignore: &str, dirs: &[&str]) -> PathBuf {
        let repo =
            std::env::temp_dir().join(format!("ledecky-watch-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(&repo).unwrap();

        run(&repo, &["init", "-q", "--initial-branch=main"]).unwrap();
        run(&repo, &["config", "user.email", "t@example.com"]).unwrap();
        run(&repo, &["config", "user.name", "t"]).unwrap();

        for dir in dirs {
            std::fs::create_dir_all(repo.join(dir)).unwrap();
            std::fs::write(repo.join(dir).join("f"), "").unwrap();
        }
        std::fs::write(repo.join(".gitignore"), ignore).unwrap();
        run(&repo, &["add", "-A"]).unwrap();
        run(&repo, &["commit", "-qm", "first"]).unwrap();

        repo
    }

    /// What the watcher would end up holding descriptors for.
    fn watched(worktree: &Path, roots: &[&Path]) -> Vec<PathBuf> {
        let mut found = Vec::new();
        walk(worktree, roots, &mut |level| found.extend_from_slice(level));
        found.sort();
        found
    }

    /// The whole point: a build's directories cost nothing, however many of
    /// them there are, and `.git` — a 256-way fanout under `objects` alone —
    /// costs nothing either.
    #[test]
    fn the_walk_stops_at_what_git_ignores() {
        let repo = scratch(
            "ignored",
            "target/\n",
            &["src/inner", "target/debug/deps", "target/release"],
        );

        assert_eq!(
            watched(&repo, &[&repo]),
            [repo.clone(), repo.join("src"), repo.join("src/inner")]
        );
    }

    /// Tracked work under an ignored path is still work, and git reads it that
    /// way — so the walk has to as well, or the pane would stop keeping up with
    /// a file its diff is showing.
    #[test]
    fn an_ignored_directory_holding_tracked_work_is_still_watched() {
        let repo = scratch("tracked", "build/\n", &[]);
        std::fs::create_dir_all(repo.join("build")).unwrap();
        std::fs::write(repo.join("build/kept.txt"), "tracked anyway\n").unwrap();
        run(&repo, &["add", "-Af"]).unwrap();
        run(&repo, &["commit", "-qm", "tracked under an ignored path"]).unwrap();

        assert_eq!(watched(&repo, &[&repo]), [repo.clone(), repo.join("build")]);
    }

    /// A new directory is walked from itself, which is what a watch established
    /// after the fact can be given without redoing the tree.
    #[test]
    fn a_new_directory_is_walked_from_where_it_starts() {
        let repo = scratch("subtree", "target/\n", &["pkg/deep", "pkg/target/debug"]);

        let pkg = repo.join("pkg");
        assert_eq!(watched(&repo, &[&pkg]), [pkg.clone(), pkg.join("deep")]);

        // And a root that is itself ignored is not walked at all.
        assert!(watched(&repo, &[&pkg.join("target")]).is_empty());
    }

    /// notify follows symlinks, so a recursive watch on a link to somewhere
    /// else watches somewhere else — and reports it under a path in here.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_directory_is_not_followed() {
        let repo = scratch("links", "\n", &["src"]);
        // Outside the repository, which is the whole hazard: following this
        // would watch a directory no card has anything to do with.
        let elsewhere = repo.with_extension("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, repo.join("link")).unwrap();

        assert_eq!(watched(&repo, &[&repo]), [repo.clone(), repo.join("src")]);
    }

    /// A `mkdir` lands before the first write, and git has nothing to say about
    /// a directory it has no file in — so the walk has to read the rules rather
    /// than the tree, or a build's first directory would be watched and every
    /// directory under it with it.
    #[test]
    fn an_empty_directory_is_still_read_against_the_rules() {
        let repo = scratch("empty", "target/\n", &[]);
        std::fs::create_dir_all(repo.join("target")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();

        assert_eq!(watched(&repo, &[&repo]), [repo.clone(), repo.join("src")]);
    }
}
