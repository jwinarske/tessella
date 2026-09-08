//! A completion-shaped transport beside the blocking one (DR-23).
//!
//! # Why a second trait rather than an async `fetch`
//!
//! [`FileSource::fetch`](crate::source::FileSource::fetch) blocks, and on native that is correct and cheap: tiles are fetched from
//! pool workers, where waiting costs a thread that has nothing else to do. A browser has no
//! blocking fetch on the main thread and no `std::net` at all, so the same call cannot exist
//! there.
//!
//! Making `fetch` async everywhere is the obvious alternative and it is rejected. It buys a
//! waker, a future and a state machine in every caller so that one target can be different,
//! and native gains nothing from any of them. Instead a source that cannot block implements
//! [`DeferredFileSource`]: [`DeferredFileSource::request`] starts a fetch and answers
//! immediately with a [`Ticket`], and [`DeferredFileSource::poll`] answers the outcome once
//! there is one. `Coalescing`, `Router`, `Shared` and the `Cache-Control` parse are untouched —
//! they sit on [`Response`](crate::source::Response), not on how it arrived.
//!
//! # Why a ticket and not a handle
//!
//! A `u64` into a table the source owns can be held by JavaScript; a pointer into Rust memory
//! cannot. That is the whole reason for the indirection, and it has a second effect worth
//! stating: a ticket a consumer invents, or keeps past its answer, addresses nothing. It reads
//! as [`None`] rather than as any other request's bytes, so the worst a wrong `u64` can do is
//! lose one fetch's result to its own caller.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use crate::source::{FetchError, Fetched};

/// A handle to one in-flight request, owned by the source that issued it.
///
/// Opaque on purpose, and convertible to and from `u64` because a consumer on the other side of
/// the ABI holds it as a number. Zero is never issued, so a zeroed word from a foreign caller
/// cannot name a live request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ticket(u64);

impl Ticket {
    /// The ticket a `u64` names.
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// The `u64` a consumer holds.
    #[must_use]
    pub const fn into_raw(self) -> u64 {
        self.0
    }
}

/// Somewhere bytes come from, for a caller that cannot wait for them.
///
/// The counterpart to [`FileSource`](crate::source::FileSource). An implementation must not
/// block in either method: `request` starts work and returns, `poll` reports.
pub trait DeferredFileSource: Send + Sync {
    /// Starts a fetch and names it.
    ///
    /// `etag` tells the origin which copy the caller already holds, exactly as
    /// [`FileSource::fetch_conditional`](crate::source::FileSource::fetch_conditional) does. A
    /// source with no notion of conditional requests may ignore it and fetch normally, which is
    /// always correct and merely slower.
    fn request(&self, url: &str, etag: Option<&str>) -> Ticket;

    /// The outcome, if there is one yet.
    ///
    /// [`None`] means still in flight. A ticket is *consumed* by the poll that answers it, so
    /// polling the same ticket again also answers [`None`]: the result has an owner, and it is
    /// whoever took it. A ticket that was never issued, or was cancelled, answers [`None`] too —
    /// there is deliberately no way to tell those apart, because a caller that has to ask has
    /// already lost track of its own request.
    fn poll(&self, ticket: Ticket) -> Option<Fetched>;

    /// Abandons a request whose answer nobody will read.
    ///
    /// Needed because a table with no way to forget is a leak: a view that closes with tiles in
    /// flight would otherwise hold their bodies until the source itself was dropped. Cancelling
    /// does not stop work already running — it says the result is unwanted, so it is dropped
    /// when it lands rather than stored.
    fn cancel(&self, ticket: Ticket);

    /// How many requests have been issued and neither answered nor cancelled.
    ///
    /// The deferred half of the settle question: a caller asking whether anything further is
    /// coming reads this the way it reads
    /// [`outstanding`](crate::source::Coalescing::stats) on the blocking side.
    fn outstanding(&self) -> usize;
}

/// One request's place in the table.
enum Slot {
    /// Issued, nothing posted.
    Waiting,
    /// Landed and not yet taken.
    Done(Fetched),
}

struct Table {
    /// The next ticket to issue. Starts at one so zero names nothing.
    next: u64,
    slots: BTreeMap<u64, Slot>,
}

/// The table a [`DeferredFileSource`] hands out tickets into.
///
/// Written once here rather than in each implementation, because the awkward parts are the same
/// for all of them: a result posted for a cancelled ticket must be dropped rather than
/// resurrect the entry, and a request whose work unwinds must post *something* or its caller
/// waits for ever. [`Tickets::post`] and [`Tickets::abandon`] are the two halves of that.
#[derive(Debug)]
pub struct Tickets {
    table: Mutex<Table>,
}

impl std::fmt::Debug for Table {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let held = self.slots.values().filter(|s| matches!(s, Slot::Done(_)));
        f.debug_struct("Table")
            .field("issued", &self.next.saturating_sub(1))
            .field("open", &self.slots.len())
            .field("landed", &held.count())
            .finish()
    }
}

impl Default for Tickets {
    fn default() -> Self {
        Self::new()
    }
}

impl Tickets {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self {
            table: Mutex::new(Table {
                next: 1,
                slots: BTreeMap::new(),
            }),
        }
    }

    /// Reserves a ticket, in the waiting state.
    pub fn issue(&self) -> Ticket {
        let mut held = self.table.lock().unwrap_or_else(PoisonError::into_inner);
        let id = held.next;
        // A `u64` at one request per nanosecond runs for five hundred years, so the saturating
        // arithmetic is a formality rather than a case with behaviour. It is here so that the
        // impossible case reuses the last id instead of wrapping to zero, which *is* a live
        // ticket's name.
        held.next = held.next.saturating_add(1);
        held.slots.insert(id, Slot::Waiting);
        Ticket(id)
    }

    /// Records an outcome, if anyone is still waiting for it.
    ///
    /// A post for a cancelled or already-taken ticket is dropped. Re-inserting would make
    /// [`Self::cancel`] a suggestion rather than a release.
    pub fn post(&self, ticket: Ticket, outcome: Fetched) {
        let mut held = self.table.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(slot) = held.slots.get_mut(&ticket.0) {
            *slot = Slot::Done(outcome);
        }
    }

    /// Posts a failure for a request whose work did not get as far as an outcome.
    ///
    /// The deferred counterpart of the drop guard in [`crate::shared::Shared`]: a worker that
    /// unwinds must not leave its ticket waiting for a result that will never be posted. Does
    /// nothing if the ticket already has one, so it is safe to run unconditionally on the way
    /// out of a request.
    pub fn abandon(&self, ticket: Ticket, url: &str) {
        let mut held = self.table.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(slot @ Slot::Waiting) = held.slots.get_mut(&ticket.0) {
            *slot = Slot::Done(Err(FetchError::LeaderLost {
                url: url.to_string(),
            }));
        }
    }

    /// Takes the outcome if it has landed, removing the entry.
    pub fn take(&self, ticket: Ticket) -> Option<Fetched> {
        let mut held = self.table.lock().unwrap_or_else(PoisonError::into_inner);
        match held.slots.get(&ticket.0) {
            Some(Slot::Done(_)) => match held.slots.remove(&ticket.0) {
                Some(Slot::Done(outcome)) => Some(outcome),
                // Removed under the same lock the match read it under, so this cannot happen.
                _ => None,
            },
            _ => None,
        }
    }

    /// Forgets a ticket, whether or not its result has landed.
    pub fn cancel(&self, ticket: Ticket) {
        self.table
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .slots
            .remove(&ticket.0);
    }

    /// Tickets issued and neither taken nor cancelled.
    #[must_use]
    pub fn open(&self) -> usize {
        self.table
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .slots
            .len()
    }

    /// Of those, the ones still waiting on their source.
    #[must_use]
    pub fn waiting(&self) -> usize {
        self.table
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .slots
            .values()
            .filter(|slot| matches!(slot, Slot::Waiting))
            .count()
    }
}
