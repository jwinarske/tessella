//! Where a label repeats along a line, against mbgl's own expectations.
//!
//! mbgl's `getAnchors` tests state the exact anchors — position, angle and segment index — for
//! six arrangements. Segment indices are what make them worth transcribing: two anchors at the
//! same place on different segments are different anchors, and the index is what the collision
//! walk and the shader's along-line projection both key off.

use tessella_layout::anchors::{Anchor, EXTENT, angle_window_size, get_anchors, line_length};

/// mbgl's fixture line: ten points straight up, starting at `shift`.
fn line(shift: i32) -> Vec<(f32, f32)> {
    #[allow(clippy::cast_precision_loss)]
    (shift..shift + 10)
        .map(|index| (1.0, index as f32))
        .collect()
}

/// A line starting at y = 0 touches the tile edge, so it reads as continued from the next tile.
fn continued() -> Vec<(f32, f32)> {
    line(0)
}

fn non_continued() -> Vec<(f32, f32)> {
    line(1)
}

const SMALL_SPACING: f32 = 2.0;
const BIG_SPACING: f32 = 3.0;
const TEXT_LEFT: f32 = -1.0;
const TEXT_RIGHT: f32 = 1.0;
const ICON_LEFT: f32 = -0.5;
const ICON_RIGHT: f32 = 0.5;
const GLYPH_SIZE: f32 = 0.1;

fn anchors(line: &[(f32, f32)], spacing: f32, box_scale: f32, overscaling: f32) -> Vec<Anchor> {
    get_anchors(
        line,
        spacing,
        core::f32::consts::PI,
        TEXT_LEFT,
        TEXT_RIGHT,
        ICON_LEFT,
        ICON_RIGHT,
        GLYPH_SIZE,
        box_scale,
        overscaling,
    )
}

/// The angle a straight upward line has.
///
/// mbgl's expectations spell it `1.570796371f`, which is its printed form of pi over two and is
/// the same `f32` — checked, not assumed. The constant says what it means.
fn at(x: f32, y: f32, segment: usize) -> Anchor {
    Anchor {
        point: (x, y),
        angle: core::f32::consts::FRAC_PI_2,
        segment,
    }
}

/// mbgl `getAnchors.NonContinuedLineShortLabels`.
#[test]
fn a_non_continued_line_with_short_labels() {
    assert_eq!(
        anchors(&non_continued(), BIG_SPACING, 1.0, 1.0),
        [at(1.0, 2.0, 1), at(1.0, 5.0, 4), at(1.0, 8.0, 7)]
    );
}

/// mbgl `getAnchors.NonContinuedLineLongLabels`.
///
/// The same line at a tighter spacing. The anchors are *not* simply closer together: the spacing
/// is widened because the label is long relative to it, which is why the segments are 1, 3, 6
/// rather than an even step.
#[test]
fn a_non_continued_line_with_long_labels() {
    assert_eq!(
        anchors(&non_continued(), SMALL_SPACING, 1.0, 1.0),
        [at(1.0, 2.0, 1), at(1.0, 5.0, 3), at(1.0, 7.0, 6)]
    );
}

/// mbgl `getAnchors.ContinuedLineShortLabels`.
#[test]
fn a_continued_line_with_short_labels() {
    assert_eq!(
        anchors(&continued(), BIG_SPACING, 1.0, 1.0),
        [at(1.0, 2.0, 1), at(1.0, 5.0, 4), at(1.0, 8.0, 7)]
    );
}

/// mbgl `getAnchors.ContinuedLineLongLabels`.
///
/// A continued line starts half a spacing in rather than half a label, so its anchors sit a
/// unit earlier than the non-continued case. That is what makes two adjacent tiles' labels
/// interleave instead of doubling up at the seam.
#[test]
fn a_continued_line_with_long_labels() {
    assert_eq!(
        anchors(&continued(), SMALL_SPACING, 1.0, 1.0),
        [at(1.0, 1.0, 1), at(1.0, 4.0, 3), at(1.0, 6.0, 6)]
    );
}

/// mbgl `getAnchors.OverscaledAnchorsInParent`.
///
/// A tile drawn at twice its own zoom must put labels where its parent did, or every label on
/// the map jumps as a zoom crossing swaps one for the other. The child's anchors are a superset
/// of the parent's, which is what the offset's `overscaling` term buys.
#[test]
fn an_overscaled_tiles_anchors_contain_its_parents() {
    let parent = anchors(&non_continued(), BIG_SPACING, 1.0, 1.0);
    let child = anchors(&non_continued(), BIG_SPACING / 2.0, 0.5, 2.0);

    assert!(!parent.is_empty());
    for anchor in &parent {
        assert!(
            child.contains(anchor),
            "the child lost the parent's anchor {anchor:?}: {child:?}"
        );
    }
}

/// mbgl `getAnchors.UseMidpointForShortLine`.
///
/// Nothing fits at the computed offset, so the second attempt places one anchor at the middle.
/// Without it a short line in an overscaled tile gets no label at all — the offset there is
/// chosen to line up with the parent, not to fit the label as early as possible.
#[test]
fn a_short_line_falls_back_to_its_midpoint() {
    let short = [(1.0, 1.0), (1.0, 3.0)];
    assert_eq!(anchors(&short, SMALL_SPACING, 1.0, 1.0), [at(1.0, 2.0, 0)]);
}

/// An empty line has no anchors, and does not panic reaching for its first point.
#[test]
fn an_empty_line_has_no_anchors() {
    assert!(anchors(&[], BIG_SPACING, 1.0, 1.0).is_empty());
}

/// Anchors outside the tile are dropped.
///
/// A line runs past the tile's edge into its neighbour, and the neighbour draws that part. Two
/// tiles both labelling the overlap would draw the name twice, a fraction of a pixel apart.
#[test]
fn anchors_outside_the_tile_are_dropped() {
    // A line running from inside the tile out past its right edge.
    let crossing = [(EXTENT - 40.0, 10.0), (EXTENT + 400.0, 10.0)];
    for anchor in anchors(&crossing, 8.0, 1.0, 1.0) {
        assert!(anchor.point.0 < EXTENT, "{anchor:?} is outside the tile");
    }
}

/// A label that does not fit between the line's ends is not placed there.
#[test]
fn a_label_longer_than_its_line_is_not_placed() {
    let short = [(100.0, 100.0), (104.0, 100.0)];
    let long = get_anchors(
        &short,
        4.0,
        core::f32::consts::PI,
        -50.0,
        50.0,
        0.0,
        0.0,
        0.1,
        1.0,
        1.0,
    );
    assert!(
        long.is_empty(),
        "a hundred-unit label on a four-unit line: {long:?}"
    );
}

/// A line that bends too sharply refuses the label.
///
/// `text-max-angle`, and the reason a name vanishes from a hairpin rather than wrapping around
/// it. The same line at a generous angle keeps its anchors.
#[test]
fn a_sharp_bend_refuses_the_label() {
    // A right angle in the middle of an otherwise straight run.
    let bend: Vec<(f32, f32)> = (0..6)
        .map(|index| {
            #[allow(clippy::cast_precision_loss)]
            if index < 3 {
                (100.0, 100.0 + index as f32 * 4.0)
            } else {
                (100.0 + (index - 2) as f32 * 4.0, 108.0)
            }
        })
        .collect();

    let generous = get_anchors(
        &bend,
        4.0,
        core::f32::consts::PI,
        -6.0,
        6.0,
        0.0,
        0.0,
        1.0,
        1.0,
        1.0,
    );
    let strict = get_anchors(
        &bend,
        4.0,
        core::f32::consts::PI / 8.0,
        -6.0,
        6.0,
        0.0,
        0.0,
        1.0,
        1.0,
        1.0,
    );

    assert!(!generous.is_empty(), "a straight-enough limit keeps them");
    assert!(
        strict.len() < generous.len(),
        "a strict limit should drop some: {} vs {}",
        strict.len(),
        generous.len()
    );
}

/// The angle window is zero for a label with no horizontal extent.
///
/// A label that cannot bend cannot fail the bend check, and mbgl skips it entirely rather than
/// running a walk that can only pass.
#[test]
fn a_label_with_no_width_skips_the_angle_check() {
    assert_eq!(angle_window_size(0.0, 0.0, 0.1, 1.0), 0.0);
    assert!(angle_window_size(-1.0, 1.0, 0.1, 1.0) > 0.0);
}

/// A line's length is the sum of its segments.
#[test]
fn a_lines_length_is_its_segments() {
    assert_eq!(line_length(&non_continued()), 9.0);
    assert_eq!(line_length(&[(0.0, 0.0), (3.0, 4.0)]), 5.0);
    assert_eq!(line_length(&[]), 0.0);
}

/// mbgl `getAnchors.GetCenterAnchor`.
#[test]
fn the_centre_anchor_matches_mbgl() {
    let line = [(1.0, 1.0), (1.0, 3.0), (3.0, 6.0), (4.0, 7.0)];
    let anchor = centre(&line, core::f32::consts::PI).expect("a centre");

    assert_eq!(anchor.point, (2.0, 4.0));
    assert!(
        (anchor.angle - 0.982_793_8).abs() < 1e-6,
        "{}",
        anchor.angle
    );
    assert_eq!(anchor.segment, 1);
}

/// mbgl `getAnchors.GetCenterAnchorOutsideTileBounds`.
///
/// A centred label belongs to its feature rather than to a position, so a line whose middle
/// falls outside the tile still gets one. That is the opposite of the repeating case, where an
/// anchor outside the tile is dropped because the neighbouring tile will draw it.
#[test]
fn a_centre_outside_the_tile_is_still_placed() {
    let line = [(-10.0, -10.0), (5.0, 5.0)];
    let anchor = centre(&line, core::f32::consts::PI).expect("a centre");

    assert_eq!(anchor.point, (-3.0, -3.0));
    assert!((anchor.angle - core::f32::consts::FRAC_PI_4).abs() < 1e-6);
    assert_eq!(anchor.segment, 0);
}

/// mbgl `getAnchors.GetCenterAnchorFailMaxAngle`.
///
/// A right angle at the middle refuses the label outright rather than sliding it along. The
/// caller asked for the centre; answering with somewhere else would silently answer a different
/// question.
#[test]
fn a_bend_at_the_centre_refuses_it() {
    let line = [(1.0, 1.0), (1.0, 3.0), (3.0, 3.0)];
    assert!(centre(&line, core::f32::consts::PI / 4.0).is_none());
}

/// An empty line has no centre.
#[test]
fn an_empty_line_has_no_centre() {
    assert!(centre(&[], core::f32::consts::PI).is_none());
}

fn centre(line: &[(f32, f32)], max_angle: f32) -> Option<tessella_layout::anchors::Anchor> {
    tessella_layout::anchors::get_center_anchor(
        line, max_angle, TEXT_LEFT, TEXT_RIGHT, ICON_LEFT, ICON_RIGHT, GLYPH_SIZE, 1.0,
    )
}

/// How far each vertex is from the anchor along the line — mbgl's `calculateTileDistances`.
///
/// Its three expectations, verbatim, plus the property they imply. What reads these wants a
/// *reach* in each direction rather than a signed position, which is why a vertex two steps
/// before the anchor and one two steps after both read two.
mod tile_distances {
    use tessella_layout::anchors::{Anchor, calculate_tile_distances};

    fn line(points: &[(i32, i32)]) -> Vec<(f32, f32)> {
        points
            .iter()
            .map(|(x, y)| {
                #[allow(clippy::cast_precision_loss)]
                (*x as f32, *y as f32)
            })
            .collect()
    }

    /// mbgl `calculateTileDistances.Line`.
    #[test]
    fn the_distances_run_out_from_the_anchor_in_both_directions() {
        let line = line(&[(1, 1), (1, 2), (1, 3), (1, 4)]);
        let anchor = Anchor {
            point: (1.0, 3.0),
            angle: 0.0,
            segment: 2,
        };
        assert_eq!(
            calculate_tile_distances(&line, &anchor),
            vec![2.0, 1.0, 0.0, 1.0]
        );
    }

    /// mbgl `calculateTileDistances.Point`: one vertex is zero from itself.
    #[test]
    fn a_single_point_is_no_distance_from_itself() {
        let line = line(&[(1, 1)]);
        let anchor = Anchor {
            point: (1.0, 1.0),
            angle: 0.0,
            segment: 0,
        };
        assert_eq!(calculate_tile_distances(&line, &anchor), vec![0.0]);
    }

    /// mbgl `calculateTileDistances.EmptySegment`: an empty line has no distances.
    #[test]
    fn an_empty_line_has_no_distances() {
        assert!(
            calculate_tile_distances(
                &[],
                &Anchor {
                    point: (1.0, 1.0),
                    angle: 0.0,
                    segment: 0,
                }
            )
            .is_empty()
        );
    }

    /// Every distance is positive, and rises with the remove from the anchor.
    ///
    /// The property the three cases above imply and none of them alone pins: they are reaches
    /// rather than positions, so the run is a valley with its floor at the anchor's segment. A
    /// signed version would read as monotonic and place the prefix on one side only.
    #[test]
    fn the_distances_are_a_valley_around_the_anchor() {
        let line = line(&[(0, 0), (10, 0), (20, 0), (30, 0), (40, 0), (50, 0)]);
        let anchor = Anchor {
            point: (25.0, 0.0),
            angle: 0.0,
            segment: 2,
        };
        let distances = calculate_tile_distances(&line, &anchor);

        assert!(distances.iter().all(|value| *value >= 0.0), "{distances:?}");
        // Falling to the anchor's segment, then rising away from it.
        assert_eq!(distances, vec![25.0, 15.0, 5.0, 5.0, 15.0, 25.0]);
    }

    /// An anchor naming a segment the line does not have is zero throughout.
    ///
    /// Not a panic. The anchors and the line reach this from different places — a line can be
    /// merged or clipped after an anchor was chosen — and indexing on trust is how that becomes
    /// a crash on a worker rather than a label in the wrong place.
    #[test]
    fn a_segment_past_the_line_is_no_distance_at_all() {
        let line = line(&[(0, 0), (10, 0)]);
        let anchor = Anchor {
            point: (5.0, 0.0),
            angle: 0.0,
            segment: 99,
        };
        assert_eq!(calculate_tile_distances(&line, &anchor), vec![0.0, 0.0]);
    }
}
