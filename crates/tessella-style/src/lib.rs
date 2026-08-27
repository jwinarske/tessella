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

pub mod config;
pub mod crossfade;
pub mod document;
pub mod expression;
pub mod filter;
pub mod generated;
pub mod light;
pub mod property;
pub mod value;

pub use document::{
    ExpressionValue, GeojsonSource, Layer, LayerKind, PropertyValue, RejectedLayer, Source, Style,
    TileSource, Transition,
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
/// rather than diverging silently — plus [`EXTENSIONS`], for the ones this build has and mbgl
/// does not.
#[must_use]
pub fn is_operator(name: &str) -> bool {
    generated::operators::OPERATORS.binary_search(&name).is_ok()
        || EXTENSIONS.binary_search(&name).is_ok()
}

/// Operators this build implements that maplibre-native does not.
///
/// # Why these are not in the generated table
///
/// `generated::operators` says of itself that a diff of it is a diff of what mbgl supports, and
/// DR-6 rests on that being exactly true: the table is regenerated from `expressionRegistry` and
/// `compoundExpressionRegistry`, and anything hand-added would survive one regeneration and
/// vanish at the next. Keeping the two apart means the generated file stays a faithful statement
/// about mbgl, and this one is the statement about the difference.
///
/// Both are Mapbox Style Spec v3 additions. mbgl has no compound expression for either; it went
/// as far as reserving `Dependency::Location = 1 << 3` for `distance-from-center` and commenting
/// it "not used yet". They appear in filters on label layers in vendor styles, where their
/// absence costs the layer rather than the property.
///
/// # Two of these the parser handles, and one the document does
///
/// `pitch` and `distance-from-center` become expression nodes and are evaluated. `config` never
/// reaches the evaluator: [`Style::resolve_config`](document::Style::resolve_config) replaces
/// every call with the value it resolves to, because a config value is fixed for a style load
/// and belongs in the document rather than in the evaluator.
///
/// It is listed here all the same, and has to be. `is_operator` is what tells a call from a
/// literal array, so without the entry `["config", "language"]` in a `text-field` would
/// deserialize as an array of two strings — a font stack, as far as the property is concerned —
/// and the substitution would never see it.
///
/// Sorted, for the same binary search as the generated table.
pub const EXTENSIONS: [&str; 3] = ["config", "distance-from-center", "pitch"];
