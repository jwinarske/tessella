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
//! Status: fill and line buckets build from tile-local geometry, and data-driven paint
//! properties bind into an interleaved per-layer buffer. The line path is byte-exact against
//! the oracle — vertex and index buffers both — which the fill path is not, because
//! `fixupPolygons` rotates a fill's rings and does not touch lines; both layers' *paint*
//! buffers are byte-exact, a rotation being invisible to a buffer that repeats one value per
//! feature. Zoom-interpolated (composite) properties are refused rather than half-written.
//! Circle, pattern and symbol layout are not implemented.

#![forbid(unsafe_code)]
#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod anchors;
pub mod circle;
pub mod fill;
pub mod fill_extrusion;
pub mod line;
pub mod paint;
pub mod raster;
pub mod symbol;
pub mod symbol_bucket;
pub mod symbol_layout;

pub use circle::CircleBucket;
pub use fill::{FillBucket, Segment};
pub use line::{ClipDistances, LineBucket, LineCap, LineJoin, LineOptions, LineVertex};
pub use paint::{BinderError, PaintBinder, Slot};
