//! Watching a card's worktree.
//!
//! An agent editing files is the one change the server never hears about: no
//! route runs, no hook fires, the bytes just land on disk. Polling used to
//! discover it by restaging the worktree every few seconds for every card.
//! Watching it instead means the work is noticed when it happens, and staged
//! only then.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use notify::event::{EventKind, ModifyKind};
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
    watches: Mutex<HashMap<i64, RecommendedWatcher>>,
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
            // Dropping the watcher closes the channel, which ends its thread.
            watches.remove(&card_id);
        }
    }

    fn spawn(
        &self,
        card_id: i64,
        project_id: i64,
        worktree: &Path,
    ) -> notify::Result<RecommendedWatcher> {
        let (tx, rx) = mpsc::channel();

        let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            let Ok(event) = event else { return };
            if !changed_content(&event.kind) {
                return;
            }
            // Git's own bookkeeping is not work anyone is reviewing.
            if event.paths.iter().any(|path| is_git_internal(path)) {
                return;
            }
            let _ = tx.send(event.paths);
        })?;
        watcher.watch(worktree, RecursiveMode::Recursive)?;

        let cache = self.0.cache.clone();
        let changes = self.0.changes.clone();
        let debounce = self.0.debounce;
        let worktree = worktree.to_path_buf();

        std::thread::spawn(move || {
            // Each pass waits for a first event, then swallows the rest of the
            // burst before saying anything.
            while let Ok(first) = rx.recv() {
                let mut touched: HashSet<PathBuf> = first.into_iter().collect();

                loop {
                    match rx.recv_timeout(debounce) {
                        Ok(paths) => {
                            // NB: a build emits these faster than they can be
                            // read. Past the cap the burst is sampled rather
                            // than collected — enough to tell a build from an
                            // edit, which is all this decides.
                            if touched.len() < BURST_SAMPLE {
                                touched.extend(paths);
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }

                let touched: Vec<PathBuf> = touched.into_iter().collect();
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
        .any(|part| part.as_os_str() == std::ffi::OsStr::new(".git"))
}
