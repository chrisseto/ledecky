//! Serving one Rocket on more than one socket.

use std::fmt;
use std::io;
use std::sync::Arc;

use rocket::listener::{Endpoint, Listener};
use rocket::tokio::sync::{mpsc, Mutex};
use rocket::tokio::task::JoinHandle;

/// Every listener a Rocket serves, accepted from as they come.
pub struct All<L: Listener> {
    listeners: Vec<Arc<L>>,
    /// What the listeners have taken, in the order they took it.
    ///
    /// NB: a task apiece rather than a race between their `accept`s. Losing
    /// that race would drop a connection somebody had already been given.
    pending_accepts: Mutex<mpsc::Receiver<(usize, io::Result<L::Accept>)>>,
    accepting: Vec<JoinHandle<()>>,
}

/// A task parked in `accept` has nothing to wake it, so dropping the receiver
/// is not enough to end one.
impl<L: Listener> Drop for All<L> {
    fn drop(&mut self) {
        for task in &self.accepting {
            task.abort();
        }
    }
}

impl<L: Listener + 'static> All<L>
where
    L::Accept: 'static,
{
    /// The first is the one Rocket's liftoff line names first.
    ///
    /// NB: panics on none. A server accepting nothing would sit there looking
    /// launched.
    pub fn new(listeners: Vec<L>) -> Self {
        assert!(!listeners.is_empty(), "a server needs a listener");

        // One deep: a listener that has taken a connection stops taking more
        // until this one has been handed on.
        let (accepted, pending_accepts) = mpsc::channel(1);
        let listeners: Vec<_> = listeners.into_iter().map(Arc::new).collect();
        let accepting = listeners
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, listener)| {
                let accepted = accepted.clone();
                rocket::tokio::spawn(async move {
                    while accepted
                        .send((index, listener.accept().await))
                        .await
                        .is_ok()
                    {}
                })
            })
            .collect();

        Self {
            listeners,
            pending_accepts: Mutex::new(pending_accepts),
            accepting,
        }
    }
}

impl<L: Listener> Listener for All<L> {
    /// Which listener took it, so `connect` can hand it back to that one.
    type Accept = (usize, L::Accept);

    type Connection = L::Connection;

    async fn accept(&self) -> io::Result<Self::Accept> {
        let (index, accepted) = self
            .pending_accepts
            .lock()
            .await
            .recv()
            .await
            .ok_or_else(|| io::Error::other("every listener has stopped"))?;

        Ok((index, accepted?))
    }

    async fn connect(&self, (index, accept): Self::Accept) -> io::Result<Self::Connection> {
        self.listeners[index].connect(accept).await
    }

    fn endpoint(&self) -> io::Result<Endpoint> {
        let mut endpoints = Vec::with_capacity(self.listeners.len());
        for listener in &self.listeners {
            endpoints.push(listener.endpoint()?);
        }

        Ok(Endpoint::new(Endpoints(endpoints)))
    }
}

/// Everywhere an `All` listens, as one endpoint.
#[derive(Debug)]
struct Endpoints(Vec<Endpoint>);

impl fmt::Display for Endpoints {
    /// Space-separated, so that the first is still a URL on its own.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (nth, endpoint) in self.0.iter().enumerate() {
            match nth {
                0 => write!(f, "{endpoint}")?,
                _ => write!(f, " + {endpoint}")?,
            }
        }
        Ok(())
    }
}
