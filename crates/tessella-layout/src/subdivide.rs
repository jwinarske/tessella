// SPDX-License-Identifier: Apache-2.0

//! Splitting flat tile geometry onto a grid, so it can be bent — plan.md §13.4's consumer half.
//!
//! # Why geometry has to change for a globe when placement does not
//!
//! §13.4 settles that the producer emits ordinary Mercator placement and the consumer's material
//! bends it per vertex. That is true of *where* a vertex goes and says nothing about *how many*
//! there are. A bend moves vertices and interpolates between them, so a triangle spanning a
//! quarter of the world bends into a flat sheet cutting through the sphere: the three corners land
//! on the surface and everything between them chords across the inside of the planet. Fills are
//! earcut output, whose triangles are as large as the polygon allows -- one triangle covers the
//! Pacific in a low-zoom water layer.
//!
//! So the geometry is split against a grid before it is handed over. [`globe::edge_segments`]
//! already answers how fine the grid must be, derived from a chord bound rather than tabulated;
//! this is what applies that answer.
//!
//! [`globe::edge_segments`]: tessella_tile::globe::edge_segments
//!
//! # What is checkable here, with no oracle
//!
//! MapLibre Native has no globe, so nothing on this page can be rendered against — §13.4 says so.
//! What a subdivider can be held to instead is structural, and it is stronger than it sounds:
//!
//! - **Area is preserved.** Splitting a triangle neither creates nor destroys surface.
//! - **No T-junctions.** A vertex sitting part-way along a neighbor's edge is a crack in the
//!   planet the moment the two are bent, because the neighbor's edge stays a straight chord while
//!   this one follows the sphere. This is why the cut is by *global* grid lines rather than by
//!   subdividing each triangle on its own: a cut point depends only on the edge and the line, so
//!   two triangles sharing an edge cut it in the same places without having to be told they are
//!   neighbors.
//! - **Every output triangle lies in one cell**, which is what bounds the chord error to the one
//!   [`globe::edge_segments`] solved for.
//!
//! # Rounding
//!
//! Clipping happens in `f64` and rounds once, at the end, back to the `i16` the vertex buffer
//! carries. A cut lands on a grid line at a fractional coordinate and there is nowhere to put it,
//! so the error is up to half a tile unit per cut vertex. That is below the half-pixel tolerance
//! the segment count is solved for at any zoom where a tile unit is smaller than a pixel, and it
//! is the reason area is preserved to a bound rather than exactly.
//!
//! Rounding does not cost the T-junction property: the same edge and the same grid line give the
//! same intersection whichever triangle asks, so both round to the same integer.

use alloc::vec::Vec;

use crate::fill::{Position, Ring};

/// A point mid-clip, before it is rounded back to the vertex buffer's integers.
type Point = [f64; 2];

/// The finest grid this will build, as cells along one tile edge.
///
/// Mirrors `globe::MAX_EDGE_SEGMENTS` and exists for the same reason: the cell count squares into
/// a vertex count, and the arithmetic that chooses it is monotone in the sphere's radius with
/// nothing stopping it asking for thousands. A caller passing a step derived from that ceiling
/// cannot exceed this one.
pub const MAX_CELLS: i32 = 128;

/// The grid step, in tile units, that divides `extent` into at most `segments` cells.
///
/// Rounds the step *up*, so the cell count comes out at or below what was asked for rather than
/// above it: a step that divides unevenly leaves a narrow last cell, which is harmless, where one
/// rounded down would add a whole extra column across every tile.
///
/// Returns `extent` — one cell, no subdivision — for a degenerate extent or a zero segment count,
/// which is the identity this whole module degrades to on a flat map.
#[must_use]
pub fn grid_step(extent: i32, segments: u32) -> i32 {
    if extent <= 0 {
        return 1;
    }
    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    let segments = (segments.min(MAX_CELLS as u32)) as i32;
    if segments <= 1 {
        return extent;
    }
    // Ceiling division: `segments` cells of this width cover `extent`.
    (extent + segments - 1) / segments
}

/// Splits `triangles` so that no triangle spans more than one cell of a `step`-unit grid.
///
/// The grid is anchored at the tile origin and runs through negative coordinates the same way,
/// which matters because a tile's geometry is buffered past its own edge and those vertices are
/// real. Triangles already inside one cell are passed through untouched, so a fine tile at a high
/// zoom costs a bounding-box test and nothing else.
///
/// A `step` of zero or less is the identity — there is no grid to cut against — as is a step at or
/// above the geometry's own size.
#[must_use]
pub fn subdivide_triangles(triangles: &[[Position; 3]], step: i32) -> Vec<[Position; 3]> {
    if step <= 0 {
        return triangles.to_vec();
    }
    let step = f64::from(step);
    let mut out = Vec::with_capacity(triangles.len());
    let mut scratch = Convex::new();
    let mut clipped = Convex::new();
    for triangle in triangles {
        if spans_one_cell(triangle, step) {
            out.push(*triangle);
            continue;
        }
        let (min_x, max_x, min_y, max_y) = bounds(triangle);
        // Cell by cell over the bounding box, clipping the whole triangle to each rather than
        // cutting once per axis and keeping the pieces. Same result, and nothing allocates: a
        // triangle clipped to a rectangle has at most seven vertices, so both buffers are fixed.
        for cell_y in floor_div(min_y, step)..=floor_div(next_below(max_y), step) {
            for cell_x in floor_div(min_x, step)..=floor_div(next_below(max_x), step) {
                scratch.clear();
                for point in triangle {
                    scratch.push([f64::from(point[0]), f64::from(point[1])]);
                }
                #[allow(clippy::cast_precision_loss)]
                let (left, bottom) = (cell_x as f64 * step, cell_y as f64 * step);
                for (axis, at, keep_greater) in [
                    (0, left, true),
                    (0, left + step, false),
                    (1, bottom, true),
                    (1, bottom + step, false),
                ] {
                    clip(&scratch, axis, at, keep_greater, &mut clipped);
                    core::mem::swap(&mut scratch, &mut clipped);
                    if scratch.len < 3 {
                        break;
                    }
                }
                fan(&scratch, &mut out);
            }
        }
    }
    out
}

/// A convex polygon mid-clip, on the stack.
///
/// A triangle clipped by an axis-aligned rectangle gains at most one vertex per side and so has
/// seven at most; eight is that with room to write before the length is checked. Fixed rather than
/// a `Vec` because this is the inner loop of a per-tile, per-layer pass and a low-zoom fill runs it
/// tens of thousands of times, each of which would otherwise be two allocations.
struct Convex {
    points: [Point; 8],
    len: usize,
}

impl Convex {
    fn new() -> Self {
        Self {
            points: [[0.0; 2]; 8],
            len: 0,
        }
    }

    fn clear(&mut self) {
        self.len = 0;
    }

    fn push(&mut self, point: Point) {
        // Silently dropped past the bound rather than panicking: the geometry says seven is the
        // most this can reach, and a dropped vertex is a wrong triangle where a panic in a bucket
        // build is a lost tile.
        if self.len < self.points.len() {
            self.points[self.len] = point;
            self.len += 1;
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn as_slice(&self) -> &[Point] {
        &self.points[..self.len]
    }
}

/// A triangle's bounding box.
fn bounds(triangle: &[Position; 3]) -> (f64, f64, f64, f64) {
    let (mut min_x, mut max_x) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut min_y, mut max_y) = (f64::INFINITY, f64::NEG_INFINITY);
    for point in triangle {
        let (x, y) = (f64::from(point[0]), f64::from(point[1]));
        min_x = min_x.min(x);
        max_x = max_x.max(x);
        min_y = min_y.min(y);
        max_y = max_y.max(y);
    }
    (min_x, max_x, min_y, max_y)
}

/// Inserts a vertex into `ring` wherever one of its edges crosses a grid line.
///
/// A fill draws its outline over the same vertices as its triangles, and an outline is a line loop
/// along the ring rather than anything earcut produced — so it needs the cuts made here, and it
/// needs them in the same places [`subdivide_triangles`] makes them or the outline lifts off the
/// fill once both are bent. Both derive a cut from the edge and the line alone, which is what
/// makes them agree without sharing state.
///
/// The ring's own vertices are all kept, in order, including a repeated closing point: the fill
/// builder counts on that repeat and `outline_indices` emits a segment for it.
#[must_use]
pub fn subdivide_ring(ring: &[Position], step: i32) -> Ring {
    if step <= 0 || ring.len() < 2 {
        return ring.to_vec();
    }
    let step = f64::from(step);
    let mut out: Ring = Vec::with_capacity(ring.len());
    for window in ring.windows(2) {
        let (from, to) = (window[0], window[1]);
        out.push(from);
        let a = [f64::from(from[0]), f64::from(from[1])];
        let b = [f64::from(to[0]), f64::from(to[1])];
        for t in crossings(a, b, step) {
            let point = [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
            let rounded = round(point);
            // A crossing that rounds onto a vertex the ring already has is not a new vertex. Two
            // grid lines meeting at a corner produce the same point twice for the same reason.
            if rounded != *out.last().unwrap_or(&from) {
                out.push(rounded);
            }
        }
    }
    if let Some(last) = ring.last()
        && out.last() != Some(last)
    {
        out.push(*last);
    }
    out
}

/// Whether a triangle is small enough to pass through untouched.
fn spans_one_cell(triangle: &[Position; 3], step: f64) -> bool {
    let cell = |v: i16, axis_min: &mut f64, axis_max: &mut f64| {
        let value = f64::from(v);
        *axis_min = axis_min.min(value);
        *axis_max = axis_max.max(value);
    };
    let (mut min_x, mut max_x) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut min_y, mut max_y) = (f64::INFINITY, f64::NEG_INFINITY);
    for point in triangle {
        cell(point[0], &mut min_x, &mut max_x);
        cell(point[1], &mut min_y, &mut max_y);
    }
    // The half-open cell `[k*step, (k+1)*step)` is what the clipper produces, so a triangle whose
    // maximum sits exactly on a line belongs to the cell below it.
    floor_div(min_x, step) == floor_div(next_below(max_x), step)
        && floor_div(min_y, step) == floor_div(next_below(max_y), step)
}

/// The largest value strictly below `v`, for the half-open cell test.
///
/// A whole tile unit rather than an ulp: coordinates here are integers on entry, so anything
/// smaller than one unit cannot change which cell a maximum falls in, and a full unit keeps the
/// arithmetic exact.
fn next_below(v: f64) -> f64 {
    v - 1.0
}

/// Which cell along one axis a coordinate falls in.
fn floor_div(value: f64, step: f64) -> i64 {
    #[allow(clippy::cast_possible_truncation)]
    let cell = libm::floor(value / step) as i64;
    cell
}

/// Sutherland-Hodgman against one axis-aligned half-plane, into `out`.
///
/// `keep_greater` selects the side: `true` keeps `coordinate >= at`, `false` keeps `<= at`. The
/// input is convex -- a triangle, or something a previous cut made from one -- so the output is a
/// single convex polygon and there is no case to split.
fn clip(polygon: &Convex, axis: usize, at: f64, keep_greater: bool, out: &mut Convex) {
    out.clear();
    let points = polygon.as_slice();
    let inside = |p: &Point| {
        if keep_greater {
            p[axis] >= at
        } else {
            p[axis] <= at
        }
    };
    for index in 0..points.len() {
        let current = points[index];
        let previous = points[(index + points.len() - 1) % points.len()];
        let (current_in, previous_in) = (inside(&current), inside(&previous));
        if current_in != previous_in {
            let span = current[axis] - previous[axis];
            // The two straddle the line, so the span cannot be zero; guarded anyway because a
            // NaN here would spread through a whole tile rather than fail where it was made.
            if span.abs() > f64::EPSILON {
                let t = (at - previous[axis]) / span;
                let mut point = [
                    previous[0] + (current[0] - previous[0]) * t,
                    previous[1] + (current[1] - previous[1]) * t,
                ];
                // Pinned exactly onto the line rather than left to the division's rounding, so
                // the two cells either side of it agree about where their shared edge is.
                point[axis] = at;
                out.push(point);
            }
        }
        if current_in {
            out.push(current);
        }
    }
}

/// Fan-triangulates a convex piece into `out`, rounding to the vertex buffer's integers.
///
/// Degenerate triangles are dropped rather than emitted: a cut that grazes a corner produces
/// slivers thinner than the grid, and rounding collapses them onto a line. They would draw
/// nothing and still cost a vertex each.
fn fan(piece: &Convex, out: &mut Vec<[Position; 3]>) {
    if piece.len() < 3 {
        return;
    }
    let mut rounded = [[0i16; 2]; 8];
    for (slot, point) in rounded.iter_mut().zip(piece.as_slice()) {
        *slot = round(*point);
    }
    let rounded = &rounded[..piece.len()];
    for index in 1..rounded.len() - 1 {
        let triangle = [rounded[0], rounded[index], rounded[index + 1]];
        if !is_degenerate(&triangle) {
            out.push(triangle);
        }
    }
}

/// Whether a triangle has no area once its vertices are integers.
fn is_degenerate(triangle: &[Position; 3]) -> bool {
    let [a, b, c] = triangle;
    let abx = i64::from(b[0]) - i64::from(a[0]);
    let aby = i64::from(b[1]) - i64::from(a[1]);
    let acx = i64::from(c[0]) - i64::from(a[0]);
    let acy = i64::from(c[1]) - i64::from(a[1]);
    abx * acy - aby * acx == 0
}

/// One point back to the integers the vertex buffer carries.
///
/// Saturating rather than wrapping: a coordinate past `i16` is a tile buffered further than the
/// format can express, and a clamp puts it on the edge of what is drawable where a wrap would put
/// it on the opposite side of the tile.
fn round(point: Point) -> Position {
    let clamp = |v: f64| {
        let r = libm::round(v);
        #[allow(clippy::cast_possible_truncation)]
        let clamped = r.clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16;
        clamped
    };
    [clamp(point[0]), clamp(point[1])]
}

/// Where a segment crosses grid lines, as parameters in `(0, 1)`, sorted and deduplicated.
fn crossings(a: Point, b: Point, step: f64) -> Vec<f64> {
    let mut out: Vec<f64> = Vec::new();
    for axis in 0..2 {
        let (from, to) = (a[axis], b[axis]);
        if (to - from).abs() <= f64::EPSILON {
            continue;
        }
        let low = from.min(to);
        let high = from.max(to);
        let first = floor_div(low, step) + 1;
        let last = floor_div(next_below(high), step);
        for cell in first..=last {
            #[allow(clippy::cast_precision_loss)]
            let line = cell as f64 * step;
            let t = (line - from) / (to - from);
            if t > 0.0 && t < 1.0 {
                out.push(t);
            }
        }
    }
    out.sort_by(|x, y| x.partial_cmp(y).unwrap_or(core::cmp::Ordering::Equal));
    out.dedup();
    out
}
