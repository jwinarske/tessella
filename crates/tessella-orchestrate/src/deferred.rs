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

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use alloc::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, PoisonError};

use tessella_storage::deferred::{DeferredFileSource, Ticket, Tickets};
use tessella_storage::source::{FetchError, Fetched, FileSource, Response};

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

/// A transport whose fetching is done by whoever is driving the map.
///
/// # Why the producer does not call `fetch` itself
///
/// §19.2 keeps `wasm-bindgen` out of the producer: the exports are `#[no_mangle] extern "C"` and
/// the header stays the single description of the surface. A producer that called the browser's
/// `fetch` would need bindings, a JS glue module and a second description of the ABI to keep in
/// step with the first.
///
/// So it does not call anything. It writes down what it needs and the host brings it back. The
/// ticket is what makes that safe across the boundary: the host holds a `u64`, not a pointer, so
/// a stale or invented one addresses nothing (see [`DeferredFileSource`]).
///
/// # It is not a wasm type
///
/// Nothing here is browser-specific, which is the point rather than an accident. A Rust test can
/// be the host, and one is: the suite drives a whole `TileSource` through this, answering every
/// request by hand, with no browser and no network. Whatever the browser does differently is then
/// the browser's, not this arrangement's.
pub struct HostTransport {
    tickets: Tickets,
    /// Issued and not yet handed to the host, in issue order.
    queue: Mutex<VecDeque<Ticket>>,
    /// Every unanswered request's URL, kept alive so a consumer across the ABI can read it where
    /// it lies rather than being handed a copy it has nowhere to put.
    ///
    /// A browser reads it as a byte range in linear memory, which is the same arrangement
    /// `tessella_regions` uses for the ring: the alternative is an allocator export and a copy on
    /// both sides of it. Removed when the request is answered, failed or cancelled, which is what
    /// bounds how long the pointer is good for.
    urls: Mutex<BTreeMap<Ticket, String>>,
}

impl Default for HostTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl HostTransport {
    /// A transport with nothing asked for yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tickets: Tickets::new(),
            queue: Mutex::new(VecDeque::new()),
            urls: Mutex::new(BTreeMap::new()),
        }
    }

    /// Takes the requests the host has not been given yet.
    ///
    /// Drained rather than read, because a host that was handed the same request twice would
    /// fetch it twice. In issue order: the first thing asked for is the first thing a host with
    /// one connection should go and get.
    pub fn take_requests(&self) -> Vec<(Ticket, String)> {
        let handed: Vec<Ticket> = self
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain(..)
            .collect();
        let urls = self.urls.lock().unwrap_or_else(PoisonError::into_inner);
        handed
            .into_iter()
            .filter_map(|ticket| urls.get(&ticket).map(|url| (ticket, url.clone())))
            .collect()
    }

    /// Hands over one request, for a caller that reads the URL where it lies.
    ///
    /// [`Self::take_requests`] one at a time and without the copy, which is what a consumer across
    /// the ABI wants: it gets the ticket, reads the URL out of this transport's memory, and comes
    /// back for the next one. [`None`] when there is nothing to fetch.
    pub fn next_request(&self) -> Option<Ticket> {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
    }

    /// Where a handed-out request's URL lies, and how long it is.
    ///
    /// The pointer is into this transport's own memory and is good until the ticket is answered,
    /// failed or cancelled, or the transport is dropped. A caller that holds it past any of those
    /// is holding a dangling pointer, which is why the three of them are the only things that
    /// remove an entry.
    ///
    /// [`None`] for a ticket that was never issued or is no longer outstanding.
    #[must_use]
    pub fn url_of(&self, ticket: Ticket) -> Option<(*const u8, usize)> {
        self.urls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&ticket)
            .map(|url| (url.as_ptr(), url.len()))
    }

    /// How many requests are waiting to be handed over.
    #[must_use]
    pub fn queued(&self) -> usize {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Answers a request with what the host fetched.
    ///
    /// `status` is the origin's, and a 404 is a *response* rather than a failure -- the tile path
    /// reads an absent tile as an edge of coverage, which is not the same thing as a fetch that
    /// did not happen. A ticket that was cancelled, already answered, or never issued is ignored,
    /// so a host that loses track of its own bookkeeping wastes a fetch rather than corrupting
    /// anything.
    pub fn answer(&self, ticket: Ticket, status: u16, body: Vec<u8>) {
        self.forget(ticket);
        self.tickets.post(
            ticket,
            Ok(Arc::new(Response {
                status,
                body,
                ..Response::default()
            })),
        );
    }

    /// Answers a request that the host could not fetch at all.
    ///
    /// For a connection that never opened, not for an origin that said no -- that is
    /// [`Self::answer`] with the status it said it with.
    pub fn fail(&self, ticket: Ticket, url: &str, message: &str) {
        self.forget(ticket);
        self.tickets.post(
            ticket,
            Err(FetchError::Transport {
                url: url.to_string(),
                message: message.to_string(),
            }),
        );
    }

    /// Drops a request's URL, which is what ends the life of the pointer [`Self::url_of`] gave.
    fn forget(&self, ticket: Ticket) {
        self.urls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&ticket);
    }
}

impl DeferredFileSource for HostTransport {
    fn request(&self, url: &str, _etag: Option<&str>) -> Ticket {
        // The etag is dropped rather than passed on. A host fetching through the browser gets
        // revalidation from the HTTP cache it is already sitting behind, and handing it a header
        // to set would be describing a policy it already has.
        let ticket = self.tickets.issue();
        self.urls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(ticket, url.to_string());
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(ticket);
        ticket
    }

    fn poll(&self, ticket: Ticket) -> Option<Fetched> {
        self.tickets.take(ticket)
    }

    fn cancel(&self, ticket: Ticket) {
        self.tickets.cancel(ticket);
        self.forget(ticket);
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|queued| *queued != ticket);
    }

    fn outstanding(&self) -> usize {
        self.tickets.open()
    }
}

// One queue, and no class to put a request in. The shape §5.4's priorities do not apply to, which
// is why the default is the whole answer here rather than a stub.
impl TileTransport for HostTransport {}
