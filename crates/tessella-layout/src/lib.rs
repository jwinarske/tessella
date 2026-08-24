//! Bucket generation and symbol layout — plan.md §7, descends from mbgl `layout/` and the
//! layout half of `text/`.
//!
//! Scope: fill tessellation (earcut), line join/cap, circle, pattern, and symbol
//! shaping/quads. Buckets are functions of `(tile, layer, tile zoom)` and camera-free,
//! which is what makes them process-scoped and shareable across views (§5.1) — zoom
//! interpolation lives in `_t` uniforms rather than vertices, so one set of shared vertices
//! serves views at different fractional zooms of the same tile level.
//!
//! The parallel unit is `(tile, layer-family)`: fill/line tessellation of a tile proceeds
//! across layers while symbol shaping of that same tile runs independently (§12.2).
//! Vertex formats are i16 tile-local positions and u16 indices with u32 spill per segment
//! (§12.4); the C++ formats are the floor, not the target.
//!
//! Status: fill and line buckets build from tile-local geometry. The line path is byte-exact
//! against the oracle — vertex and index buffers both — which the fill path is not, because
//! `fixupPolygons` rotates a fill's rings and does not touch lines. Circle, pattern and symbol
//! layout are not implemented.

#![forbid(unsafe_code)]
#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod fill;
pub mod line;

pub use fill::{FillBucket, Segment};
pub use line::{ClipDistances, LineBucket, LineCap, LineJoin, LineOptions, LineVertex};
