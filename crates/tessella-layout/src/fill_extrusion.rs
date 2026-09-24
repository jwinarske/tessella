//! Extruded polygons — mbgl's `FillExtrusionBucket`.
//!
//! # The instanced path, and why that is the one to port
//!
//! mbgl builds this two ways, chosen by `MLN_USE_FILL_EXTRUSION_INSTANCING`, which its own header
//! defines as `(MLN_RENDER_BACKEND_METAL || MLN_RENDER_BACKEND_VULKAN)`. DR-16 settled this build
//! on Vulkan, so the instanced path is not a choice made here — it is the one the target backend
//! takes, and the one whose attribute ids the generated tables carry.
//!
//! The difference is not a detail. Without instancing a bucket emits four extra vertices and six
//! extra indices *per edge* to build the walls, and each carries a 2D normal. With instancing it
//! emits the ring's own vertices and nothing else: the walls are drawn as instances over the same
//! buffer, so a building is its outline plus an earcut roof. A port of the wrong branch produces
//! roughly five times the geometry and a vertex layout the shader does not read.
//!
//! # The vertex packing
//!
//! Two attributes, and the second is three things at once. `Short2` carries the *integer* part of
//! the position; `UShort2` carries the fractional part of both axes packed into one number
//! together with a discard flag, and the edge distance beside it.
//!
//! The fractional part is why. An extrusion's ground outline has to line up with the walls the
//! shader raises from it, and a tile coordinate rounded to an integer moves the wall's foot by up
//! to half a unit — visible as a seam between a building and its own shadow. mbgl keeps seven
//! bits per axis: `(frac.x * 256 + frac.y) * 2 + discarded`.
//!
//! # Edge distance is for patterns, and it wraps
//!
//! `edgeDistance` accumulates along a ring so a `fill-extrusion-pattern` can run continuously
//! around a wall. It is a `u16`, and mbgl resets it to zero rather than letting it wrap — the
//! reset repeats the pattern from its start, where a wrap would jump it to an arbitrary phase.

use alloc::vec::Vec;

use crate::fill::{Ring, Segment, classify_rings, limit_holes};

/// One extrusion vertex, in the layout the instanced shader binds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtrusionVertex {
    /// The position's integer part, in tile units.
    pub position: [i16; 2],
    /// The fractional part and the discard flag, packed as mbgl packs them.
    pub decimals: u16,
    /// Distance along the ring, for wrapping a pattern.
    pub edge_distance: u16,
}

/// A fill-extrusion layer's geometry for one tile.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FillExtrusionBucket {
    /// One per ring point.
    pub vertices: Vec<ExtrusionVertex>,
    /// Roof triangles, from earcut.
    pub indices: Vec<u16>,
    /// Draw segments.
    pub segments: Vec<Segment>,
    /// Whether the layer declares a `fill-extrusion-pattern`.
    ///
    /// mbgl's `hasPattern`, and it is *unevaluated*: what matters is that the style asks for a
    /// pattern, not that the atlas had one. Together with [`Self::opaque`] it decides whether
    /// there is a depth pass — see [`Self::needs_depth_pass`].
    pub patterned: bool,
    /// Whether the layer's opacity is one, which decides how many passes it takes.
    ///
    /// mbgl's `opaque = evaluated.get<FillExtrusionOpacity>() >= 1`, and it reaches the bucket
    /// because the *geometry* is the same either way while the drawable count is not. A
    /// translucent extrusion needs a depth-only pass in front of its color pass; an opaque one
    /// does not.
    pub opaque: bool,
}

/// Largest vertex index a segment can address.
const MAX_SEGMENT_VERTICES: usize = u16::MAX as usize;

/// Packs a position into the two attributes the shader reads.
///
/// `discarded` marks a ring's closing point, which has no edge leaving it and therefore no wall
/// to raise — mbgl passes `!p2`, the absence of a next point. The flag rides in the low bit of
/// the packed fraction rather than in a field of its own, which is why the fraction is multiplied
/// by two.
///
/// The input is tile-unit integers here where mbgl's is `double`, so the fractional part is
/// always zero. That is not a simplification of the packing: a rounded-corner or a
/// simplification pass produces fractional positions, and the layout has to carry them or the
/// walls part company with the roof. The arithmetic is mbgl's so that it already does.
#[must_use]
pub fn pack_vertex(x: f64, y: f64, discarded: bool, edge_distance: u16) -> ExtrusionVertex {
    let (int_x, int_y) = (x.floor(), y.floor());
    // Seven bits per axis: the fraction times 128, which lands in 0..=127.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let frac_x = ((x - int_x) * 128.0) as u8;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let frac_y = ((y - int_y) * 128.0) as u8;

    #[allow(clippy::cast_possible_truncation)]
    let position = [int_x as i16, int_y as i16];
    let packed = (u16::from(frac_x) * 256 + u16::from(frac_y)) * 2 + u16::from(discarded);

    ExtrusionVertex {
        position,
        decimals: packed,
        edge_distance,
    }
}

/// A ring point, integer off the tile or fractional out of the corner rounder.
///
/// mbgl carries the same split as a `std::variant<GeometryCollection, GeometryCollectionFloat>`
/// and visits it; this is that variant as a trait, so the emit loop below is written once and the
/// unrounded path still walks the tile's own `i16` rings without converting them.
pub trait RingPoint: Copy {
    /// The x in tile units, fractional part included.
    fn x(self) -> f64;
    /// The y in tile units.
    fn y(self) -> f64;
}

impl RingPoint for [i16; 2] {
    fn x(self) -> f64 {
        f64::from(self[0])
    }
    fn y(self) -> f64 {
        f64::from(self[1])
    }
}

impl RingPoint for [f32; 2] {
    fn x(self) -> f64 {
        f64::from(self[0])
    }
    fn y(self) -> f64 {
        f64::from(self[1])
    }
}

/// The distance between two points, rounded as mbgl rounds it.
///
/// `util::dist<uint16_t>` is a `hypot` truncated to the integer type, so a diagonal edge of ten
/// by ten contributes fourteen rather than fifteen. The rounding matters because the value
/// accumulates: taking the nearest integer instead drifts a pattern along a long wall.
fn edge_length<P: RingPoint>(a: P, b: P) -> u32 {
    let dx = b.x() - a.x();
    let dy = b.y() - a.y();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        dx.hypot(dy) as u32
    }
}

impl FillExtrusionBucket {
    /// Whether this needs a depth-only pass in front of its color pass.
    ///
    /// mbgl's `doDepthPass = (!opaque || hasPattern)`. Both halves matter and only the first was
    /// implemented: an *opaque* extrusion with a pattern still gets one, because a pattern is
    /// sampled per fragment and can be transparent wherever the sprite is, so the surface is not
    /// opaque whatever the opacity says.
    ///
    /// It decides two things that were being decided separately — how many drawables the layer
    /// becomes, and whether the color pass is stencilled, since mbgl writes
    /// `colorBuilder->setEnableStencil(doDepthPass)`.
    #[must_use]
    pub const fn needs_depth_pass(&self) -> bool {
        !self.opaque || self.patterned
    }
}

/// Builds a bucket from one feature's rings.
#[must_use]
pub fn build(rings: &[Ring]) -> FillExtrusionBucket {
    build_features(core::slice::from_ref(&rings))
}

/// As [`build_features`], reporting the bucket's vertex count after each input feature.
///
/// The paint binder needs it for the reason [`crate::fill::build_features_tracked`] gives: a
/// feature's vertex count is not the sum of its rings' lengths, because `classify_rings` may
/// split one feature into several polygons and drops degenerate ones. An extrusion's three
/// data-driven properties — color, height and base — are all bound this way, so a miscount
/// paints one building with its neighbor's height.
#[must_use]
pub fn build_features_tracked(
    features: &[&[Ring]],
    opaque: bool,
    patterned: bool,
    rounded_corner_distance: f64,
) -> (FillExtrusionBucket, Vec<usize>) {
    let mut bucket = FillExtrusionBucket {
        opaque,
        patterned,
        ..FillExtrusionBucket::default()
    };
    let mut ends = Vec::with_capacity(features.len());
    for rings in features {
        build_into(&mut bucket, rings, rounded_corner_distance);
        ends.push(bucket.vertices.len());
    }
    (bucket, ends)
}

/// Builds a bucket from several features, each classified on its own.
///
/// Per feature for the reason [`crate::fill::build_features`] gives: `classify_rings` decides
/// exterior from hole by winding, and handed every feature's rings at once it attaches one
/// feature's hole to another's exterior.
#[must_use]
pub fn build_features(features: &[&[Ring]]) -> FillExtrusionBucket {
    let mut bucket = FillExtrusionBucket::default();
    for rings in features {
        build_into(&mut bucket, rings, 0.0);
    }
    bucket
}

/// Rounds a classified polygon's corners, as mbgl's `roundPolygonCorners` does.
///
/// A port of `src/mbgl/tile/geometry_tile_data.cpp`. Every constant is mbgl's:
///
/// | | |
/// |---|---|
/// | `ARC_POINTS` | 3 interior points, so a rounded corner is five points: start, three, end |
/// | `MAX_EDGE_LEN_PERCENT` | 0.2 -- a corner never eats more than a fifth of either edge |
/// | parallel threshold | `sin(5 degrees)`; below it the two edges are one and the corner stands |
///
/// # Why the output is `f32`
///
/// The arc does not land on tile units, and mbgl keeps it in a `GeometryCollectionFloat` for
/// exactly that reason. It reaches the wire at the same precision either way -- the position
/// attribute is `Short2` and [`pack_vertex`] puts the fraction in `decimals` -- but rounding to
/// integers *here* would move the arc before it was packed, and the walls would part company with
/// the roof by up to half a tile unit.
///
/// # What a corner costs
///
/// Five points where there was one, so a closed rectangle of five points becomes twenty-one. A
/// corner is left alone -- one point -- when either edge has zero length, or when the two edges
/// are within five degrees of parallel and there is no arc to strike.
#[must_use]
pub fn round_polygon_corners(polygon: &[Ring], desired_corner_distance: f64) -> Vec<Vec<[f32; 2]>> {
    /// Interior points on each arc.
    const ARC_POINTS: usize = 3;
    /// The most of an edge one corner may consume.
    const MAX_EDGE_LEN_PERCENT: f64 = 0.2;

    let parallel_threshold = 5.0_f64.to_radians().sin();
    let cross = |a: (f64, f64), b: (f64, f64)| a.0 * b.1 - a.1 * b.0;
    // mbgl's `util::perp`: (-y, x).
    let perp = |a: (f64, f64)| (-a.1, a.0);
    let unit = |from: (f64, f64), to: (f64, f64)| {
        let (dx, dy) = (to.0 - from.0, to.1 - from.1);
        let len = dx.hypot(dy);
        (dx / len, dy / len)
    };

    let mut rounded = Vec::with_capacity(polygon.len());
    for ring in polygon {
        // mbgl's `nVertices = ring.size() - 1`: the ring is closed, so the repeated first point
        // is not a corner of its own. A ring too short to have one is copied through.
        let Some(corners) = ring.len().checked_sub(1).filter(|n| *n > 0) else {
            rounded.push(ring.iter().map(|p| [p[0] as f32, p[1] as f32]).collect());
            continue;
        };

        let at = |i: usize| (f64::from(ring[i][0]), f64::from(ring[i][1]));
        let mut out: Vec<[f32; 2]> = Vec::with_capacity(corners * (ARC_POINTS + 2) + 1);
        #[allow(clippy::cast_possible_truncation)]
        let keep = |out: &mut Vec<[f32; 2]>, i: usize| {
            out.push([ring[i][0] as f32, ring[i][1] as f32]);
        };

        for i in 0..corners {
            let previous = at((i + corners - 1) % corners);
            let corner = at(i);
            let next = at((i + 1) % corners);

            let edge1_len = (corner.0 - previous.0).hypot(corner.1 - previous.1);
            let edge2_len = (next.0 - corner.0).hypot(next.1 - corner.1);
            if edge1_len == 0.0 || edge2_len == 0.0 {
                // A duplicate vertex has no direction to round.
                keep(&mut out, i);
                continue;
            }

            let edge1 = unit(previous, corner);
            let edge2 = unit(corner, next);
            let distance = desired_corner_distance
                .min(edge1_len * MAX_EDGE_LEN_PERCENT)
                .min(edge2_len * MAX_EDGE_LEN_PERCENT);

            let start = (corner.0 - edge1.0 * distance, corner.1 - edge1.1 * distance);
            let end = (corner.0 + edge2.0 * distance, corner.1 + edge2.1 * distance);

            // Both perpendiculars must point to the same side of the turn, which its handedness
            // decides.
            let (mut perp1, mut perp2) = (perp(edge1), perp(edge2));
            if cross(edge1, edge2) < 0.0 {
                perp1 = (-perp1.0, -perp1.1);
                perp2 = (-perp2.0, -perp2.1);
            }

            // The arc's center is where the two perpendiculars meet, and they do not meet when
            // the edges are parallel.
            let perp_cross = cross(perp1, perp2);
            if perp_cross.abs() < parallel_threshold {
                keep(&mut out, i);
                continue;
            }
            let t = cross((end.0 - start.0, end.1 - start.1), perp2) / perp_cross;
            let center = (start.0 + perp1.0 * t, start.1 + perp1.1 * t);

            #[allow(clippy::cast_possible_truncation)]
            out.push([start.0 as f32, start.1 as f32]);

            let radius = (start.0 - center.0).hypot(start.1 - center.1);
            let start_angle = (start.1 - center.1).atan2(start.0 - center.0);
            // mbgl's `util::angle_between`, which is the signed turn from one vector to the other.
            let (from, to) = (
                (start.0 - center.0, start.1 - center.1),
                (end.0 - center.0, end.1 - center.1),
            );
            let arc_angle = cross(from, to).atan2(from.0 * to.0 + from.1 * to.1);
            #[allow(clippy::cast_possible_truncation)]
            for k in 1..=ARC_POINTS {
                let angle = start_angle + arc_angle * k as f64 / (ARC_POINTS + 1) as f64;
                out.push([
                    (center.0 + angle.cos() * radius) as f32,
                    (center.1 + angle.sin() * radius) as f32,
                ]);
            }

            #[allow(clippy::cast_possible_truncation)]
            out.push([end.0 as f32, end.1 as f32]);
        }

        // Close it, the way the ring that came in was closed.
        if let Some(&first) = out.first() {
            out.push(first);
        }
        rounded.push(out);
    }
    rounded
}

fn build_into(bucket: &mut FillExtrusionBucket, rings: &[Ring], rounded_corner_distance: f64) {
    for mut polygon in classify_rings(rings) {
        // mbgl caps an extrusion's interior rings exactly as it caps a fill's -- the call
        // is in `FillExtrusionBucket::addFeature`, with the same five hundred. Capping one
        // and not the other gives a building's roof a different triangulation from the
        // fill beneath it, for the same rings.
        limit_holes(&mut polygon);
        // mbgl's order exactly: classify, cap the holes, *then* round. Rounding first would
        // round a ring the cap was about to drop.
        if rounded_corner_distance > 0.0 {
            emit_polygon(
                bucket,
                &round_polygon_corners(&polygon, rounded_corner_distance),
            );
        } else {
            emit_polygon(bucket, &polygon);
        }
    }
}

/// Writes one classified polygon's roof and its wall vertices into `bucket`.
///
/// Generic over the point so the rounded and unrounded paths share it -- see [`RingPoint`].
fn emit_polygon<P: RingPoint>(bucket: &mut FillExtrusionBucket, polygon: &[alloc::vec::Vec<P>]) {
    let total: usize = polygon.iter().map(|ring| ring.len()).sum();
    if total == 0 {
        return;
    }
    // mbgl refuses a polygon whose points cannot be indexed rather than truncating it: a
    // partial building is a shape nobody drew.
    if total > MAX_SEGMENT_VERTICES {
        return;
    }

    let start = bucket.vertices.len();
    let needs_segment = bucket
        .segments
        .last()
        .is_none_or(|segment| segment.vertex_length as usize + total > MAX_SEGMENT_VERTICES);
    if needs_segment {
        #[allow(clippy::cast_possible_truncation)]
        bucket.segments.push(Segment {
            vertex_offset: start as u32,
            index_offset: bucket.indices.len() as u32,
            vertex_length: 0,
            index_length: 0,
        });
    }
    let segment = bucket.segments.last_mut().expect("just pushed or present");
    let base = segment.vertex_length;

    // Where each ring point landed, so earcut's output can be mapped back. Earcut
    // numbers points across the whole polygon; the buffer numbers them within a segment.
    let mut slots: Vec<u16> = Vec::with_capacity(total);
    let mut flat: Vec<f64> = Vec::with_capacity(total * 2);
    let mut holes: Vec<usize> = Vec::new();
    for (ring_index, ring) in polygon.iter().enumerate() {
        if ring_index > 0 {
            holes.push(flat.len() / 2);
        }
        let mut edge_distance: u32 = 0;
        for (index, point) in ring.iter().enumerate() {
            let next = ring.get(index + 1);
            #[allow(clippy::cast_possible_truncation)]
            bucket.vertices.push(pack_vertex(
                point.x(),
                point.y(),
                next.is_none(),
                edge_distance as u16,
            ));
            #[allow(clippy::cast_possible_truncation)]
            slots.push(base as u16 + slots.len() as u16);
            flat.push(point.x());
            flat.push(point.y());

            if let Some(next) = next {
                let step = edge_length(*point, *next);
                // Reset rather than wrap: a wrapped distance restarts the pattern at an
                // arbitrary phase, where a reset restarts it at its beginning.
                if edge_distance + step > u32::from(u16::MAX) {
                    edge_distance = 0;
                }
                edge_distance += step;
            }
        }
    }

    let roof = earcutr::earcut(&flat, &holes, 2).unwrap_or_default();
    for triangle in roof.as_chunks::<3>().0 {
        // Counter-clockwise, which mbgl produces by swapping the second and third
        // indices of earcut's output.
        let (Some(&a), Some(&c), Some(&b)) = (
            slots.get(triangle[0]),
            slots.get(triangle[2]),
            slots.get(triangle[1]),
        ) else {
            continue;
        };
        bucket.indices.extend_from_slice(&[a, c, b]);
    }

    #[allow(clippy::cast_possible_truncation)]
    {
        segment.vertex_length += total as u32;
        segment.index_length += roof.len() as u32;
    }
}
