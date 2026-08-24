//! Circle buckets: points in, one quad each.
//!
//! Transcribed from mbgl's `CircleLayout::addCircle` (`layout/circle_layout.hpp`) and
//! `CircleBucket::vertex` (`renderer/buckets/circle_bucket.hpp`).
//!
//! # The quad is a unit square, not a circle
//!
//! Four vertices and two triangles per point. The circle itself is drawn by the fragment
//! shader inside that quad, which is why `circle-radius` never reaches the geometry: the
//! vertices carry the centre and a corner sign, and the shader scales by the radius uniform (or
//! the radius attribute, when it is data-driven). One bucket therefore serves every radius the
//! style can produce, at every zoom.
//!
//! The corner sign rides in the low bit of the position, the same trick the line vertex uses:
//! the point is doubled and `(sign + 1) / 2` — zero or one — is added. So a vertex is again not
//! a position, and a reader treating it as one draws the map at twice the scale.
//!
//! # Points outside the tile are dropped, and the buffer is not consulted
//!
//! `addCircle` drops any point outside `0..EXTENT` — the tile *proper*, not the buffered box
//! every other layer type clips to. That is deliberate in mbgl and load-bearing here: the
//! hermetic style's single point falls inside four tiles' buffered boxes and is drawn in
//! exactly one, which is why the oracle has one circle drawable rather than four. A layer that
//! kept the buffer would draw the same circle up to four times, each slightly offset, and the
//! overdraw would be invisible against an opaque fill.
//!
//! mbgl skips the check in `Still` mode, where a neighbouring tile's points are wanted so a
//! snapshot is not clipped at its edges. This build is continuous-only, so the check always
//! runs; the mode is named here rather than silently assumed.

use alloc::vec::Vec;

use crate::fill::{Position, Segment};

/// mbgl's tile extent. A point at or past this belongs to the next tile.
const EXTENT: i32 = 8192;

/// Largest vertex index a segment can address.
const MAX_SEGMENT_VERTICES: usize = u16::MAX as usize;

/// A built circle bucket.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CircleBucket {
    /// Four vertices per point: the centre doubled, plus a corner bit.
    pub vertices: Vec<Position>,
    /// Two triangles per point.
    pub indices: Vec<u16>,
    /// Draw segments.
    pub segments: Vec<Segment>,
}

/// One corner of the quad.
///
/// mbgl passes these as floats and computes `(sign + 1) / 2` in float before narrowing, which
/// for `±1` is exactly zero or one. Kept as the sign rather than the bit so the correspondence
/// with the C++ is visible at the call site.
const CORNERS: [(i16, i16); 4] = [(-1, -1), (1, -1), (1, 1), (-1, 1)];

impl CircleBucket {
    /// Adds one feature's points.
    ///
    /// Points outside the tile proper contribute nothing — see the module note on why the
    /// buffer is not consulted here.
    pub fn add_geometry(&mut self, points: &[Position]) {
        for point in points {
            let (x, y) = (i32::from(point[0]), i32::from(point[1]));
            if !(0..EXTENT).contains(&x) || !(0..EXTENT).contains(&y) {
                continue;
            }

            let needs_segment = match self.segments.last() {
                None => true,
                Some(segment) => segment.vertex_length as usize + 4 > MAX_SEGMENT_VERTICES,
            };
            if needs_segment {
                #[allow(clippy::cast_possible_truncation)]
                self.segments.push(Segment {
                    vertex_offset: self.vertices.len() as u32,
                    index_offset: self.indices.len() as u32,
                    vertex_length: 0,
                    index_length: 0,
                });
            }

            let segment = self.segments.last_mut().expect("just ensured");
            #[allow(clippy::cast_possible_truncation)]
            let base = segment.vertex_length as u16;

            for (ex, ey) in CORNERS {
                #[allow(clippy::cast_possible_truncation)]
                self.vertices.push([
                    ((x * 2) + i32::from((ex + 1) / 2)) as i16,
                    ((y * 2) + i32::from((ey + 1) / 2)) as i16,
                ]);
            }

            // 1,2,3 then 1,4,3 — the second triangle reuses the first and third corners, which
            // is why the winding of the pair is not what a naive 0,1,2 / 0,2,3 quad gives.
            self.indices
                .extend_from_slice(&[base, base + 1, base + 2, base, base + 3, base + 2]);

            segment.vertex_length += 4;
            segment.index_length += 6;
        }
    }
}

/// Builds a bucket from one feature's points.
#[must_use]
pub fn build(points: &[Position]) -> CircleBucket {
    let mut bucket = CircleBucket::default();
    bucket.add_geometry(points);
    bucket
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_point_is_four_vertices_and_two_triangles() {
        let bucket = build(&alloc::vec![[7799, 1121]]);
        // The oracle's circle drawable, tile 13/4093/2724.
        assert_eq!(
            bucket.vertices,
            [[15598, 2242], [15599, 2242], [15599, 2243], [15598, 2243]]
        );
        assert_eq!(bucket.indices, [0, 1, 2, 0, 3, 2]);
        assert_eq!(bucket.segments.len(), 1);
        assert_eq!(bucket.segments[0].vertex_length, 4);
        assert_eq!(bucket.segments[0].index_length, 6);
    }

    /// A point outside the tile proper draws nothing, even though a fill or line would keep it.
    ///
    /// The buffered box reaches `-2048..10240`; this is the one layer type that does not use it.
    #[test]
    fn a_point_outside_the_tile_is_dropped() {
        for outside in [
            [-1, 100],
            [100, -1],
            [8192, 100],
            [100, 8192],
            [-2048, -2048],
        ] {
            let bucket = build(&alloc::vec![outside]);
            assert!(bucket.vertices.is_empty(), "{outside:?} drew something");
            assert!(bucket.segments.is_empty(), "and opened no segment");
        }
        // The bounds are half-open: zero is inside, the extent is not.
        assert_eq!(build(&alloc::vec![[0, 0]]).vertices.len(), 4);
        assert_eq!(build(&alloc::vec![[8191, 8191]]).vertices.len(), 4);
    }

    /// Several points share a segment, each indexing from its own base.
    #[test]
    fn points_accumulate_into_one_segment() {
        let bucket = build(&alloc::vec![[10, 10], [20, 20], [30, 30]]);
        assert_eq!(bucket.vertices.len(), 12);
        assert_eq!(bucket.segments.len(), 1);
        assert_eq!(bucket.segments[0].vertex_length, 12);
        assert_eq!(&bucket.indices[6..12], &[4, 5, 6, 4, 7, 6]);
        assert_eq!(&bucket.indices[12..], &[8, 9, 10, 8, 11, 10]);
    }

    /// A bucket larger than a u16 index opens a second segment.
    #[test]
    fn a_bucket_larger_than_a_u16_index_splits() {
        let points: Vec<Position> = (0..20_000i16).map(|i| [i % 8000, i % 8000]).collect();
        let bucket = build(&points);
        assert!(
            bucket.segments.len() > 1,
            "{} segments",
            bucket.segments.len()
        );

        let mut seen = 0u32;
        for segment in &bucket.segments {
            assert!(segment.vertex_length <= u32::from(u16::MAX));
            assert_eq!(segment.vertex_offset, seen, "segments are contiguous");
            seen += segment.vertex_length;
            let range = segment.index_offset as usize
                ..(segment.index_offset + segment.index_length) as usize;
            for index in &bucket.indices[range] {
                assert!(
                    u32::from(*index) < segment.vertex_length,
                    "index {index} out of range"
                );
            }
        }
        assert_eq!(seen as usize, bucket.vertices.len());
    }
}
