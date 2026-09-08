//! The ticket table: what it promises about a result's owner, and about forgetting.

use tessella_storage::deferred::{Ticket, Tickets};
use tessella_storage::source::{FetchError, Response};

use std::sync::Arc;

fn body(bytes: &[u8]) -> Result<Arc<Response>, FetchError> {
    Ok(Arc::new(Response {
        status: 200,
        body: bytes.to_vec(),
        ..Response::default()
    }))
}

#[test]
fn an_issued_ticket_is_open_and_has_no_answer() {
    let tickets = Tickets::new();
    let ticket = tickets.issue();
    assert_eq!(tickets.open(), 1);
    assert_eq!(tickets.waiting(), 1);
    assert!(tickets.take(ticket).is_none());
    // Still open: a poll that answered `None` has not consumed anything.
    assert_eq!(tickets.open(), 1);
}

#[test]
fn a_posted_result_has_exactly_one_owner() {
    let tickets = Tickets::new();
    let ticket = tickets.issue();
    tickets.post(ticket, body(b"tile"));
    assert_eq!(tickets.waiting(), 0);
    assert_eq!(tickets.open(), 1);

    let taken = tickets.take(ticket).expect("landed");
    assert_eq!(taken.expect("ok").body, b"tile");
    // The second poll of a landed ticket answers nothing. Two callers must not both believe
    // they hold the tile, and a table that answered twice would let them.
    assert!(tickets.take(ticket).is_none());
    assert_eq!(tickets.open(), 0);
}

#[test]
fn cancelling_releases_the_entry_and_drops_what_lands_after() {
    let tickets = Tickets::new();
    let ticket = tickets.issue();
    tickets.cancel(ticket);
    assert_eq!(tickets.open(), 0);

    // The work was already running; its result arrives with nobody to read it. Storing it would
    // make `cancel` a suggestion, and the table would grow by one entry per abandoned view.
    tickets.post(ticket, body(b"too late"));
    assert_eq!(tickets.open(), 0);
    assert!(tickets.take(ticket).is_none());
}

#[test]
fn abandoning_a_waiting_ticket_fails_it_rather_than_stranding_it() {
    let tickets = Tickets::new();
    let ticket = tickets.issue();
    tickets.abandon(ticket, "https://example.invalid/a.mvt");

    match tickets.take(ticket) {
        Some(Err(FetchError::LeaderLost { url })) => {
            assert_eq!(url, "https://example.invalid/a.mvt");
        }
        other => panic!("expected a lost leader, got {other:?}"),
    }
}

#[test]
fn abandoning_does_not_overwrite_a_result_that_already_landed() {
    let tickets = Tickets::new();
    let ticket = tickets.issue();
    tickets.post(ticket, body(b"tile"));
    // The guard runs on the way out of every request, including the ones that succeeded.
    tickets.abandon(ticket, "https://example.invalid/a.mvt");

    let taken = tickets.take(ticket).expect("landed");
    assert_eq!(taken.expect("ok").body, b"tile");
}

#[test]
fn a_ticket_nobody_issued_names_nothing() {
    let tickets = Tickets::new();
    let live = tickets.issue();
    tickets.post(live, body(b"tile"));

    // Zero is never issued, so a zeroed word off the ABI cannot name a live request.
    assert!(tickets.take(Ticket::from_raw(0)).is_none());
    assert!(tickets.take(Ticket::from_raw(u64::MAX)).is_none());
    // And none of that disturbed the one that is real.
    assert_eq!(tickets.open(), 1);
    assert!(tickets.take(live).is_some());
}

#[test]
fn tickets_are_distinct_and_survive_the_round_trip_through_a_u64() {
    let tickets = Tickets::new();
    let first = tickets.issue();
    let second = tickets.issue();
    assert_ne!(first, second);
    assert_ne!(first.into_raw(), 0);

    tickets.post(second, body(b"second"));
    // A consumer holds the number, not the type.
    let held = Ticket::from_raw(second.into_raw());
    let taken = tickets.take(held).expect("landed");
    assert_eq!(taken.expect("ok").body, b"second");
    assert_eq!(tickets.waiting(), 1);
}
