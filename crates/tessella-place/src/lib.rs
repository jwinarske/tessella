//! Per-view symbol placement — plan.md §7, descends from the placement half of mbgl `text/`.
//!
//! Collision index, cross-tile index, placement and opacity fades. Irreducibly per-view
//! (§5.2, §5.5): every one of these is a function of bearing, pitch and zoom, so two views
//! over identical shared geometry place differently and must. This is the per-view cost
//! center — pace it per view class: tight interval on a primary display, lazy on cluster and
//! inset views.
//!
//! Fades count as churn while fading and then settle to silence, which is what keeps the
//! §6.5 still-frame guarantee honest.
//!
//! Status: the collision grid is in. Placement, the cross-tile index and fades are not.
//! Largest phase in §10 (R2), and R-1 is the risk
//! that it is still underestimated.

#![forbid(unsafe_code)]

pub mod fade;
pub mod feature;
pub mod grid;
