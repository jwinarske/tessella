//! Which recorded features a region of one tile touches.
//!
//! The second half of a rendered-feature query (#338). [`crate::query`] wrote down what each feature
//! left in a bucket; this decides which of those records a tap or a box actually lands on, in that
//! tile's own units.
//!
//! # The paint has to be put back
//!
//! A bucket's vertices are **not** what was drawn, which is the correction on #338 and the thing
//! this module is shaped by. Only a fill's are literal. `line.rs` writes the *centerline* point
//! doubled with cap and side flags in the low bits, and the extrusion arrives in the vertex shader
//! from `line-width`; `circle.rs` writes the center doubled plus a 0-or-1 corner bit, so all four
//! corners of a circle's quad sit within one tile unit of each other whatever `circle-radius` says.
//!
//! So this does what mbgl's `queryIntersectsFeature` does: it grows the geometry by the paint,
//! evaluated per feature at query time. The record already carries the feature's properties and id,
//! so a data-driven `line-width` is answered by evaluating its expression against the record rather
//! than by decoding the binder's bytes -- which would mean knowing each slot's encoding, and would
//! give the same number.
//!
//! # Why a line's shape comes out of the index buffer
//!
//! A line's vertices alone do not say which centerline points are joined. Two vertices per point,
//! in emission order, and a feature clipped into three pieces leaves three runs of them with nothing
//! between -- so connecting consecutive points would invent a segment from the end of one piece to
//! the start of the next, which can cross the whole tile. A query that reports a road because of a
//! phantom join is exactly the failure that looks like nothing at all.
//!
//! `segments` cannot be used to break it either: `LineBucket::add_geometry` only starts a new one at
//! the 64k vertex cap, so one segment spans many features and many pieces.
//!
//! The triangles can. Two centerline points are joined if and only if some triangle mentions a
//! vertex of each, because that triangle is the quad between them -- and no triangle spans a gap.
//! The triangles are degenerate in position, since the vertices are all on the centerline, but their
//! *topology* is the line's shape, and that is what is read here.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use tessella_layout::fill::Segment;
use tessella_style::Value;
use tessella_style::expression::Feature;
use tessella_style::property::ResolvedProperty;

use crate::query::{Queryable, Tags};
use crate::tile::Content;

/// A query region, in one tile's own units.
///
/// Convex, because that is what the unprojection of a screen rectangle onto the map plane is: a
/// projective map takes straight lines to straight lines, so a screen rect's image is a quad rather
/// than some curved thing. A tap is the one-corner case and is not a tiny box -- the tolerance a tap
/// needs is the paint's, which this module adds anyway.
#[derive(Debug, Clone, PartialEq)]
pub struct Region {
    corners: Vec<[f64; 2]>,
}

impl Region {
    /// A single point.
    #[must_use]
    pub fn at(point: [f64; 2]) -> Self {
        Self {
            corners: alloc::vec![point],
        }
    }

    /// A convex quad, as the four corners of a screen rectangle land in this tile.
    ///
    /// Given in order around the quad, either winding. Four points that are not convex, or are
    /// given crossed, describe a region this cannot test and will answer about the convex reading
    /// of them.
    #[must_use]
    pub fn quad(corners: [[f64; 2]; 4]) -> Self {
        Self {
            corners: corners.to_vec(),
        }
    }

    /// Its corners.
    #[must_use]
    pub fn corners(&self) -> &[[f64; 2]] {
        &self.corners
    }

    /// Whether `point` is inside, with an edge counting as inside.
    fn contains(&self, point: [f64; 2]) -> bool {
        if self.corners.len() < 3 {
            return false;
        }
        // One consistent sign across every edge. Either winding, so the test is that the signs
        // agree rather than that they are positive.
        let mut positive = false;
        let mut negative = false;
        for at in 0..self.corners.len() {
            let a = self.corners[at];
            let b = self.corners[(at + 1) % self.corners.len()];
            let side = cross(a, b, point);
            if side > 0.0 {
                positive = true;
            }
            if side < 0.0 {
                negative = true;
            }
        }
        !(positive && negative)
    }

    /// How far `point` is from this region, zero inside it.
    fn distance_to_point(&self, point: [f64; 2]) -> f64 {
        if self.corners.len() == 1 {
            return length(sub(point, self.corners[0]));
        }
        if self.contains(point) {
            return 0.0;
        }
        let mut best = f64::INFINITY;
        for at in 0..self.corners.len() {
            let a = self.corners[at];
            let b = self.corners[(at + 1) % self.corners.len()];
            best = best.min(point_to_segment(point, a, b));
        }
        best
    }

    /// How far the segment `a..b` is from this region, zero where they meet.
    fn distance_to_segment(&self, a: [f64; 2], b: [f64; 2]) -> f64 {
        if self.corners.len() == 1 {
            return point_to_segment(self.corners[0], a, b);
        }
        if self.contains(a) || self.contains(b) {
            return 0.0;
        }
        let mut best = f64::INFINITY;
        for at in 0..self.corners.len() {
            let c = self.corners[at];
            let d = self.corners[(at + 1) % self.corners.len()];
            if segments_cross(a, b, c, d) {
                return 0.0;
            }
            best = best
                .min(point_to_segment(c, a, b))
                .min(point_to_segment(d, a, b))
                .min(point_to_segment(a, c, d))
                .min(point_to_segment(b, c, d));
        }
        best
    }

    /// Whether this region and a triangle overlap.
    ///
    /// Separating axes over both shapes' edges. A one-corner region has no edges of its own, so the
    /// test falls back to the triangle's three axes, which is point-in-triangle -- the same code
    /// path rather than a special case.
    fn meets_triangle(&self, triangle: [[f64; 2]; 3]) -> bool {
        separated(&self.corners, &triangle).is_none()
    }
}

/// Which of `records` the region touches, in the order they were recorded.
///
/// `units_per_pixel` converts a paint property's screen pixels into this tile's units, which is
/// mbgl's `pixelsToTileUnits`: a `circle-radius` of 5 is five pixels wherever the tile sits in the
/// cover, and the region is in tile units. A caller with no scale to offer can pass 1.0 and will get
/// an answer in which a radius means a tile unit.
///
/// Families that draw from no features answer empty, as do symbols -- a label's placement decides
/// whether it is drawn at all, which this does not see. That is #338's third slice.
#[must_use]
pub fn touched(
    content: &Content,
    records: &[Queryable],
    paint: &BTreeMap<&'static str, ResolvedProperty>,
    zoom: f64,
    units_per_pixel: f64,
    region: &Region,
) -> Vec<usize> {
    let mut hit = Vec::new();
    match content {
        // A fill's vertices are the only ones that are not packed: they are the tile coordinate
        // itself, so they are read straight rather than shifted. Shifting them halved the map, and
        // a tap inside a polygon found nothing.
        Content::Fill(bucket) => triangles(
            &bucket
                .vertices
                .iter()
                .copied()
                .map(plain)
                .collect::<Vec<_>>(),
            &bucket.indices,
            &bucket.segments,
            records,
            region,
            &mut hit,
        ),
        Content::Fill3d(bucket) => {
            let positions: Vec<[f64; 2]> = bucket
                .vertices
                .iter()
                .map(|vertex| plain(vertex.position))
                .collect();
            triangles(
                &positions,
                &bucket.indices,
                &bucket.segments,
                records,
                region,
                &mut hit,
            );
        }
        Content::Line(bucket) => {
            let half = |record: &Queryable| {
                // mbgl's `getLineWidth`: a gap splits the line in two, so the outer edge is the gap's
                // half plus the whole width; without one it is half the width.
                let width = number(paint, "line-width", zoom, record, 1.0);
                let gap = number(paint, "line-gap-width", zoom, record, 0.0);
                let outer = if gap > 0.0 {
                    gap / 2.0 + width
                } else {
                    width / 2.0
                };
                outer * units_per_pixel
            };
            lines(bucket, records, region, &half, &mut hit);
        }
        Content::Circle(bucket) => {
            let reach = |record: &Queryable| {
                (number(paint, "circle-radius", zoom, record, 5.0)
                    + number(paint, "circle-stroke-width", zoom, record, 0.0))
                    * units_per_pixel
            };
            points(&bucket.vertices, records, region, &reach, &mut hit);
        }
        Content::Heatmap(bucket) => {
            let reach = |record: &Queryable| {
                number(paint, "heatmap-radius", zoom, record, 30.0) * units_per_pixel
            };
            points(&bucket.vertices, records, region, &reach, &mut hit);
        }
        Content::Background
        | Content::Raster(_)
        | Content::Hillshade(_)
        | Content::ColorRelief(_)
        | Content::Terrain(_)
        | Content::LocationIndicator(_)
        | Content::Symbol(_) => {}
    }
    hit.dedup();
    hit
}

/// The record owning `vertex`, if any does.
///
/// The ranges tile the buffer in order -- which `the_ranges_tile_the_buffer` pins -- so this is a
/// binary search rather than a scan, and a query over a seventeen-thousand-feature layer does not
/// become quadratic in it.
fn owner(records: &[Queryable], vertex: usize) -> Option<usize> {
    let at = records.partition_point(|record| (record.vertices.end as usize) <= vertex);
    records
        .get(at)
        .filter(|record| (record.vertices.start as usize) <= vertex)
        .map(|_| at)
}

/// Records whose triangles meet the region.
fn triangles(
    vertices: &[[f64; 2]],
    indices: &[u16],
    segments: &[Segment],
    records: &[Queryable],
    region: &Region,
    hit: &mut Vec<usize>,
) {
    for segment in segments {
        let base = segment.vertex_offset as usize;
        let first = segment.index_offset as usize;
        for triangle in 0..(segment.index_length as usize) / 3 {
            let at = first + triangle * 3;
            let Some(corners) = indices.get(at..at + 3) else {
                continue;
            };
            let absolute: [usize; 3] = [
                base + corners[0] as usize,
                base + corners[1] as usize,
                base + corners[2] as usize,
            ];
            let Some(record) = owner(records, absolute[0]) else {
                continue;
            };
            if hit.last() == Some(&record) {
                continue;
            }
            let Some(points) = absolute
                .iter()
                .map(|at| vertices.get(*at).copied())
                .collect::<Option<Vec<[f64; 2]>>>()
            else {
                continue;
            };
            if region.meets_triangle([points[0], points[1], points[2]]) {
                hit.push(record);
            }
        }
    }
    hit.sort_unstable();
}

/// Records whose centerline comes within their own half-width of the region.
fn lines(
    bucket: &tessella_layout::line::LineBucket,
    records: &[Queryable],
    region: &Region,
    half: &dyn Fn(&Queryable) -> f64,
    hit: &mut Vec<usize>,
) {
    // One reach per record rather than per triangle: a data-driven width is one expression
    // evaluation per feature, and a road layer has thousands of triangles to a feature.
    let reach: Vec<f64> = records.iter().map(half).collect();
    for segment in &bucket.segments {
        let base = segment.vertex_offset as usize;
        let first = segment.index_offset as usize;
        for triangle in 0..(segment.index_length as usize) / 3 {
            let at = first + triangle * 3;
            let Some(corners) = indices_of(&bucket.indices, at) else {
                continue;
            };
            let absolute = [
                base + corners[0] as usize,
                base + corners[1] as usize,
                base + corners[2] as usize,
            ];
            let Some(record) = owner(records, absolute[0]) else {
                continue;
            };
            if hit.contains(&record) {
                continue;
            }
            // The two centerline points this quad spans. A triangle of one quad mentions two of
            // them, so the distinct pair is the segment -- and a triangle whose three vertices are
            // all one point is a cap, which is covered by its neighbor.
            for (left, right) in [(0, 1), (1, 2), (0, 2)] {
                if absolute[left] / 2 == absolute[right] / 2 {
                    continue;
                }
                let (Some(a), Some(b)) = (
                    bucket.vertices.get(absolute[left]),
                    bucket.vertices.get(absolute[right]),
                ) else {
                    continue;
                };
                if region.distance_to_segment(real(a.pos_normal), real(b.pos_normal))
                    <= reach[record]
                {
                    hit.push(record);
                    break;
                }
            }
        }
    }
    hit.sort_unstable();
}

/// Three indices at `at`, if the buffer holds them.
fn indices_of(indices: &[u16], at: usize) -> Option<[u16; 3]> {
    let window = indices.get(at..at + 3)?;
    Some([window[0], window[1], window[2]])
}

/// Records whose points come within their own reach of the region.
///
/// Four vertices a point, all within a tile unit of each other, so the first of each four is the
/// center and the other three say nothing a query wants.
fn points(
    vertices: &[[i16; 2]],
    records: &[Queryable],
    region: &Region,
    reach: &dyn Fn(&Queryable) -> f64,
    hit: &mut Vec<usize>,
) {
    for (at, record) in records.iter().enumerate() {
        let far = reach(record);
        let mut found = false;
        let mut vertex = record.vertices.start as usize;
        while vertex < record.vertices.end as usize && !found {
            if let Some(center) = vertices.get(vertex) {
                found = region.distance_to_point(real(*center)) <= far;
            }
            vertex += 4;
        }
        if found {
            hit.push(at);
        }
    }
}

/// A packed vertex as the coordinate it stands for.
///
/// A line doubles its centerline point and puts a cap flag in x's low bit and a side flag in y's; a
/// circle doubles its center and adds a 0-or-1 corner bit. Either way the shift is the coordinate.
fn real(packed: [i16; 2]) -> [f64; 2] {
    [f64::from(packed[0] >> 1), f64::from(packed[1] >> 1)]
}

/// An unpacked vertex, which is a fill's.
///
/// Separate from [`real`] rather than the same function, because the difference is not cosmetic: a
/// fill writes the tile coordinate itself, and shifting it reports every polygon at half its
/// position. That is a map-wide miss that looks like a projection bug.
fn plain(position: [i16; 2]) -> [f64; 2] {
    [f64::from(position[0]), f64::from(position[1])]
}

/// A paint property's number for one record, or `fallback` when the style did not set it.
///
/// Evaluated against the record rather than read out of the binder: the record carries the feature's
/// properties and id, which is what a data-driven expression reads, and it gives the same number
/// without this having to know each slot's byte encoding.
///
/// An expression that fails takes the fallback, as it does at build time -- `PropertyExpression`'s
/// own rule. A query is not the place to discover that a style's `line-width` does not type.
fn number(
    paint: &BTreeMap<&'static str, ResolvedProperty>,
    name: &str,
    zoom: f64,
    record: &Queryable,
    fallback: f64,
) -> f64 {
    let Some(property) = paint.get(name) else {
        return fallback;
    };
    let shim = Recorded(record);
    property
        .expression
        .evaluate(Some(zoom), Some(&shim as &dyn Feature))
        .ok()
        .and_then(|value| value.as_number())
        .unwrap_or(fallback)
}

/// A record as the thing an expression reads.
struct Recorded<'a>(&'a Queryable);

impl Feature for Recorded<'_> {
    fn property(&self, key: &str) -> Option<Value> {
        self.0.properties.get(key)
    }

    fn geometry_type(&self) -> &str {
        self.0.geometry_type
    }

    fn id(&self) -> Option<Value> {
        self.0.id.clone()
    }

    fn properties(&self) -> Value {
        // Built on demand, which is once per record per property that asks for the whole object --
        // `["properties"]` and nothing else. The per-key path above is what a style normally takes.
        match &self.0.properties {
            Tags::Json(map) => Value::Object(
                map.iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
            ),
            Tags::Mvt { .. } => Value::Object(
                self.0
                    .properties
                    .iter()
                    .map(|(key, value)| (alloc::string::String::from(key), value))
                    .collect(),
            ),
        }
    }
}

/// `a -> b` crossed with `a -> point`.
fn cross(a: [f64; 2], b: [f64; 2], point: [f64; 2]) -> f64 {
    let edge = sub(b, a);
    let to = sub(point, a);
    edge[0] * to[1] - edge[1] * to[0]
}

fn sub(a: [f64; 2], b: [f64; 2]) -> [f64; 2] {
    [a[0] - b[0], a[1] - b[1]]
}

fn length(v: [f64; 2]) -> f64 {
    v[0].hypot(v[1])
}

/// How far `point` is from the segment `a..b`.
fn point_to_segment(point: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let along = sub(b, a);
    let span = along[0] * along[0] + along[1] * along[1];
    if span == 0.0 {
        return length(sub(point, a));
    }
    let to = sub(point, a);
    let at = ((to[0] * along[0] + to[1] * along[1]) / span).clamp(0.0, 1.0);
    length(sub(point, [a[0] + along[0] * at, a[1] + along[1] * at]))
}

/// Whether the segments `a..b` and `c..d` cross, touching counting as crossing.
fn segments_cross(a: [f64; 2], b: [f64; 2], c: [f64; 2], d: [f64; 2]) -> bool {
    let (one, two) = (cross(a, b, c), cross(a, b, d));
    let (three, four) = (cross(c, d, a), cross(c, d, b));
    if (one > 0.0) != (two > 0.0) && (three > 0.0) != (four > 0.0) {
        return true;
    }
    // Collinear or touching: a zero on either side with the point inside the other segment's span.
    (one == 0.0 && on_segment(a, b, c))
        || (two == 0.0 && on_segment(a, b, d))
        || (three == 0.0 && on_segment(c, d, a))
        || (four == 0.0 && on_segment(c, d, b))
}

/// Whether a collinear `point` lies within the segment `a..b`.
fn on_segment(a: [f64; 2], b: [f64; 2], point: [f64; 2]) -> bool {
    point[0] >= a[0].min(b[0])
        && point[0] <= a[0].max(b[0])
        && point[1] >= a[1].min(b[1])
        && point[1] <= a[1].max(b[1])
}

/// A separating axis between two convex shapes, or `None` when they overlap.
///
/// Returned rather than a bare bool so that a test can say *why* two shapes are apart, which is the
/// difference between a geometry bug and a scale one.
fn separated(one: &[[f64; 2]], two: &[[f64; 2]]) -> Option<[f64; 2]> {
    for shape in [one, two] {
        if shape.len() < 2 {
            continue;
        }
        for at in 0..shape.len() {
            let a = shape[at];
            let b = shape[(at + 1) % shape.len()];
            let edge = sub(b, a);
            let axis = [-edge[1], edge[0]];
            if axis[0] == 0.0 && axis[1] == 0.0 {
                continue;
            }
            let (low_one, high_one) = extent(one, axis);
            let (low_two, high_two) = extent(two, axis);
            if high_one < low_two || high_two < low_one {
                return Some(axis);
            }
        }
    }
    None
}

/// A shape's span along `axis`.
fn extent(shape: &[[f64; 2]], axis: [f64; 2]) -> (f64, f64) {
    let mut low = f64::INFINITY;
    let mut high = f64::NEG_INFINITY;
    for point in shape {
        let at = point[0] * axis[0] + point[1] * axis[1];
        low = low.min(at);
        high = high.max(at);
    }
    (low, high)
}
