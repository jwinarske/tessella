// SPDX-License-Identifier: BSD-2-Clause
//! Douglas-Peucker importance, as `geojson-vt` computes it.
//!
//! # Annotate once, filter per tile
//!
//! geojson-vt does not drop points when it simplifies. It runs Douglas-Peucker over a feature
//! once and stores each point's *importance* -- the squared perpendicular distance from the
//! segment spanning its recursion window -- and cutting a tile is where points go:
//!
//! ```cpp
//! if (line.dist > tolerance) {
//!     for (const auto& p : line)
//!         if (p.z > sq_tolerance) result.emplace_back(transform(p));
//! }
//! ```
//!
//! So one annotated feature serves every zoom, and each tile keeps what clears its own threshold.
//! That is why this lives beside [`crate::geojson::read`] and not in the tile builder: the work is
//! once per feature, not once per feature per tile of the cover.
//!
//! # The tolerance
//!
//! `GeoJSONVT` converts at the *finest* tolerance it will ever need, `maxZoom`'s, so the
//! annotation always has resolution to spare for the coarser zooms that filter against it. mbgl
//! sets the inputs in `geojson_source_impl.cpp`:
//!
//! ```cpp
//! constexpr double scale = util::EXTENT / util::tileSize_D;   // 8192 / 512 = 16
//! vtOptions.extent    = util::EXTENT;                          // 8192
//! vtOptions.tolerance = scale * options->tolerance;            // 16 * 0.375 = 6
//! ```
//!
//! and geojson-vt takes `(tolerance / extent) / 2^z` from there, in the projected `0..1` space it
//! works in. That is a constant **six tile units at whatever zoom the tile is**, which was checked
//! against the oracle rather than left as algebra -- a three-point line whose middle point is
//! offset `d` tile units, captured at two zooms:
//!
//! ```text
//! z13   d=4:v4  d=5:v4  d=6:v4  d=7:v6  d=8:v6
//! z15   d=4:v4  d=5:v4  d=6:v4  d=7:v6  d=8:v6
//! ```
//!
//! The threshold sits between six and seven at both, identically, and it is `>` rather than `>=`:
//! a six-unit deviation squares to exactly the tolerance and is dropped.
//!
//! # Why it is worth the pass
//!
//! On a four-hundred-point line of the shape a route or a GPS track has, the oracle spends 44
//! vertices and this build spent 2,316 -- fifty-three times as many, for fourteen differing pixels
//! of 786,432. Gross-pixel parity cannot see that by construction, since the tolerance is chosen
//! so the picture does not change (tessella#274).

use alloc::vec::Vec;

use crate::geojson::Position;

/// mbgl's `util::EXTENT`.
const EXTENT: f64 = 8192.0;
/// `util::EXTENT / util::tileSize_D`, which scales a source's tolerance into extent units.
const SCALE: f64 = EXTENT / 512.0;
/// The style spec's default for a GeoJSON source's `tolerance`.
pub const DEFAULT_TOLERANCE: f64 = 0.375;
/// The style spec's default for a GeoJSON source's `maxzoom`, which the annotation is cut at.
pub const DEFAULT_MAX_ZOOM: u8 = 18;

/// One line's or ring's simplification data, in the projected `0..1` space.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Simplification {
    /// Per point, the squared distance that decided it. Endpoints carry one, which clears every
    /// threshold: a tolerance at z13 is about 8e-15 there, so `1.0` beats it by fourteen orders
    /// of magnitude and an endpoint is never dropped.
    pub importance: Vec<f64>,
    /// A line's projected length, or a ring's absolute projected area. mbgl drops a whole line
    /// shorter than the tolerance and a whole ring smaller than its square, which is how a dense
    /// extract loses small features rather than only losing points.
    pub extent: f64,
}

/// The tolerance a tile at `zoom` filters with, in the projected `0..1` space.
///
/// `source_tolerance` is the source's own, which the style spec defaults to
/// [`DEFAULT_TOLERANCE`].
#[must_use]
pub fn tolerance_at(zoom: u8, source_tolerance: f64) -> f64 {
    #[allow(clippy::cast_possible_truncation)]
    let z2 = f64::from(1u32 << zoom.min(30));
    ((SCALE * source_tolerance) / EXTENT) / z2
}

/// Projects into the `0..1` space geojson-vt measures in.
///
/// `tessella_tile::projection::project` at a world size of one, which was checked against
/// geojson-vt's own formula and agrees to a part in 1e16.
fn projected(point: Position) -> (f64, f64) {
    let [x, y] = tessella_tile::projection::project(point[0], point[1], 1.0);
    (x, y)
}

/// The squared distance from `p` to the segment `a`-`b`. mbgl's `getSqSegDist`.
fn sq_seg_dist(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (mut x, mut y) = a;
    let (dx, dy) = (b.0 - x, b.1 - y);

    if dx != 0.0 || dy != 0.0 {
        let t = ((p.0 - x) * dx + (p.1 - y) * dy) / dx.mul_add(dx, dy * dy);
        if t > 1.0 {
            x = b.0;
            y = b.1;
        } else if t > 0.0 {
            x += dx * t;
            y += dy * t;
        }
    }

    let (dx, dy) = (p.0 - x, p.1 - y);
    dx.mul_add(dx, dy * dy)
}

/// The recursion, ported including its tiebreak.
///
/// When two points tie for furthest, geojson-vt takes the one nearest the middle of the window --
/// a guard against deep recursion on degenerate input, from mapbox/geojson-vt#104. It changes
/// *which* point is annotated, so it is not an optimization to leave out.
fn walk(points: &[(f64, f64)], first: usize, last: usize, sq_tolerance: f64, out: &mut [f64]) {
    let mut max_sq_dist = sq_tolerance;
    let mut index = 0;
    let mid = (last - first) >> 1;
    let mut min_pos_to_mid = last - first;

    for i in (first + 1)..last {
        let sq_dist = sq_seg_dist(points[i], points[first], points[last]);
        if sq_dist > max_sq_dist {
            index = i;
            max_sq_dist = sq_dist;
        } else if sq_dist == max_sq_dist {
            let pos_to_mid = i.abs_diff(first + mid);
            if pos_to_mid < min_pos_to_mid {
                index = i;
                min_pos_to_mid = pos_to_mid;
            }
        }
    }

    if max_sq_dist > sq_tolerance {
        out[index] = max_sq_dist;
        if index - first > 1 {
            walk(points, first, index, sq_tolerance, out);
        }
        if last - index > 1 {
            walk(points, index, last, sq_tolerance, out);
        }
    }
}

/// Annotates `points`, with `metric` computed by the caller for its geometry kind.
fn annotate(points: &[Position], sq_tolerance: f64, metric: f64) -> Simplification {
    let len = points.len();
    let mut importance = alloc::vec![0.0; len];
    if len == 0 {
        return Simplification {
            importance,
            extent: metric,
        };
    }
    // Always retain the endpoints, which is what the one is for.
    importance[0] = 1.0;
    importance[len - 1] = 1.0;
    if len > 2 {
        let projected: Vec<(f64, f64)> = points.iter().map(|point| projected(*point)).collect();
        walk(&projected, 0, len - 1, sq_tolerance, &mut importance);
    }
    Simplification {
        importance,
        extent: metric,
    }
}

/// A line's importance and its projected length.
#[must_use]
pub fn line(points: &[Position], max_zoom: u8, source_tolerance: f64) -> Simplification {
    let tolerance = tolerance_at(max_zoom, source_tolerance);
    let projected: Vec<(f64, f64)> = points.iter().map(|point| projected(*point)).collect();
    let dist = projected
        .windows(2)
        .map(|pair| (pair[1].0 - pair[0].0).hypot(pair[1].1 - pair[0].1))
        .sum();
    annotate(points, tolerance * tolerance, dist)
}

/// A ring's importance and its absolute projected area.
///
/// The area is the shoelace over the ring as it arrived, halved and unsigned, and it is taken
/// *before* the annotation as mbgl takes it.
#[must_use]
pub fn ring(points: &[Position], max_zoom: u8, source_tolerance: f64) -> Simplification {
    let tolerance = tolerance_at(max_zoom, source_tolerance);
    let projected: Vec<(f64, f64)> = points.iter().map(|point| projected(*point)).collect();
    let area: f64 = projected
        .windows(2)
        .map(|pair| pair[0].0.mul_add(pair[1].1, -(pair[1].0 * pair[0].1)))
        .sum();
    annotate(points, tolerance * tolerance, (area / 2.0).abs())
}

/// Whether a point survives a tile whose threshold is `sq_tolerance`.
#[must_use]
pub fn keeps(importance: f64, sq_tolerance: f64) -> bool {
    importance > sq_tolerance
}
