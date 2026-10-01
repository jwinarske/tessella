//! The renderer-agnostic half of a tessella consumer.
//!
//! A consumer of the capture stream is two halves. One reads the ring, joins the records into
//! drawables, groups a frame's order into batches and decides how the clip masks share a stencil
//! byte; the other uploads buffers and issues draws. The first half is the same whatever the
//! second is drawing with, and this crate is that half.
//!
//! It exists because there are now two consumers that would otherwise each write it. The Filament
//! mirror has it in C++; a Vulkan mirror would need the same logic again, and a third would need
//! it a third time. Two independent implementations of one ABI are the instrument this project
//! relies on — where both agree with the oracle and not with each other, one reads the ABI wrong —
//! but that argument is about the *drawing*, not about the arithmetic of reading a ring. Three
//! copies of the reader is triplicated work and triplicated bugs, not triplicated evidence.
//!
//! # What it is not
//!
//! It does not upload, own or outlive anything, and it never advances the ring's tail on its own.
//! §11.7 requires a slab reference be released only after the driver's copy completes, and only a
//! backend knows when that is — so reading reports how far it got and retiring is the backend's
//! separate call. A host that never retires stalls the producer, which is the correct failure: the
//! alternative is the producer reusing bytes the GPU is still reading.
//!
//! # Scope
//!
//! `no_std` with `alloc`, and it depends on nothing but the ABI. That is what keeps it honest as
//! the boundary — a reader that reached into the producer's types could agree with the producer
//! about something the wire does not actually say, which is the one class of bug two consumers
//! cannot catch between them.

#![no_std]

extern crate alloc;

pub mod join;
pub mod stencil;
