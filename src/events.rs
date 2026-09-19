//! The change bus: what the board is told about, and how it hears.
//!
//! Every fragment that used to poll now sits on a named event from here. The
//! payload is deliberately just a signal — rendering stays in the ordinary
//! routes, so ETags keep working and a connected client costs no template work.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use rocket::response::stream::{Event, EventStream};
use rocket::{get, State};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

use crate::db::Db;
use crate::project::Card;
use crate::watch::Worktrees;

/// How many changes may pile up behind a slow client before it is told to
/// resync wholesale. Generous: a `Lagged` client costs a full reload.
const BACKLOG: usize = 256;

/// Which fragment a change concerns. The names are the SSE event names, and so
/// are what `hx-trigger="<kind> from:body"` listens for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A card appeared, moved, or changed enough to redraw the board.
    Board,
    /// One card's agent changed state.
    State,
    /// The worktree moved under a card, so its diff is stale.
    Diff,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Board => "board",
            Kind::State => "state",
            Kind::Diff => "diff",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Change {
    pub project_id: i64,
    pub card_id: Option<i64>,
    pub kind: Kind,
    /// Monotonic, so a reconnecting client's `Last-Event-ID` means something.
    seq: u64,
}

/// Cloned into the threads that outlive a request — the prompt deliverer, the
/// worktree watcher — the same way [`Db`] is.
#[derive(Clone)]
pub struct Changes(Arc<Bus>);

struct Bus {
    tx: broadcast::Sender<Change>,
    seq: AtomicU64,
}

impl Default for Changes {
    fn default() -> Self {
        let (tx, _) = broadcast::channel(BACKLOG);
        Self(Arc::new(Bus {
            tx,
            seq: AtomicU64::new(0),
        }))
    }
}

impl Changes {
    pub fn subscribe(&self) -> broadcast::Receiver<Change> {
        self.0.tx.subscribe()
    }

    /// Announces a change on a project. Nobody listening is the normal case.
    pub fn project(&self, project_id: i64, kind: Kind) {
        self.emit(project_id, None, kind);
    }

    /// Announces a change on a card, resolving which board it belongs to.
    ///
    /// NB: takes the db lock to find the project. Every caller is a mutation
    /// that just released it, and these are far rarer than reads.
    pub fn card(&self, db: &Db, card_id: i64, kind: Kind) {
        let project_id = Card::find(&db.lock(), card_id).map(|card| card.project_id);
        if let Some(project_id) = project_id {
            self.emit(project_id, Some(card_id), kind);
        }
    }

    fn emit(&self, project_id: i64, card_id: Option<i64>, kind: Kind) {
        let seq = self.0.seq.fetch_add(1, Ordering::Relaxed);
        let _ = self.0.tx.send(Change {
            project_id,
            card_id,
            kind,
            seq,
        });
    }
}

/// The one connection a board holds open.
///
/// Everything the page needs arrives here and is fanned out to fragments by
/// event name, so opening a card or a review pane adds no connection of its own.
///
/// NB: signals, not rendered HTML. Pushing the fragment would save a round trip
/// on the board and the state chip, but it cannot work for the review pane:
/// which range, which expansions and which line has its comment box open all
/// live in that client's query string, and the server has no idea what any
/// given reader is looking at. Rendering it here would mean tracking per-client
/// view state — exactly the state this design keeps in the URL — and would give
/// up the conditional GET that makes an unchanged fragment cost a 304.
#[get("/events?<project>")]
pub fn stream(
    db: &State<Db>,
    changes: &State<Changes>,
    worktrees: &State<Worktrees>,
    project: i64,
) -> EventStream![Event] {
    let mut rx = changes.subscribe();

    // Someone is looking at this board, so its live worktrees are worth
    // watching. Doing it on connect rather than at worktree creation is what
    // carries the watches across a restart.
    //
    // NB: off this thread. Establishing a recursive watch walks the whole
    // checkout — ~90ms for one an agent has run a build in — and this is a sync
    // handler, so doing it inline would hold a Rocket worker and keep the
    // stream from opening for as long as it took. Nothing below waits on it.
    let db = db.inner().clone();
    let worktrees = worktrees.inner().clone();
    let announce = changes.inner().clone();
    std::thread::spawn(move || {
        // Bound first: a `for` loop holds its head expression's temporaries for
        // the whole loop, which would sit on the db lock for the walk.
        let live = Card::live_worktrees(&db.lock(), project);
        let started: Vec<i64> = live
            .into_iter()
            .filter(|(card_id, path)| worktrees.ensure(*card_id, project, Path::new(path)))
            .map(|(card_id, _)| card_id)
            .collect();

        // A watch that has only just been established missed anything written
        // while it was being set up, so say the diff moved once rather than
        // leave a reader on a worktree that has already changed. Nothing is
        // said when everything was already covered, which is every connect
        // after the first.
        for card_id in started {
            announce.card(&db, card_id, Kind::Diff);
        }
    });

    EventStream! {
        // Every connection opens with a resync. A page renders before it gets
        // here, and an agent starting in that gap would otherwise go unheard —
        // as would everything a backgrounded tab missed, since nothing below
        // replays. One conditional fetch per fragment is cheaper than the poll
        // this replaced, and it makes "the page is current" true by
        // construction rather than by luck.
        yield Event::data("").event("reload");

        loop {
            match rx.recv().await {
                Ok(change) if change.project_id == project => {
                    let data = change.card_id.map(|id| id.to_string()).unwrap_or_default();
                    yield Event::data(data)
                        .event(change.kind.as_str())
                        .id(change.seq.to_string());
                }
                // Another board's change. Not ours to relay.
                Ok(_) => {}
                // Too far behind to say what was missed, so say everything.
                Err(RecvError::Lagged(_)) => {
                    yield Event::data("").event("reload");
                }
                Err(RecvError::Closed) => break,
            }
        }
    }
}
