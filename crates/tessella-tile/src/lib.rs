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
//! Status: the Mercator projection is in, spelled to match the oracle bit for bit. The tile
//! store, cover and retain are not.

//! # This crate uses std, deliberately
//!
//! `core` has no `tan`, `ln`, `exp` or `atan`, so a `no_std` version would have to pull in the
//! `libm` crate — a Rust reimplementation of those functions, which is free to round
//! differently in the last bit from the system libm that the C++ oracle links against. Since
//! the projection's whole contract is bit-exact agreement with that oracle, borrowing the same
//! libm is not an incidental convenience but the mechanism. `tessella-capture-abi` stays
//! `no_std` because a flat ABI genuinely has no need of one; this crate does.
//!
//! The corollary is worth stating plainly: bit-exactness holds where both sides use the same
//! libm. Two platforms with different libm implementations may disagree in the last bit, and
//! the cross-target lanes only compile this crate rather than running it.

#![forbid(unsafe_code)]

pub mod projection;
