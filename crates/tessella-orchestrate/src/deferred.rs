//! The native side of the deferred transport (DR-23).
//!
//! # One code path for two targets
//!
//! A browser's transport is completion-shaped because it has no choice. Native's is not: a pool
//! worker can block on a socket, and it should, because that is the cheapest thing a thread with
//! nothing else to do can be doing. [`PoolBacked`] is what lets both be true at once — it wears
//! [`DeferredFileSource`] on the outside and submits the blocking [`FileSource::fetch`] to the
//! pool on the inside.
//!
//! That is not a shim for the sake of symmetry. It means the deferred loop — request, drain,
//! build — can be *run on Linux*, against the same sources the render tests already use, so the
//! wasm path is exercised by the suite that exists rather than only by a browser.
//!
//! # Priority is not on the trait
//!
//! §5.4's three classes are how native decides what a worker picks up next; a browser has one
//! queue and no such decision to make. Putting `Priority` in `request` would be a native concept
//! in a trait that exists for the target which has no use for it, so it is a property of the
//! [`PoolBacked`] instead: [`PoolBacked::request_at`] names a class, and the trait's `request`
//! uses the one the source was built with.

use alloc::string::ToString;
use alloc::sync::Arc;

use tessella_storage::deferred::{DeferredFileSource, Ticket, Tickets};
use tessella_storage::source::{Fetched, FileSource};

use crate::pool::{Pool, Priority};

/// A blocking [`FileSource`] wearing the deferred trait, with the pool doing the waiting.
pub struct PoolBacked<S> {
    source: Arc<S>,
    pool: &'static Pool,
    priority: Priority,
    /// Behind an [`Arc`] because a pool job is `'static` and has to carry the table with it.
    tickets: Arc<Tickets>,
}

impl<S> PoolBacked<S> {
    /// Wraps a source, fetching at `priority` unless a caller says otherwise.
    pub fn new(source: Arc<S>, pool: &'static Pool, priority: Priority) -> Self {
        Self {
            source,
            pool,
            priority,
            tickets: Arc::new(Tickets::new()),
        }
    }

    /// The wrapped source.
    pub fn inner(&self) -> &Arc<S> {
        &self.source
    }
}

/// Posts a failure for a ticket whose job did not reach one.
///
/// A pool job that unwinds is caught by the pool and counted, but the ticket it was carrying is
/// still in the table and still waiting — and a `drain` loop waiting on it never finishes. The
/// guard is the same one [`tessella_storage::shared::Shared`] holds for a coalescing leader, and
/// it is here for the same reason: the failure case must reach the caller, not merely be
/// survived.
struct Guard<'a> {
    tickets: &'a Tickets,
    ticket: Ticket,
    url: &'a str,
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.tickets.abandon(self.ticket, self.url);
    }
}

/// A deferred transport that can be told how urgent a request is.
///
/// [`DeferredFileSource`] deliberately has no priority: §5.4's three classes are how native
/// decides what a worker picks up next, and a browser has one queue and no such decision to make.
/// Putting `Priority` on that trait would also put an orchestrator type in the storage layer,
/// which is the wrong direction for it to point.
///
/// So the class lives here instead, on a trait the tile path asks for. The default ignores it and
/// falls through to [`DeferredFileSource::request`], which is exactly right for a transport with
/// one queue: it is not a stub, it is the whole of the answer for that shape. [`PoolBacked`]
/// overrides it, because it has somewhere to put the class.
pub trait TileTransport: DeferredFileSource {
    /// Starts a fetch, saying what it is competing against.
    fn request_at(&self, priority: Priority, url: &str, etag: Option<&str>) -> Ticket {
        let _ = priority;
        self.request(url, etag)
    }
}

impl<S: FileSource + 'static> TileTransport for PoolBacked<S> {
    fn request_at(&self, priority: Priority, url: &str, etag: Option<&str>) -> Ticket {
        let ticket = self.tickets.issue();

        // Everything the job touches has to be owned by it: a pool job is `'static` (see
        // `pool.rs`), and the alternative is a lifetime transmute this crate forbids.
        let tickets = Arc::clone(&self.tickets);
        let source = Arc::clone(&self.source);
        let url = url.to_string();
        let etag = etag.map(ToString::to_string);
        self.pool.submit(priority, move || {
            let guard = Guard {
                tickets: &tickets,
                ticket,
                url: &url,
            };
            let outcome = source
                .fetch_conditional(&url, etag.as_deref())
                .map(Arc::new);
            tickets.post(ticket, outcome);
            // Posted, so the guard has nothing left to do. Dropped explicitly rather than at the
            // end of the block so the order is stated rather than inferred.
            drop(guard);
        });
        ticket
    }
}

impl<S: FileSource + 'static> DeferredFileSource for PoolBacked<S> {
    fn request(&self, url: &str, etag: Option<&str>) -> Ticket {
        TileTransport::request_at(self, self.priority, url, etag)
    }

    fn poll(&self, ticket: Ticket) -> Option<Fetched> {
        self.tickets.take(ticket)
    }

    fn cancel(&self, ticket: Ticket) {
        self.tickets.cancel(ticket);
    }

    fn outstanding(&self) -> usize {
        self.tickets.open()
    }
}
