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
//! `ureq` is behind the off-by-default `http` feature and pinned to `default-features = false`,
//! which is pure Rust — no TLS, and so no `ring` and no C. That is what keeps the cross
//! `cargo check` lane green, which has no C toolchain for the target. TLS is the change that
//! brings C back and is deliberately held until the cross toolchains land (§16); `https://`
//! fails at the transport rather than silently downgrading.
//!
//! Not yet wired: the SQLite cache (`rusqlite`, also C), etag revalidation, and the
//! speculative manifest fetch §12.5 wants ahead of layer compilation.

#![forbid(unsafe_code)]

#[cfg(feature = "http")]
pub mod http;
pub mod source;
pub mod tileset;
pub mod url;

#[cfg(feature = "http")]
pub use http::HttpFileSource;
pub use source::{Coalescing, FetchError, Fetched, FileSource, Response, Stats};
pub use tileset::{ResolveError, TileSet, resolve};
pub use url::{Scheme, ZoomRange, expand, fetch_zoom};
