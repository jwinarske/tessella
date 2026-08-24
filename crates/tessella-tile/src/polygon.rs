//! Tile cover over an arbitrary shape — a port of mbgl's `util::TileCover`.
//!
//! # Why a rectangle is not enough
//!
//! [`crate::cover::Bounds`] answers "which tiles does this box touch" with a closed formula,
//! and for a box that is the whole answer. But "pick the area you want offline" is a shape the
//! user drew: a city limit, a coastline, a route corridor. Covering a coastal city by its
//! bounding box downloads the sea, which at street zoom is most of the tiles and all of the
//! waiting.
//!
//! # The algorithm
//!
//! A modified scanline. Each ring is cut at its local y-minima into *bounds* — chains of edges
//! running monotonically downward in tile space — and those are indexed by the tile row they
//! start in. Row by row, every bound that enters the row is scanned for the x range it spans
//! between the row's top and bottom edges, and the resulting spans are merged.
//!
//! For a closed shape the merge uses the non-zero winding rule, which is what fills the
//! interior: a span that opens a winding and a span that closes it bracket tiles that are
//! inside the polygon even though no edge passes through them. That is also why bounds carry
//! their original direction — a hole wound the other way subtracts rather than adds.
//!
//! # Precision
//!
//! Points are projected once, at the target zoom, and everything after that is arithmetic in
//! tile units. Projecting per row would put a `tan`/`ln` on the inner loop and, worse, let a
//! point land in different rows depending on which edge asked.

use std::collections::{BTreeMap, VecDeque};

use crate::cover::TileCoord;
use crate::projection;

/// A ring of longitude/latitude points.
///
/// The first and last point may or may not repeat; both spellings describe the same ring and
/// are treated the same. Winding order is significant only in that a hole must wind opposite to
/// the ring that contains it — the non-zero rule reads direction, not absolute orientation.
pub type Ring = Vec<[f64; 2]>;

/// A shape to cover: an outer ring and any number of holes.
///
/// Holes are not optional decoration. A region drawn around a city with a lake in it should not
/// download the lake, and at street zoom that is a visible fraction of the total.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Polygon {
    /// The outline.
    pub exterior: Ring,
    /// Rings subtracted from it.
    pub interiors: Vec<Ring>,
}

impl Polygon {
    /// A polygon with no holes.
    #[must_use]
    pub fn new(exterior: Ring) -> Self {
        Self {
            exterior,
            interiors: Vec::new(),
        }
    }

    /// Adds a hole.
    #[must_use]
    pub fn with_hole(mut self, ring: Ring) -> Self {
        self.interiors.push(ring);
        self
    }

    /// Every ring, outline first.
    fn rings(&self) -> impl Iterator<Item = &Ring> {
        core::iter::once(&self.exterior).chain(&self.interiors)
    }
}

/// A chain of edges running from a local y-minimum to a local y-maximum, in tile units.
///
/// Cutting rings into monotone chains is what makes a scanline possible: within one bound, the
/// row a point belongs to only ever increases, so the scan never has to look backwards.
#[derive(Debug, Clone)]
struct Bound {
    points: Vec<[f64; 2]>,
    current: usize,
    /// Whether the chain runs the same way the original ring did.
    ///
    /// The non-zero rule counts this, which is how a hole subtracts from the ring around it.
    winding: bool,
}

impl Bound {
    /// The x where this bound's current edge crosses row `y`.
    fn interpolate(&self, y: u32) -> f64 {
        let p0 = self.points[self.current];
        let p1 = self.points[self.current + 1];
        let y = f64::from(y);

        let dx = p1[0] - p0[0];
        let dy = p1[1] - p0[1];
        if dx == 0.0 {
            return p0[0];
        }
        // A horizontal edge crosses no row; which end applies depends on which side of it the
        // row is.
        if dy == 0.0 {
            return if y <= p0[1] { p0[0] } else { p1[0] };
        }
        if y < p0[1] {
            return p0[0];
        }
        if y > p1[1] {
            return p1[0];
        }
        (dx / dy) * (y - p0[1]) + p0[0]
    }
}

/// The x tiles one bound spans across a row, and which way it was wound.
#[derive(Debug, Clone, Copy)]
struct Span {
    min: i64,
    max: i64,
    winding: bool,
}

impl Span {
    fn extend(&mut self, x: f64) {
        #[allow(clippy::cast_possible_truncation)]
        {
            self.min = self.min.min(x.floor() as i64);
            self.max = self.max.max(x.ceil() as i64);
        }
    }
}

/// Rotates a closed ring so it starts at a local y-minimum.
///
/// Without this a ring could start part way down an edge, and the first bound would run the
/// wrong way — producing a chain that is not monotone and a scan that misses tiles.
fn start_on_local_minimum(points: &mut Vec<[f64; 2]>) {
    if points.len() < 3 {
        return;
    }
    // The ring is closed here, so the point before the first is the second-to-last.
    let mut previous = points.len() - 2;
    let mut found = None;
    for index in 0..points.len() {
        let next = if index + 1 == points.len() {
            1
        } else {
            index + 1
        };
        if points[index][1] <= points[previous][1] && points[index][1] < points[next][1] {
            found = Some(index);
            break;
        }
        previous = index;
    }
    let Some(start) = found else {
        // Every point at the same latitude: a degenerate ring with no local minimum, which
        // covers no rows and is left alone rather than rotated arbitrarily.
        return;
    };
    if points.last() == points.first() {
        points.pop();
    }
    points.rotate_left(start);
    points.push(points[0]);
}

/// Takes the chain running downward from `at`, if there is one.
fn bound_towards_maximum(points: &[[f64; 2]], at: &mut usize) -> Option<Bound> {
    if points.len().checked_sub(*at)? < 2 {
        return None;
    }
    let begin = *at;
    let mut next = begin + 1;
    while points[*at][1] <= points[next][1] {
        *at += 1;
        next += 1;
        if next == points.len() {
            *at += 1;
            break;
        }
    }
    if next - begin < 2 {
        return None;
    }
    Some(Bound {
        // Exclusive of `next`, which is where the chain stops rather than a point on it. An
        // inclusive slice makes every bound one point too long: the extra edge belongs to the
        // chain running the other way, so it gets scanned twice and with the wrong winding —
        // which shows up as a hole that does not get punched and a fill a few tiles wide.
        points: points[begin..next.min(points.len())].to_vec(),
        current: 0,
        winding: true,
    })
}

/// Takes the chain running upward from `at`, reversed so it too runs downward.
///
/// Every bound starts at a minimum, whichever direction the ring was travelling — which is what
/// lets one scan handle both sides of a shape.
fn bound_towards_minimum(points: &[[f64; 2]], at: &mut usize) -> Option<Bound> {
    if points.len().checked_sub(*at)? < 2 {
        return None;
    }
    let begin = *at;
    let mut next = begin + 1;
    while points[*at][1] > points[next][1] {
        *at += 1;
        next += 1;
        if next == points.len() {
            *at += 1;
            break;
        }
    }
    if next - begin < 2 {
        return None;
    }
    // Exclusive, for the reason [`bound_towards_maximum`] gives.
    let mut chain = points[begin..next.min(points.len())].to_vec();
    chain.reverse();
    Some(Bound {
        points: chain,
        current: 0,
        winding: false,
    })
}

/// Cuts one ring or line into bounds, indexed by the tile row each starts in.
fn build_bounds(
    points: &[[f64; 2]],
    rows: u32,
    table: &mut BTreeMap<u32, Vec<Bound>>,
    closed: bool,
) {
    if points.len() < 2 {
        return;
    }
    let mut points = points.to_vec();
    if closed {
        // A ring may arrive open or closed; the scan needs it closed.
        if points.first() != points.last() {
            points.push(points[0]);
        }
        start_on_local_minimum(&mut points);
    }

    let mut at = 0usize;
    while at < points.len() {
        let before = at;
        let to_max = bound_towards_maximum(&points, &mut at);
        let to_min = bound_towards_minimum(&points, &mut at);
        for bound in [to_max, to_min].into_iter().flatten() {
            // A chain with no vertical extent is a horizontal edge, and a horizontal edge is
            // never crossed by a scanline — it lies along one. Keeping it costs nothing in the
            // spans it contributes (its endpoints are already the endpoints of the chains on
            // either side) and corrupts the winding count, which is what actually fills the
            // polygon.
            //
            // # A deliberate divergence from mbgl
            //
            // mbgl keeps these, and it shows: two axis-aligned parts at the same latitudes get
            // a full-width bound each in their shared top row, the winding never returns to
            // zero between them, and the gap fills in. Two selected cities then download the
            // ocean between them. mbgl's own tests do not catch it because their multipolygon
            // lobes differ in latitude. Every one of those tests still passes with this in.
            if bound.points.first().map(|p| p[1]) == bound.points.last().map(|p| p[1]) {
                continue;
            }
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let row = bound.points[0][1].clamp(0.0, f64::from(rows)) as u32;
            table.entry(row).or_default().push(bound);
        }
        // Neither direction consumed anything, which happens on a degenerate tail. Without
        // this the loop spins forever on a ring of coincident points.
        if at == before {
            break;
        }
    }
}

/// Scans one row, returning the x spans every active bound covers in it.
fn scan_row(row: u32, active: &mut Vec<Bound>) -> Vec<Span> {
    let mut spans = Vec::with_capacity(active.len());
    for bound in active.iter_mut() {
        let mut span = Span {
            min: i64::MAX,
            max: i64::MIN,
            winding: bound.winding,
        };
        let edges = bound.points.len() - 1;
        while bound.current < edges {
            span.extend(bound.interpolate(row));

            let next = bound.points[bound.current + 1];
            if next[1] > f64::from(row) + 1.0 {
                // The edge leaves the row; take where it crosses the bottom.
                span.extend(bound.interpolate(row + 1));
                break;
            } else if bound.current == edges - 1 {
                // The last edge ends inside the row, so its endpoint is the extent.
                span.extend(next[0]);
            }
            bound.current += 1;
        }
        spans.push(span);
    }

    // A bound whose last edge ended in this row has nothing left to contribute.
    active.retain(|bound| {
        !(bound.current == bound.points.len() - 1
            && bound.points[bound.current][1] <= f64::from(row) + 1.0)
    });

    spans.sort_unstable_by_key(|span| (span.min, span.max));
    spans
}

/// Tiles covering a shape at one zoom, produced row by row.
///
/// An iterator rather than a `Vec` because the caller may want to stop: a polygon over a
/// continent at zoom 16 is more tiles than anyone will accept, and finding that out should not
/// require materialising them.
#[derive(Debug)]
pub struct Cover {
    zoom: u8,
    rows: u32,
    closed: bool,
    table: BTreeMap<u32, Vec<Bound>>,
    active: Vec<Bound>,
    row: u32,
    spans: VecDeque<(i64, i64)>,
    x: i64,
    done: bool,
}

impl Cover {
    /// Covers one polygon at `zoom`.
    #[must_use]
    pub fn polygon(polygon: &Polygon, zoom: u8) -> Self {
        Self::shape(core::slice::from_ref(polygon), zoom)
    }

    /// Covers a shape in any number of parts at `zoom`.
    ///
    /// Every ring of every part goes into one edge table, which is how mbgl does it and is not
    /// merely convenient: the non-zero rule has to see all of them at once, or a part that
    /// overlaps another would be filled twice and a hole cut by one part would be ignored by
    /// the next.
    #[must_use]
    pub fn shape(parts: &[Polygon], zoom: u8) -> Self {
        let rows = 1u32 << zoom;
        let mut table = BTreeMap::new();
        for ring in parts.iter().flat_map(Polygon::rings) {
            let projected: Vec<[f64; 2]> = ring
                .iter()
                .map(|point| projection::tile_units(point[0], point[1], zoom))
                .collect();
            build_bounds(&projected, rows, &mut table, true);
        }
        Self::start(zoom, rows, table, true)
    }

    fn start(zoom: u8, rows: u32, table: BTreeMap<u32, Vec<Bound>>, closed: bool) -> Self {
        let mut cover = Self {
            zoom,
            rows,
            closed,
            table,
            active: Vec::new(),
            row: 0,
            spans: VecDeque::new(),
            x: 0,
            done: false,
        };
        if cover.table.is_empty() {
            cover.done = true;
            return cover;
        }
        cover.next_row();
        match cover.spans.front() {
            Some(&(min, _)) => cover.x = min,
            None => cover.done = true,
        }
        cover
    }

    /// Gathers the bounds entering this row and merges their spans.
    fn next_row(&mut self) {
        // A shape may not touch every row — two separate lobes of one polygon, say — so skip
        // ahead to wherever the next bound actually starts rather than scanning empty rows.
        if self.active.is_empty()
            && let Some(&next) = self.table.keys().find(|&&row| row >= self.row)
            && next > self.row
        {
            self.row = next;
        }
        if let Some(entering) = self.table.remove(&self.row) {
            self.active.extend(entering);
        }

        let spans = scan_row(self.row, &mut self.active);
        if spans.is_empty() {
            return;
        }

        // The non-zero rule. A span is emitted only where the winding count has returned to
        // zero — between an opening and closing pair the tiles are interior, edge or not, and
        // that is what fills a polygon rather than outlining it.
        let mut min = spans[0].min;
        let mut max = spans[0].max;
        let mut winding = if spans[0].winding { 1i32 } else { -1 };
        for span in &spans[1..] {
            if !(self.closed && winding != 0) && span.min > max && span.max >= max {
                self.spans.push_back((min, max));
                min = span.min;
            }
            winding += if span.winding { 1 } else { -1 };
            // Replaced, not accumulated. Taking a running maximum here looks like the safe
            // reading and is wrong: two disjoint lobes of one shape then merge into a single
            // span that swallows everything between them, so a selection of two cities
            // downloads the sea in the middle. mbgl spells it the same way.
            max = span.max.max(min);
        }
        self.spans.push_back((min, max));
    }
}

impl Iterator for Cover {
    type Item = TileCoord;

    fn next(&mut self) -> Option<TileCoord> {
        loop {
            if self.done || self.row >= self.rows {
                return None;
            }
            let Some(&(_, end)) = self.spans.front() else {
                self.done = true;
                return None;
            };
            if self.x < end {
                let coord = TileCoord {
                    z: self.zoom,
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    x: self.x.clamp(0, i64::from(self.rows) - 1) as u32,
                    y: self.row,
                    // Never a second world copy. mbgl's polygon cover does not wrap either,
                    // and a ring written across the antimeridian — 170 to -170 — sweeps back
                    // across the whole map rather than the short way, so it covers everything.
                    // [`crate::cover::Bounds`] handles that case for rectangles; a shape that
                    // needs it should be split at the seam before it gets here.
                    wrap: 0,
                };
                self.advance();
                // A span may run outside the world where the projection overshot; those
                // clamp onto the edge tile, which the caller then sees twice. Cheaper to let
                // the caller dedupe than to carry a set through the whole scan.
                return Some(coord);
            }
            self.advance();
        }
    }
}

impl Cover {
    fn advance(&mut self) {
        self.x += 1;
        let Some(&(_, end)) = self.spans.front() else {
            self.done = true;
            return;
        };
        if self.x >= end {
            self.spans.pop_front();
            if self.spans.is_empty() {
                self.row += 1;
                if self.row >= self.rows && self.table.is_empty() && self.active.is_empty() {
                    self.done = true;
                    return;
                }
                self.next_row();
            }
            match self.spans.front() {
                Some(&(min, _)) => self.x = min,
                None => self.done = true,
            }
        }
    }
}
