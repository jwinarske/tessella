//! Glyph manager, PBF range path and local SDF rasterization — plan.md §7, descends from the
//! glyph half of mbgl `text/` and from `sprite/`.
//!
//! Owns the shared atlases: one glyph atlas per fontstack, one sprite atlas per style,
//! emitted once regardless of view count (§5.1). `contentHash` is retired from the protocol
//! precisely because this ownership makes consumer-side dedup unnecessary (§6.2).
//!
//! Two process-wide caches sit here (§12.3): a shaped-run cache keyed
//! `(fontstack, text, layout params)` — label text is massively repetitive across tiles,
//! zooms and views — and a glyph-SDF rasterization cache one level below it. Atlases are R8
//! single-channel, not RGBA: 4x on the largest persistent texture (§12.4). The shelf
//! allocator keeps insertions clustered so the §6.4 dirty-rect list rarely spills to union.
//!
//! Status: the glyph range format is read. Shaping, the atlas and local rasterization are
//! not implemented.

#![forbid(unsafe_code)]

pub mod atlas;
pub mod generated;
pub mod manager;
pub mod pbf;
pub mod quads;
pub mod shaping;
pub mod text;
