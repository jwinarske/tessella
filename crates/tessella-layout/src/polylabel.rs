//! The pole of inaccessibility: where a point-placed label sits on an area.
//!
//! A polygon's label does not go at its centroid. The centroid of a crescent is outside the
//! crescent, and the centroid of an L is in the notch, so a label placed there sits off its own
//! shape. What mbgl places at is the *pole of inaccessibility* — the center of the largest circle
//! that fits inside the polygon — through `mapbox::polylabel`, called with "1 pixel worth of
//! precision, in tile coordinates".
//!
//! # Why the queue is libstdc++'s and not a `BinaryHeap`
//!
//! polylabel is a best-first search: a quadtree of cells in a priority queue ordered by the
//! largest distance the cell could still hold, each cell split into four until no unexplored cell
//! could beat the best found by more than `precision`. The search stops *short* of the exact pole
//! by design, so where two cells are within `precision` of each other the answer is whichever the
//! queue hands over first — and that is the library's business rather than the algorithm's.
//! `std::priority_queue` is `std::push_heap` and `std::pop_heap`, and the oracle is built against
//! libstdc++, so the sift is transcribed too.
//!
//! Measured rather than assumed: of four hundred random seven-to-nine-sided polygons, twenty-one
//! pole somewhere else when the same search runs its queue in a different order, by up to 37 tile
//! units — 2.3 pixels, which is a label visibly off its shape. One of those polygons is the star
//! in `tools/parity/scenes/arealabel_p.json`, and answering it from a sorted queue instead of a
//! sifted one moves that scene from 0 gross pixels to 242.

use crate::libstdcxx::{pop_heap, push_heap};
use alloc::vec::Vec;

/// A point in tile units.
pub type Point = [f64; 2];

/// One cell of the search, with the bound that orders it.
#[derive(Debug, Clone, Copy)]
struct Cell {
    /// Its center.
    c: Point,
    /// Half its side.
    h: f64,
    /// Signed distance from the center to the outline: negative outside.
    d: f64,
    /// The largest `d` anywhere in the cell, which is `d` plus the half-diagonal.
    max: f64,
}

impl Cell {
    fn new(c: Point, h: f64, polygon: &[&[Point]]) -> Self {
        let d = point_to_polygon_dist(c, polygon);
        Self {
            c,
            h,
            d,
            max: d + h * core::f64::consts::SQRT_2,
        }
    }
}

/// Squared distance from a point to a segment. `getSegDistSq`.
fn seg_dist_sq(p: Point, a: Point, b: Point) -> f64 {
    let (mut x, mut y) = (a[0], a[1]);
    let mut dx = b[0] - x;
    let mut dy = b[1] - y;

    if dx != 0.0 || dy != 0.0 {
        let t = ((p[0] - x) * dx + (p[1] - y) * dy) / (dx * dx + dy * dy);
        if t > 1.0 {
            x = b[0];
            y = b[1];
        } else if t > 0.0 {
            x += dx * t;
            y += dy * t;
        }
    }

    dx = p[0] - x;
    dy = p[1] - y;
    dx * dx + dy * dy
}

/// Signed distance from a point to the polygon's outline, negative outside it.
///
/// One walk does both halves: the crossing parity says inside or out, and the running minimum says
/// how far. Every ring takes part, so a hole pushes the pole out of itself.
fn point_to_polygon_dist(point: Point, polygon: &[&[Point]]) -> f64 {
    let mut inside = false;
    let mut min_dist_sq = f64::INFINITY;

    for ring in polygon {
        let len = ring.len();
        if len == 0 {
            continue;
        }
        let mut j = len - 1;
        for i in 0..len {
            let a = ring[i];
            let b = ring[j];
            if (a[1] > point[1]) != (b[1] > point[1])
                && (point[0] < (b[0] - a[0]) * (point[1] - a[1]) / (b[1] - a[1]) + a[0])
            {
                inside = !inside;
            }
            min_dist_sq = min_dist_sq.min(seg_dist_sq(point, a, b));
            j = i;
        }
    }

    (if inside { 1.0 } else { -1.0 }) * libm::sqrt(min_dist_sq)
}

/// The centroid of the outer ring, as the first guess. `getCentroidCell`.
fn centroid_cell(polygon: &[&[Point]]) -> Cell {
    let mut area = 0.0;
    let mut c = [0.0, 0.0];
    let ring = polygon[0];

    let len = ring.len();
    let mut j = len - 1;
    for i in 0..len {
        let a = ring[i];
        let b = ring[j];
        let f = a[0] * b[1] - b[0] * a[1];
        c[0] += (a[0] + b[0]) * f;
        c[1] += (a[1] + b[1]) * f;
        area += f * 3.0;
        j = i;
    }

    let centroid = if area == 0.0 {
        ring[0]
    } else {
        [c[0] / area, c[1] / area]
    };
    Cell::new(centroid, 0.0, polygon)
}

/// Where to put a label on an area, to within `precision`.
///
/// `polygon` is an exterior ring followed by its holes, in tile units. `None` where there is no
/// ring to measure, which mbgl does not have to answer because its caller cannot produce one:
/// `classifyRings` emits no empty polygon and `polygon.at(0)` would throw on it.
#[must_use]
pub fn polylabel(polygon: &[&[Point]], precision: f64) -> Option<Point> {
    let outer = polygon.first()?;
    if outer.is_empty() {
        return None;
    }

    // The bounding box of the outer ring alone, which is `geometry::envelope(polygon.at(0))`.
    let mut min = *outer.first()?;
    let mut max = min;
    for point in *outer {
        min = [min[0].min(point[0]), min[1].min(point[1])];
        max = [max[0].max(point[0]), max[1].max(point[1])];
    }

    let size = [max[0] - min[0], max[1] - min[1]];
    let cell_size = size[0].min(size[1]);
    if cell_size == 0.0 {
        return Some(min);
    }
    let h = cell_size / 2.0;

    // Ordered by the bound, so the top is the cell that could still hold the best pole.
    let less = |a: &Cell, b: &Cell| a.max < b.max;
    let mut queue: Vec<Cell> = Vec::new();

    // One row at a time, accumulating as the loop's `x += cellSize` does: a stride summed is not
    // the same number as a stride multiplied, and the cell centers are what the search probes.
    let mut x = min[0];
    while x < max[0] {
        let mut y = min[1];
        while y < max[1] {
            queue.push(Cell::new([x + h, y + h], h, polygon));
            push_heap(&mut queue, &less);
            y += cell_size;
        }
        x += cell_size;
    }

    let mut best = centroid_cell(polygon);

    // The special case for a rectangle, whose centroid is already its pole.
    let bbox = Cell::new(
        [min[0] + size[0] / 2.0, min[1] + size[1] / 2.0],
        0.0,
        polygon,
    );
    if bbox.d > best.d {
        best = bbox;
    }

    while !queue.is_empty() {
        pop_heap(&mut queue, &less);
        let cell = queue.pop()?;

        if cell.d > best.d {
            best = cell;
        }

        // Nothing in this cell can beat the best by enough to matter, so it is not split.
        if cell.max - best.d <= precision {
            continue;
        }

        let h = cell.h / 2.0;
        for center in [
            [cell.c[0] - h, cell.c[1] - h],
            [cell.c[0] + h, cell.c[1] - h],
            [cell.c[0] - h, cell.c[1] + h],
            [cell.c[0] + h, cell.c[1] + h],
        ] {
            queue.push(Cell::new(center, h, polygon));
            push_heap(&mut queue, &less);
        }
    }

    Some(best.c)
}
