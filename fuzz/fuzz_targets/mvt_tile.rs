#![no_main]
//! A vector tile, decoded from whatever an origin sent.
//!
//! The deepest parser here and the one that runs on every tile of every frame: nested protobuf
//! with lengths, counts and zigzag commands, all read from bytes a network handed over. The
//! contract is `malformed_input.rs`'s — return, either way. `forbid(unsafe_code)` means a bad
//! tile cannot corrupt memory; what it can do is panic on a worker thread, which a hostile
//! origin then triggers at will, or allocate from a count it was told rather than one it checked.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = tessella_source::mvt::Tile::decode(data);
});
