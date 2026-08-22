//! Style parse, expression evaluation and property types — plan.md §7, descends from mbgl `style/`.
//!
//! Scope: style JSON deserialization, the expression evaluator, property types and
//! transitions. A compiled style is immutable after parse and process-scoped; views hold an
//! `Arc` and repoint on a new revision (§5.1). Mutation is a new revision, never in place.
//!
//! Two design commitments land here:
//!
//! - **DR-11** — expressions are classified at compile time. Constants fold at parse;
//!   camera-only expressions evaluate once per `(layer, integer-zoom interval)` process-wide
//!   and cache as interpolation endpoints; the data-driven residue compiles to flat bytecode
//!   evaluated columnar across a tile's feature array (§12.1). No boxed-AST walk, no JIT.
//! - **§12.9/DR-12** — parse is the `dyn` boundary that stops the serde/expression
//!   monomorphization fan-out.
//!
//! Conformance is the style-spec expression test corpus plus the §9.1 oracle diff (R-3).
//!
//! Status: the style document parses, and expressions parse, classify and evaluate. The
//! bytecode VM (R1), the typed property view, and legacy filter conversion are not
//! implemented.

#![forbid(unsafe_code)]
#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod document;
pub mod expression;
pub mod value;

pub use document::{
    ExpressionValue, GeojsonSource, Layer, LayerKind, PropertyValue, Source, Style, TileSource,
    Transition,
};
pub use expression::{Dependency, Expression};
pub use value::Value;

/// Something went wrong reading a style.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The document is not valid JSON, or does not match the style spec's shape.
    #[error("style is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// The document declares a spec version this frontend does not implement.
    ///
    /// Version 8 is the only one there is. A different number means the document is either
    /// from the future or not a style at all, and guessing at it would turn a clear failure
    /// into a confusing one.
    #[error("unsupported style spec version {0}; only version 8 is implemented")]
    UnsupportedVersion(u32),
}
