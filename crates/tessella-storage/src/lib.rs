//! Online and cache file sources with request coalescing — plan.md §7, descends from mbgl
//! `storage/` and the platform default file source.
//!
//! One file-source stack and one cache per process: two views wanting the same tile produce
//! one in-flight fetch with two waiters, and the same holds for glyph and sprite PBFs
//! (§5.1). That coalescing is what the §9.3 flatness counters police — fetches must not
//! scale with view count for overlapping covers.
//!
//! Path shape per §12.6: SQLite WAL + mmap reads so cache hits decode straight from the
//! mapped page; connection reuse and TLS session resumption, since coalescing concentrates
//! traffic onto one or two origins; etag revalidation per TileJSON expiry.
//!
//! # Status
//!
//! URL templating, source resolution, request coalescing and an online file source over plain
//! HTTP. `tools/tile-server` is a local origin these are tested against over a real socket.
//!
//! `ureq` is behind the off-by-default `http` feature and pinned to `default-features = false`
//! plus `gzip`, which is pure Rust throughout — `flate2` on `miniz_oxide`, no `ring`, no C.
//! That is what keeps the cross `cargo check` lane green, which has no C toolchain for the
//! target. TLS is the change that brings C back and is deliberately held until the cross
//! toolchains land (§16); `https://` fails at the transport rather than silently downgrading.
//!
//! Gzip is not optional in practice. Every real vector-tile origin serves
//! `Content-Encoding: gzip` — `pmtiles serve` and every hosted basemap — and without inflation
//! the caller gets `1f 8b 08` and a decoder that correctly refuses it, reporting a protobuf
//! wire-type error several steps from the cause. Decompression belongs to the transport, not
//! the decoder: the encoding is a property of the transfer, and a decoder that sniffed for gzip
//! magic would then have to decide about a tile that is legitimately gzip-inside-protobuf.
//!
//! Sources: vector tiles by inline template or TileJSON, GeoJSON inline or by URL, and
//! `.pmtiles` archives on local storage behind the `pmtiles` feature. Not raster, not
//! raster-dem, not a *remote* archive — that wants §12.6's range requests — and not the
//! `maplibre://` and `mapbox://` scheme aliases mbgl expands through `TileServerOptions`.
//!
//! An SQLite response cache with etag revalidation sits behind the off-by-default `cache`
//! feature, for the same reason: `rusqlite` bundles SQLite's C. Both features are on in the
//! test lane and off in the cross one, which is checked rather than assumed — `cargo tree`
//! shows `libsqlite3-sys` present with the feature and absent without it.
//!
//! Not yet wired: the speculative manifest fetch §12.5 wants ahead of layer compilation, and
//! the tile identity that would keep an access token in a query string from defeating the
//! cache.

#![forbid(unsafe_code)]

#[cfg(feature = "cache")]
pub mod cache;
pub mod canonical;
#[cfg(feature = "cache")]
pub mod download;
pub mod geojson;
#[cfg(feature = "http")]
pub mod http;
pub mod offline;
#[cfg(feature = "pmtiles")]
pub mod pmtiles;
pub mod shared;
pub mod source;
pub mod tileset;
pub mod url;

#[cfg(feature = "cache")]
pub use cache::{
    CacheError, CacheStats, CachingFileSource, Entry, RegionId, RegionProgress, SqliteCache,
    StoredRegion,
};
pub use canonical::{Canonical, CanonicalError, Kind, TileServer, canonicalize, canonicalize_any};
#[cfg(feature = "cache")]
pub use download::{Download, DownloadError, Got, Progress, Summary};
pub use geojson::{GeoJsonSourceError, Origin};
#[cfg(feature = "http")]
pub use http::HttpFileSource;
pub use offline::{Area, AreaError, Estimate, Region, SourceContribution, SourceKind, StyleAssets};
#[cfg(feature = "pmtiles")]
pub use pmtiles::source::PmtilesFileSource;
pub use shared::{Abandoned, ShareStats, Shared};
pub use source::{Coalescing, FetchError, Fetched, FileSource, Response, Router};
pub use tileset::{ResolveError, TileSet, resolve};
pub use url::{Scheme, ZoomRange, expand, fetch_zoom, percent_encode};
