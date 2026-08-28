#![no_main]
//! A capture ring, as a consumer that did not write it sees one.
//!
//! # Why this target and not only the parsers
//!
//! Because the bytes are the one input here that another *process* writes. §3.5's spike made
//! that real: the producer and the consumer map the same file, and every number the consumer
//! walks — a record's `total_len`, its `record_len`, a span's offset and count — is one it reads
//! rather than one it computed. The ABI says so in as many words: a record is "reconstructed
//! from untrusted bytes".
//!
//! A producer fault is the likelier source than an attacker, which does not make it less worth
//! finding: a consumer that spins or panics on a malformed region takes the map down and blames
//! the wrong half.
//!
//! # The control block is built, not fuzzed
//!
//! The first version handed `attach` raw fuzz bytes and got four new coverage units in two
//! hundred thousand runs — every input failed the ABI-revision check at the front door, so the
//! walk under test never ran. What is interesting here is not that `attach` rejects garbage,
//! which one test asserts once; it is what the walk does with *records*. So the control block is
//! well formed by construction and the fuzzer owns the data region and the head, which is where
//! the records live.
//!
//! # The iteration cap
//!
//! A record whose length is zero would advance the cursor by nothing and loop for ever. That is
//! a real failure, and libFuzzer would report it as a timeout — a slow and unclear way to learn
//! it — so the walk is capped and the cap panics, naming what happened.

use libfuzzer_sys::fuzz_target;
use tessella_capture_abi::ring;

/// Far more records than a region this size can hold.
const CAP: usize = 1 << 16;

/// Big enough for several records, small enough that the fuzzer fills it.
const CAPACITY: usize = 4096;

fuzz_target!(|data: &[u8]| {
    let header = core::mem::size_of::<ring::RingControl>();
    let mut region = alloc_region(header + CAPACITY);

    // A control block a producer would have written: this revision, this capacity, nothing
    // consumed, and `head` at however many bytes the fuzzer supplied.
    let published = data.len().min(CAPACITY);
    write_control(&mut region, CAPACITY as u64, published as u64);
    region[header..header + published].copy_from_slice(&data[..published]);

    // SAFETY: the region is this function's own allocation, is `header + CAPACITY` bytes, and
    // outlives both halves.
    let Some((_producer, mut consumer)) =
        (unsafe { ring::attach(region.as_mut_ptr(), CAPACITY) })
    else {
        return;
    };

    let mut walked = 0usize;
    while let Some(record) = consumer.peek() {
        // Touch what a consumer touches: the kind, and the two slices, so a length that
        // overruns its own record is read rather than merely computed.
        let _ = record.kind;
        let _ = record.record.len();
        let _ = record.payload.len();
        let consumed = record.consumed();
        consumer.advance(consumed);
        walked += 1;
        assert!(
            walked < CAP,
            "the walk did not advance: a record consumed nothing"
        );
    }
});

/// A zeroed, suitably aligned region.
fn alloc_region(len: usize) -> Vec<u8> {
    // `RingControl` wants eight-byte alignment and a `Vec<u8>` promises one. Over-allocating a
    // `Vec<u64>` and viewing it as bytes is what the in-tree tests do for the same reason.
    let words = len.div_ceil(8);
    let mut region: Vec<u64> = vec![0; words];
    let bytes = region.as_mut_ptr().cast::<u8>();
    // SAFETY: `words * 8 >= len`, the allocation is live, and the `Vec<u64>` is forgotten so it
    // is not freed twice. The capacity is in bytes to match.
    let out = unsafe { Vec::from_raw_parts(bytes, len, words * 8) };
    core::mem::forget(region);
    out
}

/// Writes the fields `attach` checks and the counters it reads.
fn write_control(region: &mut [u8], capacity: u64, head: u64) {
    let put = |region: &mut [u8], at: usize, value: &[u8]| {
        region[at..at + value.len()].copy_from_slice(value);
    };
    put(
        region,
        core::mem::offset_of!(ring::RingControl, abi_rev),
        &tessella_capture_abi::ABI_REV.to_ne_bytes(),
    );
    put(
        region,
        core::mem::offset_of!(ring::RingControl, capacity),
        &capacity.to_ne_bytes(),
    );
    put(
        region,
        core::mem::offset_of!(ring::RingControl, head),
        &head.to_ne_bytes(),
    );
    put(
        region,
        core::mem::offset_of!(ring::RingControl, tail),
        &0u64.to_ne_bytes(),
    );
}
