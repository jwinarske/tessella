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
//! Status: one tile's buckets build from a style and its features. The ring emission, damage
//! gates, draw order and UBO packing are not implemented.

#![forbid(unsafe_code)]
#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod tile;

pub use tile::{Content, LayerBucket, TileError, TileId, build_tile};
