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
//! Status: scaffold. No implementation yet.

#![forbid(unsafe_code)]
