//! Render orchestration — plan.md §7, descends from mbgl `renderer/`.
//!
//! Render layers, tweakers, paint-property binders, draw order, UBO packing, and the damage
//! gates of §6. One orchestrator ticks every view (§5.4): coherent wakeups, one pass
//! computing every view's cover against the shared store, one worker pool with priority
//! classes ordered foreground visible-tile decode > background view > prefetch, and one
//! deadline wheel for all timers (§5.5).
//!
//! All stream emission happens on this thread — the invariant inherited from mbgl's actor
//! model and preserved by DR-7 (threads and channels, no async runtime).
//!
//! The damage contract is normative, not aspirational (§6, DR-8): a parked view emits zero
//! ring bytes, pure camera motion emits camera-block bytes only, and churn emits
//! churn-proportional bytes. Three mechanisms carry it — UBO byte-compare suppression before
//! dirtying, texture dirty-rect lists, and the §6.3 split of `FrameOrder` into `CameraUpdate`
//! and `OrderUpdate` so camera cadence stops dragging the order payload behind it. The
//! orchestrator does not run a frame for a view whose transform is unchanged and whose
//! sources report no churn (§6.5).
//!
//! Status: one tile's buckets build from a style and its features, encode into geometry
//! envelopes on the ring, and are gated by damage so a parked view emits nothing. Draw order
//! and UBO packing are not implemented.

// Not `forbid`: `emit` reinterprets `#[repr(C)]` envelope records as bytes to put them on the
// ring, which is the one thing this crate does that cannot be expressed safely. Everything
// else is safe, and `deny` keeps any new unsafe deliberate.
#![deny(unsafe_code)]
#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod binder;
pub mod camera;
pub mod counters;
pub mod damage;
pub mod emit;
pub mod order;
pub mod stencil;
pub mod sweep;
pub mod tile;
pub mod ubo;
pub mod view;

pub use binder::{BoundAttribute, VertexLayout, pack_color};
pub use counters::{SharedCounters, SharedWork};
pub use damage::{CameraKey, DamageTracker, Traffic, TrafficMeter, Work};
pub use emit::{Encoded, SlabArena, encode_fill};
pub use tile::{Content, LayerBucket, TileError, TileId, build_tile};
pub use view::{GeometryBinding, ViewError, ViewSession};
