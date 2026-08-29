//! The C ABI a consumer embeds tessella through.
//!
//! # What this is and is not
//!
//! It is not a second protocol. Everything a consumer draws from arrives on the capture stream,
//! described by the one generated header (`include/tessella_capture_abi.h`); this is only the
//! handful of calls that get a producer running and a frame emitted. Anything that could travel
//! as a record does travel as a record, because a second way to say the same thing is a second
//! thing to keep in agreement.
//!
//! # Why a `staticlib`
//!
//! §3.5: the frontend's only process coupling is the ring, so a consumer links this into its own
//! shared object and the ring is ordinary memory. Promoting that to a separate process is a
//! linker change rather than a redesign — no envelope carries an in-process pointer, and slab
//! handles are offsets — so the same header and the same records serve both.
//!
//! In-process is the case worth optimising for and the case this exists for: the consumer reads
//! geometry out of the producer's own arena, and "zero copy" is not a technique but the absence
//! of a reason to copy.
//!
//! # The rules every entry point here follows
//!
//! - **Borrowed in, owned nowhere.** A `const char*` is copied before the call returns.
//! - **No panics across the boundary.** Rust unwinding into C is undefined; every entry point
//!   returns a status and reports failure rather than unwinding.
//! - **A handle is opaque and non-null.** Zero is the failure value, so a caller that ignores
//!   the status still cannot mistake a failed create for a working map.

#![deny(unsafe_op_in_unsafe_fn)]
#![allow(clippy::missing_safety_doc)]

/// How a call went.
///
/// A single `Ok` and a reason for everything else. The reasons are stable numbers because a
/// consumer logs them and a log outlives the build that wrote it.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// It worked.
    Ok = 0,
    /// A pointer argument was null where the call requires one.
    NullArgument = 1,
    /// A handle did not name a live map.
    NoSuchMap = 2,
    /// A string argument was not UTF-8.
    NotUtf8 = 3,
    /// The style did not parse.
    BadStyle = 4,
    /// The ring could not take the frame. The consumer is behind; drain and retry.
    RingFull = 5,
    /// Something failed in a way this ABI has no more specific word for. The producer logs it.
    Failed = 6,
}
