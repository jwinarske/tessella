// SPDX-License-Identifier: BSD-2-Clause
//! `line-round-limit` is one, and that is mbgl's number rather than the style spec's.
//!
//! The spec documents 1.05. mbgl does not implement it:
//!
//! ```text
//! struct LineRoundLimit : LayoutProperty<float> {
//!     static constexpr const char *name() { return "line-round-limit"; }
//!     static float defaultValue() { return 1; }
//! };
//! ```
//!
//! Parity here is measured against mbgl, so mbgl's number is the one to carry.
//!
//! # Why the difference is not small
//!
//! A join's miter length is `1 / cos(half the turn)`, so it is never below one and reaches 1.05
//! only once the turn passes about 36 degrees. A limit of one therefore converts almost no round
//! join into a miter, and 1.05 converts every shallow one -- which on a coastline is most of
//! them.
//!
//! Counted on one demotiles ring of 3,147 points, both renderers instrumented at the same place:
//!
//! | | miter | round | fake round |
//! |---|---|---|---|
//! | mbgl | 83 | 135 | 2,929 |
//! | 1.05 | 2,011 | 135 | 1,001 |
//! | 1.0 | 14 | 135 | 2,998 |
//!
//! The arcs mbgl draws where this drew corners are the whole of a 523-pixel gap at
//! `set-pitch-and-bearing`, which went to zero for the coastline layer alone.

use tessella_layout::line::{LineBucket, LineCap, LineJoin, LineOptions};

/// Two segments meeting at a 20-degree turn -- shallow enough for 1.05 to have flattened it.
///
/// The miter length there is `1 / cos(10 deg)` = 1.0154: above one, below 1.05.
fn shallow_corner() -> [[i16; 2]; 3] {
    // A horizontal run, then one turning 20 degrees off it.
    let length = 2000.0_f64;
    let turn = 20.0_f64.to_radians();
    #[allow(clippy::cast_possible_truncation)]
    let end = [
        (length + length * turn.cos()) as i16,
        (length * turn.sin()) as i16,
    ];
    [[0, 0], [length as i16, 0], end]
}

fn joins(round_limit: f32) -> usize {
    let mut bucket = LineBucket::default();
    let options = LineOptions {
        join: LineJoin::Round,
        begin_cap: LineCap::Butt,
        end_cap: LineCap::Butt,
        round_limit,
        ..LineOptions::default()
    };
    bucket.add_geometry(&shallow_corner(), &options);
    bucket.vertices.len()
}

/// The default is mbgl's one, and the shallow corner is drawn as an arc because of it.
#[test]
fn the_default_round_limit_is_mbgls_one() {
    assert!(
        (LineOptions::default().round_limit - 1.0).abs() < f32::EPSILON,
        "the default is mbgl's 1, not the spec's 1.05: {}",
        LineOptions::default().round_limit
    );
}

/// A limit of one leaves a shallow corner rounded; 1.05 flattens it to a miter.
///
/// Counted in vertices rather than by inspecting the join kind, because the kind is internal and
/// the vertices are what reaches the screen: an arc spends a fan of them where a miter spends
/// two.
#[test]
fn a_shallow_corner_stays_round_at_mbgls_limit() {
    let rounded = joins(1.0);
    let flattened = joins(1.05);
    assert!(
        rounded > flattened,
        "at mbgl's limit the corner should keep its arc: {rounded} vertices against {flattened} \
         at the spec's 1.05"
    );
}
