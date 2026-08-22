//! Vector, raster and GeoJSON sources — plan.md §7, descends from mbgl `style/sources` and
//! `renderer/sources`.
//!
//! Scope: source definitions, TileJSON, MVT decode, and GeoJSON including clustering.
//! Decode is zero-copy by design (§12.2): a varint cursor over the fetch buffer with
//! geometry decoded straight into the slab arena, and no intermediate feature
//! materialization for layers that never read properties.
//!
//! Status: scaffold. No implementation yet.

#![forbid(unsafe_code)]
