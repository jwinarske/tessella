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
//! Status: scaffold. `rusqlite` and `ureq` are pinned in the workspace but not yet wired —
//! both build C for the target and would break the cross `cargo check` lane until the CI
//! cross toolchains land (§16).

#![forbid(unsafe_code)]
