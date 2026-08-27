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

use crate::fill::{Position, Ring, Segment, classify_rings, limit_holes};

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
    /// Whether the layer's opacity is one, which decides how many passes it takes.
    ///
    /// mbgl's `opaque = evaluated.get<FillExtrusionOpacity>() >= 1`, and it reaches the bucket
    /// because the *geometry* is the same either way while the drawable count is not. A
    /// translucent extrusion needs a depth-only pass in front of its colour pass; an opaque one
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

/// The distance between two points, rounded as mbgl rounds it.
///
/// `util::dist<uint16_t>` is a `hypot` truncated to the integer type, so a diagonal edge of ten
/// by ten contributes fourteen rather than fifteen. The rounding matters because the value
/// accumulates: taking the nearest integer instead drifts a pattern along a long wall.
fn edge_length(a: Position, b: Position) -> u32 {
    let dx = f64::from(b[0]) - f64::from(a[0]);
    let dy = f64::from(b[1]) - f64::from(a[1]);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        dx.hypot(dy) as u32
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
/// data-driven properties — colour, height and base — are all bound this way, so a miscount
/// paints one building with its neighbour's height.
#[must_use]
pub fn build_features_tracked(
    features: &[&[Ring]],
    opaque: bool,
) -> (FillExtrusionBucket, Vec<usize>) {
    let mut bucket = FillExtrusionBucket {
        opaque,
        ..FillExtrusionBucket::default()
    };
    let mut ends = Vec::with_capacity(features.len());
    for rings in features {
        build_into(&mut bucket, rings);
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
        build_into(&mut bucket, rings);
    }
    bucket
}

fn build_into(bucket: &mut FillExtrusionBucket, rings: &[Ring]) {
    {
        for mut polygon in classify_rings(rings) {
            // mbgl caps an extrusion's interior rings exactly as it caps a fill's -- the call
            // is in `FillExtrusionBucket::addFeature`, with the same five hundred. Capping one
            // and not the other gives a building's roof a different triangulation from the
            // fill beneath it, for the same rings.
            limit_holes(&mut polygon);
            let total: usize = polygon.iter().map(|ring| ring.len()).sum();
            if total == 0 {
                continue;
            }
            // mbgl refuses a polygon whose points cannot be indexed rather than truncating it: a
            // partial building is a shape nobody drew.
            if total > MAX_SEGMENT_VERTICES {
                continue;
            }

            let start = bucket.vertices.len();
            let needs_segment = bucket.segments.last().is_none_or(|segment| {
                segment.vertex_length as usize + total > MAX_SEGMENT_VERTICES
            });
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
                        f64::from(point[0]),
                        f64::from(point[1]),
                        next.is_none(),
                        edge_distance as u16,
                    ));
                    #[allow(clippy::cast_possible_truncation)]
                    slots.push(base as u16 + slots.len() as u16);
                    flat.push(f64::from(point[0]));
                    flat.push(f64::from(point[1]));

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
    }
}
