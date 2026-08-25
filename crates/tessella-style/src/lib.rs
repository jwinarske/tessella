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
//! Status: the style document parses; expressions parse, classify and evaluate; filters compile
//! in both syntaxes; and paint and layout properties resolve for background, fill, line and
//! circle with their binding decided. DR-11's bytecode VM was built and measured slower than the
//! walk it replaced — §12.1 records why, and what has to come first. Symbol layers are R2.

#![forbid(unsafe_code)]
#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod document;
pub mod expression;
pub mod filter;
pub mod generated;
pub mod light;
pub mod property;
pub mod value;

pub use document::{
    ExpressionValue, GeojsonSource, Layer, LayerKind, PropertyValue, Source, Style, TileSource,
    Transition,
};
pub use expression::{Dependency, Expression};
pub use filter::{Filter, FilterError};
pub use property::{Binding, Color, PropertyError, PropertySpec, ResolvedProperty};
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

/// Whether `name` heads an expression call.
///
/// The style spec's rule for telling a call from a literal array is a registry lookup — the
/// spec spells it `expression[0] in expressions` — and this is that registry. It matters most
/// for `text-font`, whose value is an `array<string>`: without the lookup the ordinary
/// `["Noto Sans Regular"]` is indistinguishable from a call to an operator of that name.
///
/// The names come from mbgl (DR-6), so a version that gains an operator gains it here too
/// rather than diverging silently.
#[must_use]
pub fn is_operator(name: &str) -> bool {
    generated::operators::OPERATORS.binary_search(&name).is_ok()
}
