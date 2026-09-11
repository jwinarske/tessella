//! A fill's outline: the same vertices, a different index buffer, a different shader.
//!
//! # What the oracle says
//!
//! `tests/golden/hermetic_style.dump` gives a fill layer two drawables — sub-layer 1 with
//! `sh0011` (`FillShader`) and six indices, sub-layer 2 with `sh0012` (`FillOutlineShader`) and
//! ten — over the same five vertices. So the outline is not the fill drawn twice: it shares the
//! vertex buffer and brings its own indices and its own shader.
//!
//! Five vertices and ten indices is the rule stated: `addOutlineIndices` emits one line segment
//! per vertex, two indices each. One more segment than there are gaps between the points,
//! because the ring closes.

use tessella_layout::fill;

fn ring(points: &[(i16, i16)]) -> Vec<[i16; 2]> {
    points.iter().map(|(x, y)| [*x, *y]).collect()
}

/// A square, closed the way a clipped fill arrives: five points, the last repeating the first.
fn square() -> Vec<[i16; 2]> {
    ring(&[(0, 0), (0, 16), (16, 16), (16, 0), (0, 0)])
}

/// The counts the golden gives: five vertices, six triangle indices, ten line indices.
#[test]
fn a_square_outlines_as_the_oracle_does() {
    let bucket = fill::build(&[square()]);
    assert_eq!(bucket.vertices.len(), 5, "the closing point is kept");
    assert_eq!(bucket.indices.len(), 6, "two triangles");
    assert_eq!(
        bucket.line_indices.len(),
        10,
        "one segment per vertex, two indices each"
    );
}

/// The closing segment comes first, which is the detail a natural implementation gets wrong.
///
/// mbgl emits `(count - 1, 0)` before walking the consecutive pairs. Appending it last draws
/// the same outline and produces a different buffer, so nothing but a comparison catches it.
#[test]
fn the_closing_segment_is_emitted_first() {
    let bucket = fill::build(&[square()]);
    assert_eq!(
        &bucket.line_indices[0..2],
        &[4, 0],
        "the first pair wraps from the last vertex to the first"
    );
    assert_eq!(
        &bucket.line_indices[2..],
        &[0, 1, 1, 2, 2, 3, 3, 4],
        "then the consecutive pairs, in order"
    );
}

/// The outline's segment spans the same vertices as the fill's.
///
/// That is what lets one vertex buffer serve both drawables. The index ranges differ and the
/// vertex ranges do not.
#[test]
fn the_outline_shares_the_fill_vertices() {
    let bucket = fill::build(&[square()]);
    let fill_segment = bucket.segments.first().expect("a fill segment");
    let line_segment = bucket.line_segments.first().expect("a line segment");

    assert_eq!(line_segment.vertex_offset, fill_segment.vertex_offset);
    assert_eq!(line_segment.vertex_length, fill_segment.vertex_length);
    assert_eq!(line_segment.index_offset, 0);
    assert_eq!(line_segment.index_length, 10);
}

/// A polygon with a hole outlines both rings, each closing on itself.
///
/// The rings share a vertex buffer and a segment, so the second ring's indices are offset by the
/// first's length — and its own closing segment wraps within the ring rather than back to the
/// polygon's first point.
#[test]
fn a_hole_gets_its_own_loop() {
    let outer = ring(&[(0, 0), (0, 32), (32, 32), (32, 0), (0, 0)]);
    let hole = ring(&[(8, 8), (24, 8), (24, 24), (8, 24), (8, 8)]);
    let bucket = fill::build(&[outer, hole]);

    assert_eq!(bucket.vertices.len(), 10);
    assert_eq!(bucket.line_indices.len(), 20, "five segments per ring");

    // The outer ring's loop, then the hole's, each wrapping inside itself.
    assert_eq!(&bucket.line_indices[0..2], &[4, 0], "outer wraps 4 -> 0");
    assert_eq!(
        &bucket.line_indices[10..12],
        &[9, 5],
        "the hole wraps 9 -> 5"
    );
}

/// An empty ring contributes no outline.
#[test]
fn nothing_outlines_nothing() {
    let bucket = fill::build(&[Vec::new()]);
    assert!(bucket.line_indices.is_empty());
    assert!(bucket.line_segments.is_empty());
}

/// And the same outline as a polyline, for a backend that cannot widen a line.
///
/// mbgl's `MLN_TRIANGULATE_FILL_OUTLINES` path: `generateFillAndOutineBuffers` runs the ring
/// through `PolylineGenerator` with `type = FeatureType::Polygon`, which closes it and forces a
/// butt end cap. Two vertices a ring point rather than one, so this is a second buffer over the
/// same geometry rather than a second index list into the first.
#[test]
fn a_square_also_outlines_as_a_polyline() {
    let bucket = fill::build(&[square()]);
    assert!(
        bucket.outline.vertices.len() > bucket.vertices.len(),
        "a polyline has two vertices a point, not one: {} against {}",
        bucket.outline.vertices.len(),
        bucket.vertices.len()
    );
    assert!(
        !bucket.outline.indices.is_empty(),
        "triangles, not line pairs"
    );
    assert_eq!(
        bucket.outline.indices.len() % 3,
        0,
        "triangles come in threes"
    );
}

/// And is not built at all where the layer will draw the line-primitive outline instead.
///
/// The flag is the caller's: a layer whose outline colour or opacity varies per feature takes
/// `FillOutlineShader` over the fill's own vertices, and the polyline would be geometry nothing
/// draws. mbgl builds both because a bucket there serves every layer over one source layer.
#[test]
fn a_polyline_outline_is_built_only_when_asked_for() {
    let rings = [square()];
    let features: &[&[Vec<[i16; 2]>]] = &[&rings];
    let both = fill::Outlines::default();
    let lines_only = fill::Outlines {
        polyline: false,
        ..both
    };
    let (with, _) = fill::build_features_tracked_on(features, 0, both);
    let (without, _) = fill::build_features_tracked_on(features, 0, lines_only);
    assert!(!with.outline.vertices.is_empty());
    assert!(without.outline.vertices.is_empty(), "not built");
    assert_eq!(
        with.vertices, without.vertices,
        "the fill itself is the same either way"
    );
    assert_eq!(with.line_indices, without.line_indices);
}

/// A layer that draws no outline carries neither form.
///
/// mbgl's `doOutline`: `fill-antialias` false, or a patterned fill whose outline colour the
/// style wrote. The fill itself is untouched -- what goes is the second drawable.
#[test]
fn a_layer_with_no_outline_builds_neither_form() {
    let rings = [square()];
    let features: &[&[Vec<[i16; 2]>]] = &[&rings];
    let (bucket, _) = fill::build_features_tracked_on(
        features,
        0,
        fill::Outlines {
            lines: false,
            polyline: false,
        },
    );
    assert_eq!(bucket.vertices.len(), 5, "the fill is still built");
    assert_eq!(bucket.indices.len(), 6);
    assert!(bucket.line_indices.is_empty(), "no line indices");
    assert!(bucket.line_segments.is_empty(), "and no segments for them");
    assert!(bucket.outline.vertices.is_empty(), "and no polyline");
}
