//! Tile pyramid, cover/retain, and the process-scoped shared tile store — plan.md §7,
//! descends from mbgl `tile/` and `algorithm/`.
//!
//! The store is keyed `(source, OverscaledTileID, styleRev)` with refcounted retain; a
//! view's cover is a set of handles and the LRU is sized once per process (§5.1). Retain
//! chains unify across views: adjacent-zoom views over one area are one pyramid, so a z12
//! view's active tiles are a z13 view's retained ancestors (§5.5, §13.2). Cover and retain
//! recompute only on crossing a tile boundary or integer-zoom threshold — between crossings
//! cover is provably unchanged (§12.7).
//!
//! Cover decisions are per view; the store beneath them is not (§5.5). Watch R-11: unified
//! retain lets one view's zoom behavior inflate another's tile lifetimes.
//!
//! Status: scaffold. No implementation yet.

#![forbid(unsafe_code)]
