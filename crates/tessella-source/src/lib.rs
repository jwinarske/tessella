//! Vector, raster and GeoJSON sources — plan.md §7, descends from mbgl `style/sources` and
//! `renderer/sources`.
//!
//! Scope: source definitions, TileJSON, MVT decode, and GeoJSON including clustering.
//! Decode is zero-copy by design (§12.2): a varint cursor over the fetch buffer with
//! geometry decoded straight into the slab arena, and no intermediate feature
//! materialization for layers that never read properties.
//!
//! Status: GeoJSON features read from inline style data. MVT decode, clustering, and the
//! network path are not implemented.

#![forbid(unsafe_code)]
#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod clip;
pub mod geojson;
pub mod mvt;
pub mod protobuf;
pub mod tiling;

pub use geojson::{GeoJsonError, GeoJsonFeature, Geometry};
pub use tiling::TilingOptions;
