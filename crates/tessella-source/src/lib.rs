//! Vector, raster and GeoJSON sources — plan.md §7, descends from mbgl `style/sources` and
//! `renderer/sources`.
//!
//! Scope: source definitions, TileJSON, MVT decode, GeoJSON, and point clustering.
//! Decode is zero-copy by design (§12.2): a varint cursor over the fetch buffer with
//! geometry decoded straight into the slab arena, and no intermediate feature
//! materialization for layers that never read properties.
//!
//! Status: GeoJSON features read from inline style data or a URL, and MVT tiles decode — the
//! decoder is checked against the spec's conformance fixtures and benchmarked against the tile
//! maplibre-native benchmarks, where it runs at about half the instruction count. The network
//! path lives in `tessella-storage`. Point clustering is `supercluster.hpp` transcribed and
//! checked against its own expectations; what is not built is `clusterProperties`, the
//! map/reduce pair that accumulates arbitrary fields into a cluster, which the style layer does
//! not parse either.

#![forbid(unsafe_code)]
#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod clip;
pub mod cluster;
pub mod geojson;
#[cfg(feature = "gltf")]
pub mod gltf;
pub mod image;
pub mod kdbush;
#[cfg(feature = "gltf")]
pub mod meshopt;
pub mod mvt;
pub mod protobuf;
pub mod tiling;

pub use geojson::{GeoJsonError, GeoJsonFeature, Geometry};
pub use image::{Image, ImageError};
pub use tiling::TilingOptions;
