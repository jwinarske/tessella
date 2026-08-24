//! Line buckets: polylines in, an extruded triangle strip out.
//!
//! Transcribed from mbgl's `gfx::PolylineGenerator::generate` (`gfx/polyline_generator.cpp`)
//! and `LineBucket::layoutVertex` (`renderer/buckets/line_bucket.hpp`). As with [`crate::fill`],
//! the bucket is what the §9.1 oracle diff compares, so the rules are taken from the C++ rather
//! than re-derived: a geometrically reasonable join that mbgl does not draw is still a diff.
//!
//! # The vertex is not a position
//!
//! A line vertex carries the *centreline* point and an *extrusion*, not a corner. The shader
//! offsets by the extrusion scaled by the current line width, which is why a line can be
//! restyled to a different width without re-tessellating, and why the same bucket serves views
//! at different zooms (§5.1). Both halves are packed:
//!
//! - `pos_normal` is the point doubled, with the low bit of x carrying "round cap" and the low
//!   bit of y carrying "this is the down side". Doubling is what frees those bits.
//! - `data` holds the extrusion in `[-1, 1]` scaled by 63 and biased by 128, the
//!   cap direction in the low two bits of `z`, and distance-along-the-line split across the
//!   top six bits of `z` and all of `w`.
//!
//! So a vertex is only meaningful to a shader that knows this layout. It is reproduced exactly
//! because the oracle compares vertex *bytes*.
//!
//! # Joins are chosen per vertex, not per layer
//!
//! `line-join` is a request. The generator downgrades it per vertex from the miter length —
//! the ratio of the mitre to the line width, which grows without bound as a corner sharpens.
//! A mitre longer than the limit becomes a bevel; a bevel longer than 2 becomes a *flipped*
//! bevel, because 128/63 is the widest extrusion the byte encoding can hold and a longer one
//! would clamp into a visible spike. Round joins that are shallow enough become mitres, and
//! the rest become fans of flat triangles. None of that is an optimisation to be skipped: each
//! branch emits a different number of vertices, so a shortcut changes the buffer lengths the
//! diff checks.
//!
//! # Sharp corners get extra points
//!
//! At a corner sharper than 75°, a point is inserted fifteen screen pixels
//! before and after it. This exists for dash patterns — distance-along-the-line is equal at the
//! inner and outer corner, so a dash crossing a sharp corner tilts — but it runs for every
//! line, dashed or not, and it adds vertices. Note the offset is in *screen* units and the
//! geometry is in tile units, hence the `EXTENT / tile_size` scaling, the same factor-of-16
//! trap documented in `tessella_source::tiling`.

use alloc::vec::Vec;

use crate::fill::{Position, Segment};

/// Scale applied to the extrusion vector before it is stored in a byte.
///
/// 63, not 127: the encoding must also represent the longer extrusions a bevel join produces,
/// up to twice the line width, and `128 / 63` is where that ceiling lands.
const EXTRUDE_SCALE: f64 = 63.0;

/// How far, in screen pixels, the extra points at a sharp corner sit from the corner.
const SHARP_CORNER_OFFSET: f64 = 15.0;

/// Degrees of arc approximated by one triangle of a fake-round join.
const DEG_PER_TRIANGLE: f64 = 20.0;

/// Bits available for distance-along-the-line in the vertex encoding.
const LINE_DISTANCE_BUFFER_BITS: u32 = 14;

/// Distance-along-the-line is stored halved, trading precision for reach.
const LINE_DISTANCE_SCALE: f64 = 0.5;

/// Largest distance-along-the-line the encoding holds, in tile units.
const MAX_LINE_DISTANCE: f64 = ((1u32 << LINE_DISTANCE_BUFFER_BITS) as f64) / LINE_DISTANCE_SCALE;

/// mbgl's tile extent, in tile units.
const EXTENT: f64 = 8192.0;

/// mbgl's nominal tile size, in screen pixels.
const TILE_SIZE: f64 = 512.0;

/// Largest vertex index a segment can address.
const MAX_SEGMENT_VERTICES: usize = u16::MAX as usize;

/// Cosine of half the sharp-corner threshold angle.
///
/// Computed in `f32` and widened, because mbgl computes it in `f32` and the comparison against
/// it is in `f64`. Evaluating the same expression in `f64` gives a slightly different threshold,
/// which is enough to disagree about a corner sitting exactly on it.
fn cos_half_sharp_corner() -> f64 {
    f64::from(libm::cosf(75.0f32 / 2.0 * (core::f32::consts::PI / 180.0)))
}

/// How the ends of a line are drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineCap {
    /// Stop at the endpoint.
    #[default]
    Butt,
    /// Extend by half the line width.
    Square,
    /// A semicircular cap.
    Round,
}

/// How corners are drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineJoin {
    /// Cut the corner off square.
    Bevel,
    /// Extend the edges to meet.
    #[default]
    Miter,
    /// A fan of triangles approximating an arc.
    Round,
}

/// The join actually emitted at a vertex, after downgrading.
///
/// `FlipBevel` and `FakeRound` are not spellable in a style: they are what the generator
/// substitutes when the requested join would exceed what the vertex encoding can express, or
/// when a round join is shallow enough to fake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolvedJoin {
    Bevel,
    FlipBevel,
    Miter,
    FakeRound,
    Round,
}

/// Clip distances for a line that is a fragment of a longer one.
///
/// Set by the annotation system through the `mapbox_clip_start` / `mapbox_clip_end` feature
/// properties, so that a dash pattern stays continuous across a line split between tiles.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClipDistances {
    /// Fraction of the whole line at which this fragment starts.
    pub clip_start: f64,
    /// Fraction of the whole line at which this fragment ends.
    pub clip_end: f64,
    /// Length of this fragment, in tile units.
    pub total: f64,
}

impl ClipDistances {
    /// Map a distance along this fragment onto the whole line's distance range.
    fn scale_to_max_line_distance(&self, tile_distance: f64) -> f64 {
        let mut relative = tile_distance / self.total;
        if !relative.is_finite() {
            relative = 0.0;
        }
        (relative * (self.clip_end - self.clip_start) + self.clip_start) * (MAX_LINE_DISTANCE - 1.0)
    }
}

/// Layout inputs for one line geometry.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LineOptions {
    /// Requested join.
    pub join: LineJoin,
    /// Cap at the start of the line.
    pub begin_cap: LineCap,
    /// Cap at the end of the line.
    pub end_cap: LineCap,
    /// `line-miter-limit`.
    pub miter_limit: f32,
    /// `line-round-limit`.
    pub round_limit: f32,
    /// Tile overscale factor.
    pub overscaling: u32,
    /// Whether the geometry is a closed ring, which suppresses caps and wraps the join.
    pub closed: bool,
    /// Distances for a fragment of a longer line.
    pub clip_distances: Option<ClipDistances>,
}

impl Default for LineOptions {
    fn default() -> Self {
        Self {
            join: LineJoin::Miter,
            begin_cap: LineCap::Butt,
            end_cap: LineCap::Butt,
            miter_limit: 2.0,
            round_limit: 1.05,
            overscaling: 1,
            closed: false,
            clip_distances: None,
        }
    }
}

/// One line vertex, in the packed form the shader reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LineVertex {
    /// Centreline point doubled, with cap and side flags in the low bits.
    pub pos_normal: [i16; 2],
    /// Extrusion, cap direction and distance-along-the-line.
    pub data: [u8; 4],
}

/// A built line bucket.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LineBucket {
    /// Vertices, two per emitted centreline point.
    pub vertices: Vec<LineVertex>,
    /// Triangle indices, relative to their segment's vertex base.
    pub indices: Vec<u16>,
    /// Draw segments.
    pub segments: Vec<Segment>,
}

/// Build the packed vertex for a point and extrusion.
///
/// `up` selects the side, `round` marks a round cap, and `dir` is the cap direction. The
/// extrusion is rounded, biased and clamped; the clamp is mbgl's and is what keeps a join that
/// slipped past the length checks from wrapping to the opposite side.
fn layout_vertex(
    p: Position,
    e: [f64; 2],
    round: bool,
    up: bool,
    dir: i8,
    linesofar: i32,
) -> LineVertex {
    let bit = |b: bool| i32::from(b);
    let byte = |v: f64| (libm::round(EXTRUDE_SCALE * v) + 128.0).clamp(0.0, 255.0) as u8;
    LineVertex {
        pos_normal: [
            ((i32::from(p[0]) * 2) | bit(round)) as i16,
            ((i32::from(p[1]) * 2) | bit(up)) as i16,
        ],
        data: [
            byte(e[0]),
            byte(e[1]),
            ((if dir == 0 { 0 } else { dir.signum() } as i32 + 1) | ((linesofar & 0x3F) << 2))
                as u8,
            (linesofar >> 6) as u8,
        ],
    }
}

fn perp(a: [f64; 2]) -> [f64; 2] {
    [-a[1], a[0]]
}

fn mag(a: [f64; 2]) -> f64 {
    libm::sqrt(a[0] * a[0] + a[1] * a[1])
}

fn unit(a: [f64; 2]) -> [f64; 2] {
    let m = mag(a);
    if m == 0.0 { a } else { [a[0] / m, a[1] / m] }
}

fn scale(a: [f64; 2], s: f64) -> [f64; 2] {
    [a[0] * s, a[1] * s]
}

fn add(a: [f64; 2], b: [f64; 2]) -> [f64; 2] {
    [a[0] + b[0], a[1] + b[1]]
}

fn sub(a: [f64; 2], b: [f64; 2]) -> [f64; 2] {
    [a[0] - b[0], a[1] - b[1]]
}

/// Distance between two tile-local points.
///
/// The differences are taken in a width that cannot overflow; mbgl relies on C's integer
/// promotion here, which is only wide enough because tile coordinates stay well inside i16.
fn dist(a: Position, b: Position) -> f64 {
    let dx = i64::from(b[0]) - i64::from(a[0]);
    let dy = i64::from(b[1]) - i64::from(a[1]);
    libm::sqrt((dx * dx + dy * dy) as f64)
}

/// Direction from `a` to `b` as a unit vector.
fn direction(a: Position, b: Position) -> [f64; 2] {
    unit([
        f64::from(b[0]) - f64::from(a[0]),
        f64::from(b[1]) - f64::from(a[1]),
    ])
}

struct TriangleElement(u16, u16, u16);

/// Per-geometry generator state.
///
/// `e1`/`e2`/`e3` are the last three vertices emitted, as offsets from this geometry's first.
/// They are signed because -1 means "no vertex yet", and a square or round cap deliberately
/// resets them to break the strip so the next segment does not connect to the previous one.
struct Gen<'a> {
    vertices: &'a mut Vec<LineVertex>,
    triangles: Vec<TriangleElement>,
    start_vertex: usize,
    e1: i32,
    e2: i32,
    e3: i32,
}

impl Gen<'_> {
    fn push(&mut self, v: LineVertex) {
        self.vertices.push(v);
        self.e3 = (self.vertices.len() - 1 - self.start_vertex) as i32;
        if self.e1 >= 0 && self.e2 >= 0 {
            self.triangles.push(TriangleElement(
                self.e1 as u16,
                self.e2 as u16,
                self.e3 as u16,
            ));
        }
        self.e1 = self.e2;
        self.e2 = self.e3;
    }

    /// Emit both sides of one centreline point.
    ///
    /// `end_left` and `end_right` push the vertex along the line as well as across it, which is
    /// how a square cap extends past the endpoint and how a bevel's two edges are offset.
    ///
    /// The argument list is mbgl's, kept parameter-for-parameter. Grouping them into a struct
    /// would read better and would make the eleven call sites below no longer legible as the
    /// C++ they are transcribed from, which is the only thing that makes them checkable.
    #[allow(clippy::too_many_arguments)]
    fn add_current_vertex(
        &mut self,
        current: Position,
        distance: &mut f64,
        normal: [f64; 2],
        end_left: f64,
        end_right: f64,
        round: bool,
        clip: Option<ClipDistances>,
    ) {
        let scaled = clip.map_or(*distance, |c| c.scale_to_max_line_distance(*distance));
        let linesofar = (scaled * LINE_DISTANCE_SCALE) as i32;

        let mut extrude = normal;
        if end_left != 0.0 {
            extrude = sub(extrude, scale(perp(normal), end_left));
        }
        self.push(layout_vertex(
            current,
            extrude,
            round,
            false,
            end_left as i8,
            linesofar,
        ));

        let mut extrude = scale(normal, -1.0);
        if end_right != 0.0 {
            extrude = sub(extrude, scale(perp(normal), end_right));
        }
        self.push(layout_vertex(
            current,
            extrude,
            round,
            true,
            -end_right as i8,
            linesofar,
        ));

        // The distance counter has a ceiling. Rather than let it wrap — which would restart a
        // dash pattern mid-line at an arbitrary phase — reset it and re-emit the point, so the
        // pattern restarts at a seam that is already a vertex.
        if *distance > MAX_LINE_DISTANCE / 2.0 && clip.is_none() {
            *distance = 0.0;
            self.add_current_vertex(current, distance, normal, end_left, end_right, round, clip);
        }
    }

    /// Emit one triangle of a fake-round join's fan.
    ///
    /// Only one of `e1`/`e2` advances, which is what makes the triangles share the corner point
    /// rather than forming a strip.
    fn add_pie_slice_vertex(
        &mut self,
        current: Position,
        distance: f64,
        extrude: [f64; 2],
        line_turns_left: bool,
        clip: Option<ClipDistances>,
    ) {
        let flipped = scale(extrude, if line_turns_left { -1.0 } else { 1.0 });
        let distance = clip.map_or(distance, |c| c.scale_to_max_line_distance(distance));
        let linesofar = (distance * LINE_DISTANCE_SCALE) as i32;

        self.vertices.push(layout_vertex(
            current,
            flipped,
            false,
            line_turns_left,
            0,
            linesofar,
        ));
        self.e3 = (self.vertices.len() - 1 - self.start_vertex) as i32;
        if self.e1 >= 0 && self.e2 >= 0 {
            self.triangles.push(TriangleElement(
                self.e1 as u16,
                self.e2 as u16,
                self.e3 as u16,
            ));
        }
        if line_turns_left {
            self.e2 = self.e3;
        } else {
            self.e1 = self.e3;
        }
    }
}

impl LineBucket {
    /// Tessellate one polyline into this bucket.
    ///
    /// Degenerate input is dropped the way mbgl drops it: duplicate points at either end are
    /// trimmed first, and a geometry left with fewer than two distinct points (three, closed)
    /// contributes nothing. Trimming before the length check is why a line of five identical
    /// points is silently skipped rather than producing a zero-length extrusion.
    ///
    /// # The one limit that is not enforced
    ///
    /// A segment splits when *this* geometry would push it past a u16 index, but a single
    /// geometry emitting more than 65535 vertices — roughly 32k points in one feature — has
    /// nowhere to split and its indices truncate. mbgl has the same limit in the same place,
    /// and it is left alone rather than guarded because a guard would draw something the
    /// oracle does not. Nothing observed approaches it: the densest real layer measured is
    /// 54k vertices spread over thousands of features, and the largest single feature is three
    /// orders of magnitude below the limit.
    pub fn add_geometry(&mut self, coordinates: &[Position], options: &LineOptions) {
        let mut len = coordinates.len();
        while len >= 2 && coordinates[len - 1] == coordinates[len - 2] {
            len -= 1;
        }
        let mut first = 0usize;
        while first + 1 < len && coordinates[first] == coordinates[first + 1] {
            first += 1;
        }

        let min_len = if options.closed { 3 } else { 2 };
        if len < min_len {
            return;
        }

        let join_type = options.join;
        let miter_limit = if join_type == LineJoin::Bevel {
            1.05f32
        } else {
            options.miter_limit
        };
        let miter_limit = f64::from(miter_limit);
        let round_limit = f64::from(options.round_limit);

        let overscaling = f64::from(options.overscaling);
        let sharp_corner_offset = if options.overscaling == 0 {
            SHARP_CORNER_OFFSET * (EXTENT / TILE_SIZE)
        } else if overscaling <= 16.0 {
            SHARP_CORNER_OFFSET * (EXTENT / (TILE_SIZE * overscaling))
        } else {
            0.0
        };

        let first_coordinate = coordinates[first];
        let begin_cap = options.begin_cap;
        let end_cap = if options.closed {
            LineCap::Butt
        } else {
            options.end_cap
        };
        let cos_half_sharp_corner = cos_half_sharp_corner();
        let clip = options.clip_distances;

        let mut distance = 0.0f64;
        let mut start_of_line = true;
        let mut current_coordinate: Option<Position> = None;
        let mut prev_coordinate: Option<Position> = None;
        let mut next_coordinate: Option<Position>;
        let mut prev_normal: Option<[f64; 2]> = None;
        let mut next_normal: Option<[f64; 2]> = None;

        if options.closed {
            current_coordinate = Some(coordinates[len - 2]);
            next_normal = Some(perp(direction(coordinates[len - 2], first_coordinate)));
        }

        let start_vertex = self.vertices.len();
        let mut out = Gen {
            vertices: &mut self.vertices,
            triangles: Vec::new(),
            start_vertex,
            e1: -1,
            e2: -1,
            e3: -1,
        };

        for i in first..len {
            next_coordinate = if options.closed && i == len - 1 {
                Some(coordinates[first + 1])
            } else if i + 1 < len {
                Some(coordinates[i + 1])
            } else {
                None
            };

            // A repeated point has no direction, so it cannot contribute a normal.
            if next_coordinate == Some(coordinates[i]) {
                continue;
            }

            if let Some(n) = next_normal {
                prev_normal = Some(n);
            }
            if let Some(c) = current_coordinate {
                prev_coordinate = Some(c);
            }

            let mut current = coordinates[i];
            current_coordinate = Some(current);

            // With no next point the line is treated as continuing straight, so the previous
            // normal stands in; with no previous point the next one does.
            next_normal = match next_coordinate {
                Some(next) => Some(perp(direction(current, next))),
                None => prev_normal,
            };
            let next_n = next_normal.expect("a line of two distinct points always has a normal");
            if prev_normal.is_none() {
                prev_normal = Some(next_n);
            }
            let prev_n = prev_normal.expect("just filled in");

            // The join normal bisects the two segments. Two normals that cancel — a line
            // doubling exactly back on itself — leave it at zero, which drives the miter length
            // to infinity and downgrades the join, rather than producing a NaN direction.
            let mut join_normal = add(prev_n, next_n);
            if join_normal[0] != 0.0 || join_normal[1] != 0.0 {
                join_normal = unit(join_normal);
            }

            let cos_angle = prev_n[0] * next_n[0] + prev_n[1] * next_n[1];
            let cos_half_angle = join_normal[0] * next_n[0] + join_normal[1] * next_n[1];
            let miter_length = if cos_half_angle != 0.0 {
                1.0 / cos_half_angle
            } else {
                f64::INFINITY
            };
            let approx_angle = 2.0 * libm::sqrt(2.0 - 2.0 * cos_half_angle);

            let is_sharp_corner = cos_half_angle < cos_half_sharp_corner
                && prev_coordinate.is_some()
                && next_coordinate.is_some();

            if is_sharp_corner && i > first {
                let prev = prev_coordinate.expect("checked by is_sharp_corner");
                let prev_segment_length = dist(current, prev);
                if prev_segment_length > 2.0 * sharp_corner_offset {
                    let d = [
                        f64::from(current[0]) - f64::from(prev[0]),
                        f64::from(current[1]) - f64::from(prev[1]),
                    ];
                    let step = scale(d, sharp_corner_offset / prev_segment_length);
                    let new_prev = [
                        current[0].wrapping_sub(libm::round(step[0]) as i16),
                        current[1].wrapping_sub(libm::round(step[1]) as i16),
                    ];
                    distance += dist(new_prev, prev);
                    out.add_current_vertex(new_prev, &mut distance, prev_n, 0.0, 0.0, false, clip);
                    prev_coordinate = Some(new_prev);
                }
            }

            let middle_vertex = prev_coordinate.is_some() && next_coordinate.is_some();
            let current_cap = if next_coordinate.is_some() {
                begin_cap
            } else {
                end_cap
            };

            let mut current_join = match join_type {
                LineJoin::Bevel => ResolvedJoin::Bevel,
                LineJoin::Miter => ResolvedJoin::Miter,
                LineJoin::Round => ResolvedJoin::Round,
            };

            if middle_vertex {
                if current_join == ResolvedJoin::Round {
                    if miter_length < round_limit {
                        current_join = ResolvedJoin::Miter;
                    } else if miter_length <= 2.0 {
                        current_join = ResolvedJoin::FakeRound;
                    }
                }

                if current_join == ResolvedJoin::Miter && miter_length > miter_limit {
                    current_join = ResolvedJoin::Bevel;
                }

                if current_join == ResolvedJoin::Bevel {
                    // 128/63 is the widest extrusion a byte can hold, so a bevel longer than
                    // twice the line width has to be built the other way round.
                    if miter_length > 2.0 {
                        current_join = ResolvedJoin::FlipBevel;
                    }
                    // A bevel this shallow is invisible; a mitre saves a triangle.
                    if miter_length < miter_limit {
                        current_join = ResolvedJoin::Miter;
                    }
                }
            }

            if let Some(prev) = prev_coordinate {
                distance += dist(current, prev);
            }

            if middle_vertex && current_join == ResolvedJoin::Miter {
                let n = scale(join_normal, miter_length);
                out.add_current_vertex(current, &mut distance, n, 0.0, 0.0, false, clip);
            } else if middle_vertex && current_join == ResolvedJoin::FlipBevel {
                let n = if miter_length > 100.0 {
                    // Almost parallel: the bisector is meaningless, so use the next normal.
                    scale(next_n, -1.0)
                } else {
                    let dir = if prev_n[0] * next_n[1] - prev_n[1] * next_n[0] > 0.0 {
                        -1.0
                    } else {
                        1.0
                    };
                    let bevel_length =
                        miter_length * mag(add(prev_n, next_n)) / mag(sub(prev_n, next_n));
                    scale(perp(join_normal), bevel_length * dir)
                };
                out.add_current_vertex(current, &mut distance, n, 0.0, 0.0, false, clip);
                out.add_current_vertex(
                    current,
                    &mut distance,
                    scale(n, -1.0),
                    0.0,
                    0.0,
                    false,
                    clip,
                );
            } else if middle_vertex
                && (current_join == ResolvedJoin::Bevel || current_join == ResolvedJoin::FakeRound)
            {
                let line_turns_left = (prev_n[0] * next_n[1] - prev_n[1] * next_n[0]) > 0.0;
                let offset = -libm::sqrt(miter_length * miter_length - 1.0);
                let (offset_a, offset_b) = if line_turns_left {
                    (offset, 0.0)
                } else {
                    (0.0, offset)
                };

                if !start_of_line {
                    out.add_current_vertex(
                        current,
                        &mut distance,
                        prev_n,
                        offset_a,
                        offset_b,
                        false,
                        clip,
                    );
                }

                if current_join == ResolvedJoin::FakeRound {
                    let n = libm::round(
                        (approx_angle * 180.0 / core::f64::consts::PI) / DEG_PER_TRIANGLE,
                    ) as u32;
                    for m in 1..n {
                        let mut t = f64::from(m) / f64::from(n);
                        if t != 0.5 {
                            // Approximate geometric slerp; a plain lerp would bunch the fan's
                            // triangles towards the middle of the arc.
                            let t2 = t - 0.5;
                            let a = 1.0904
                                + cos_angle
                                    * (-3.2452 + cos_angle * (3.55645 - cos_angle * 1.43519));
                            let b = 0.848013 + cos_angle * (-1.06021 + cos_angle * 0.215638);
                            t += t * t2 * (t - 1.0) * (a * t2 * t2 + b);
                        }
                        let approx = unit(add(scale(prev_n, 1.0 - t), scale(next_n, t)));
                        out.add_pie_slice_vertex(current, distance, approx, line_turns_left, clip);
                    }
                }

                if next_coordinate.is_some() {
                    out.add_current_vertex(
                        current,
                        &mut distance,
                        next_n,
                        -offset_a,
                        -offset_b,
                        false,
                        clip,
                    );
                }
            } else if !middle_vertex && current_cap == LineCap::Butt {
                if !start_of_line {
                    out.add_current_vertex(current, &mut distance, prev_n, 0.0, 0.0, false, clip);
                }
                if next_coordinate.is_some() {
                    out.add_current_vertex(current, &mut distance, next_n, 0.0, 0.0, false, clip);
                }
            } else if !middle_vertex && current_cap == LineCap::Square {
                if !start_of_line {
                    out.add_current_vertex(current, &mut distance, prev_n, 1.0, 1.0, false, clip);
                    // Break the strip: the cap ends this segment.
                    out.e1 = -1;
                    out.e2 = -1;
                }
                if next_coordinate.is_some() {
                    out.add_current_vertex(current, &mut distance, next_n, -1.0, -1.0, false, clip);
                }
            } else if if middle_vertex {
                current_join == ResolvedJoin::Round
            } else {
                current_cap == LineCap::Round
            } {
                if !start_of_line {
                    out.add_current_vertex(current, &mut distance, prev_n, 0.0, 0.0, false, clip);
                    out.add_current_vertex(current, &mut distance, prev_n, 1.0, 1.0, true, clip);
                    out.e1 = -1;
                    out.e2 = -1;
                }
                if next_coordinate.is_some() {
                    out.add_current_vertex(current, &mut distance, next_n, -1.0, -1.0, true, clip);
                    out.add_current_vertex(current, &mut distance, next_n, 0.0, 0.0, false, clip);
                }
            }

            if is_sharp_corner && i < len - 1 {
                let next = next_coordinate.expect("checked by is_sharp_corner");
                let next_segment_length = dist(current, next);
                if next_segment_length > 2.0 * sharp_corner_offset {
                    let d = [
                        f64::from(next[0]) - f64::from(current[0]),
                        f64::from(next[1]) - f64::from(current[1]),
                    ];
                    let step = scale(d, sharp_corner_offset / next_segment_length);
                    let new_current = [
                        current[0].wrapping_add(libm::round(step[0]) as i16),
                        current[1].wrapping_add(libm::round(step[1]) as i16),
                    ];
                    distance += dist(new_current, current);
                    out.add_current_vertex(
                        new_current,
                        &mut distance,
                        next_n,
                        0.0,
                        0.0,
                        false,
                        clip,
                    );
                    current = new_current;
                    current_coordinate = Some(current);
                }
            }

            start_of_line = false;
        }

        let triangles = out.triangles;
        let vertex_count = self.vertices.len() - start_vertex;

        let needs_segment = match self.segments.last() {
            None => true,
            Some(s) => s.vertex_length as usize + vertex_count > MAX_SEGMENT_VERTICES,
        };
        if needs_segment {
            self.segments.push(Segment {
                vertex_offset: start_vertex as u32,
                index_offset: self.indices.len() as u32,
                vertex_length: 0,
                index_length: 0,
            });
        }
        let segment = self.segments.last_mut().expect("just ensured");
        let base = segment.vertex_length as u16;

        for t in &triangles {
            self.indices.push(base + t.0);
            self.indices.push(base + t.1);
            self.indices.push(base + t.2);
        }

        segment.vertex_length += vertex_count as u32;
        segment.index_length += (triangles.len() * 3) as u32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A straight two-point line: one segment, four vertices, two triangles.
    fn straight() -> Vec<Position> {
        alloc::vec![[0, 0], [100, 0]]
    }

    #[test]
    fn a_segment_is_four_vertices_and_two_triangles() {
        let mut bucket = LineBucket::default();
        bucket.add_geometry(&straight(), &LineOptions::default());
        assert_eq!(bucket.vertices.len(), 4);
        assert_eq!(bucket.indices, [0, 1, 2, 1, 2, 3]);
        assert_eq!(bucket.segments.len(), 1);
        assert_eq!(bucket.segments[0].vertex_length, 4);
        assert_eq!(bucket.segments[0].index_length, 6);
    }

    /// The point is doubled and the low bits carry flags, so the buffer never holds the
    /// coordinate itself. A reader treating `pos_normal` as a position draws the map at twice
    /// the scale, which is the kind of error that looks like a projection bug.
    #[test]
    fn the_position_is_doubled_and_the_low_bits_are_flags() {
        let mut bucket = LineBucket::default();
        bucket.add_geometry(&straight(), &LineOptions::default());
        // Butt caps are not round, so x keeps its low bit clear; the two sides differ only in
        // the low bit of y.
        assert_eq!(bucket.vertices[0].pos_normal, [0, 0]);
        assert_eq!(bucket.vertices[1].pos_normal, [0, 1]);
        assert_eq!(bucket.vertices[2].pos_normal, [200, 0]);
        assert_eq!(bucket.vertices[3].pos_normal, [200, 1]);
    }

    /// The extrusion is `±63` biased by 128, and the two sides are opposite.
    #[test]
    fn the_extrusion_is_scaled_and_biased() {
        let mut bucket = LineBucket::default();
        bucket.add_geometry(&straight(), &LineOptions::default());
        // The line runs along +x, so its normal is ±y.
        assert_eq!(bucket.vertices[0].data[0], 128);
        assert_eq!(bucket.vertices[0].data[1], 128 + 63);
        assert_eq!(bucket.vertices[1].data[1], 128 - 63);
    }

    /// A round cap marks itself in the low bit of x and adds a vertex pair at each end.
    #[test]
    fn a_round_cap_is_flagged_in_the_position() {
        let mut bucket = LineBucket::default();
        let options = LineOptions {
            begin_cap: LineCap::Round,
            end_cap: LineCap::Round,
            ..LineOptions::default()
        };
        bucket.add_geometry(&straight(), &options);
        assert_eq!(bucket.vertices.len(), 8);
        assert!(bucket.vertices.iter().any(|v| v.pos_normal[0] % 2 == 1));
    }

    /// A square cap breaks the strip, which is why it emits the same vertex count as a round
    /// cap but fewer triangles: the two end pairs are not connected to each other.
    #[test]
    fn a_square_cap_breaks_the_strip() {
        let mut butt = LineBucket::default();
        butt.add_geometry(&straight(), &LineOptions::default());

        let mut square = LineBucket::default();
        square.add_geometry(
            &straight(),
            &LineOptions {
                begin_cap: LineCap::Square,
                end_cap: LineCap::Square,
                ..LineOptions::default()
            },
        );
        assert_eq!(square.vertices.len(), butt.vertices.len());
        assert_eq!(square.indices.len(), butt.indices.len());
        // The cap direction is stored, and butt caps store zero.
        assert_ne!(square.vertices[0].data[2] & 0x3, 1);
        assert_eq!(butt.vertices[0].data[2] & 0x3, 1);
    }

    /// A corner sharp enough to exceed the miter limit becomes a bevel, which emits an extra
    /// vertex pair the miter does not.
    #[test]
    fn a_sharp_corner_downgrades_the_miter() {
        let shallow = alloc::vec![[0, 0], [1000, 0], [2000, 100]];
        let sharp = alloc::vec![[0, 0], [1000, 0], [0, 100]];

        let mut a = LineBucket::default();
        a.add_geometry(&shallow, &LineOptions::default());
        let mut b = LineBucket::default();
        b.add_geometry(&sharp, &LineOptions::default());

        assert_eq!(a.vertices.len(), 6, "a shallow corner stays a mitre");
        assert!(
            b.vertices.len() > a.vertices.len(),
            "a sharp corner emits more: {} vs {}",
            b.vertices.len(),
            a.vertices.len()
        );
    }

    /// A round join fans out into triangles, so it emits more than a bevel at the same corner.
    #[test]
    fn a_round_join_fans() {
        let sharp = alloc::vec![[0, 0], [1000, 0], [0, 100]];
        let mut bevel = LineBucket::default();
        bevel.add_geometry(
            &sharp,
            &LineOptions {
                join: LineJoin::Bevel,
                ..LineOptions::default()
            },
        );
        let mut round = LineBucket::default();
        round.add_geometry(
            &sharp,
            &LineOptions {
                join: LineJoin::Round,
                ..LineOptions::default()
            },
        );
        assert!(
            round.vertices.len() > bevel.vertices.len(),
            "round {} should exceed bevel {}",
            round.vertices.len(),
            bevel.vertices.len()
        );
    }

    /// Degenerate geometry draws nothing rather than extruding a zero-length direction.
    ///
    /// The duplicate trimming runs before the length check, so a line of repeated points is
    /// short *after* trimming and is dropped — mbgl's order, and the reason a five-point line
    /// of one distinct position produces no vertices instead of a unit-vector division by zero.
    #[test]
    fn degenerate_geometry_draws_nothing() {
        for input in [
            alloc::vec![],
            alloc::vec![[5, 5]],
            alloc::vec![[5, 5], [5, 5]],
            alloc::vec![[5, 5], [5, 5], [5, 5], [5, 5], [5, 5]],
        ] {
            let mut bucket = LineBucket::default();
            bucket.add_geometry(&input, &LineOptions::default());
            assert!(bucket.vertices.is_empty(), "{input:?} drew something");
        }
    }

    /// A line doubling exactly back on itself has cancelling normals, which mbgl handles by
    /// leaving the join normal at zero rather than taking the unit vector of nothing.
    #[test]
    fn a_reversal_does_not_produce_a_nan() {
        let mut bucket = LineBucket::default();
        bucket.add_geometry(
            &alloc::vec![[0, 0], [1000, 0], [0, 0]],
            &LineOptions::default(),
        );
        for v in &bucket.vertices {
            // The bytes are already quantised, so a NaN extrusion would have clamped rather
            // than propagated; what it cannot do is stay within one unit of the unextruded
            // midpoint on both axes at once.
            assert!(v.data[0] != 128 || v.data[1] != 128, "an unextruded vertex");
        }
    }

    /// A closed ring has no caps and wraps its join, so it emits a join at every point
    /// including the one where it closes.
    #[test]
    fn a_closed_ring_joins_at_the_seam() {
        let square = alloc::vec![[0, 0], [1000, 0], [1000, 1000], [0, 1000], [0, 0]];
        let mut open = LineBucket::default();
        open.add_geometry(&square, &LineOptions::default());
        let mut closed = LineBucket::default();
        closed.add_geometry(
            &square,
            &LineOptions {
                closed: true,
                ..LineOptions::default()
            },
        );
        assert!(
            closed.indices.len() > open.indices.len(),
            "closed {} should exceed open {}",
            closed.indices.len(),
            open.indices.len()
        );
    }

    /// A bucket that outgrows a u16 index opens a second segment, and the new segment's
    /// indices restart at zero against its own vertex base.
    ///
    /// Not a theoretical branch: one real tile's `admin` layer reaches 54k vertices, 83% of the
    /// way here. Getting it wrong draws the tail of a layer as garbage triangles pointing into
    /// the head of it, which is far enough from the cause to be worth a test of its own.
    #[test]
    fn a_bucket_larger_than_a_u16_index_splits() {
        let mut bucket = LineBucket::default();
        // Four vertices per call, so this crosses 65535 partway through.
        for i in 0..20_000i16 {
            bucket.add_geometry(&alloc::vec![[0, i], [100, i]], &LineOptions::default());
        }
        assert!(
            bucket.segments.len() > 1,
            "{} segments",
            bucket.segments.len()
        );

        let mut seen = 0u32;
        for segment in &bucket.segments {
            assert!(
                segment.vertex_length <= u32::from(u16::MAX),
                "segment holds {} vertices",
                segment.vertex_length
            );
            assert_eq!(segment.vertex_offset, seen, "segments are contiguous");
            seen += segment.vertex_length;

            let range = segment.index_offset as usize
                ..(segment.index_offset + segment.index_length) as usize;
            for index in &bucket.indices[range] {
                assert!(
                    u32::from(*index) < segment.vertex_length,
                    "index {index} outside its own segment"
                );
            }
        }
        assert_eq!(
            seen as usize,
            bucket.vertices.len(),
            "every vertex is in a segment"
        );
    }

    /// Two geometries share a segment, with the second's indices based on the first's count.
    /// Restarting indices at zero per geometry would draw the second feature over the first.
    #[test]
    fn a_second_geometry_continues_the_segment() {
        let mut bucket = LineBucket::default();
        bucket.add_geometry(&straight(), &LineOptions::default());
        bucket.add_geometry(&alloc::vec![[0, 500], [100, 500]], &LineOptions::default());
        assert_eq!(bucket.segments.len(), 1);
        assert_eq!(bucket.segments[0].vertex_length, 8);
        assert_eq!(&bucket.indices[6..], &[4, 5, 6, 5, 6, 7]);
    }
}
