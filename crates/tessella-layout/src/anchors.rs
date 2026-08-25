//! Where along a line a label can sit.
//!
//! A transcription of mbgl's `getAnchors` and `checkMaxAngle`. A road name does not sit at a
//! point — it is repeated along the road, bending with it — and this decides where each
//! repetition goes.
//!
//! # Three things have to be true at once
//!
//! A candidate position is kept only if the whole label fits on the line, if it lies inside the
//! tile, and if the line does not bend too sharply under it. The last is what `text-max-angle`
//! controls, and it is why a label vanishes from a hairpin rather than wrapping around it.
//!
//! # The spacing is not the spacing you asked for
//!
//! If a label is long relative to `symbol-spacing`, the spacing is widened so that a quarter of
//! it always remains as a gap between label *edges*. Without that, a long name at a short
//! spacing produces labels that overlap each other along the line, which collision then throws
//! most of away — work done to be discarded.
//!
//! # And the first one is deliberately not at the start
//!
//! A line continued from outside the tile starts half a spacing in, so that the labels of two
//! adjacent tiles interleave rather than doubling up at the seam. A line that begins inside the
//! tile starts half a label plus two glyph widths in, which mbgl's comment attributes to
//! avoiding collisions at T intersections — a name printed right at a junction collides with
//! the crossing road's.

use alloc::vec::Vec;

/// The tile extent anchors live in.
pub const EXTENT: f32 = 8192.0;

/// A place a label can sit on a line.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Anchor {
    /// Where, in tile units.
    pub point: (f32, f32),
    /// Which way the line runs there, in radians.
    pub angle: f32,
    /// Which segment of the line it falls on.
    pub segment: usize,
}

/// The angle between two points, as mbgl measures it.
fn angle_to(a: (f32, f32), b: (f32, f32)) -> f32 {
    (a.1 - b.1).atan2(a.0 - b.0)
}

fn distance(a: (f32, f32), b: (f32, f32)) -> f32 {
    (a.0 - b.0).hypot(a.1 - b.1)
}

/// How far along a line runs.
#[must_use]
pub fn line_length(line: &[(f32, f32)]) -> f32 {
    line.windows(2).map(|pair| distance(pair[0], pair[1])).sum()
}

/// How much of the line the angle check looks at.
///
/// Zero for a label with no horizontal extent, which is what makes the check free for a label
/// that cannot bend — mbgl gates on `textLeft != textRight`.
#[must_use]
pub fn angle_window_size(text_left: f32, text_right: f32, glyph_size: f32, box_scale: f32) -> f32 {
    if (text_left - text_right) == 0.0 {
        0.0
    } else {
        3.0 / 5.0 * glyph_size * box_scale
    }
}

/// Whether the line stays straight enough under a label placed here.
///
/// Sums the turn at every corner the label covers, over a sliding window, and fails when that
/// sum passes `max_angle`. A window rather than a single corner because a label survives one
/// sharp bend and not three gentle ones in a row — it is the accumulated curvature that makes
/// text unreadable, not any one turn.
#[must_use]
pub fn check_max_angle(
    line: &[(f32, f32)],
    anchor: &Anchor,
    label_length: f32,
    window_size: f32,
    max_angle: f32,
) -> bool {
    // A label on the first segment has no corner behind it to bend.
    if anchor.segment == 0 {
        return true;
    }

    let mut point = anchor.point;
    let mut index = anchor.segment + 1;
    let mut anchor_distance = 0.0f32;

    // Walk back to the segment the label starts on.
    while anchor_distance > -label_length / 2.0 {
        if index == 0 {
            // The line ends before the label does.
            return false;
        }
        index -= 1;
        anchor_distance -= distance(line[index], point);
        point = line[index];
    }

    anchor_distance += distance(line[index], line[index + 1]);
    index += 1;

    // Corners inside the window, and their total turn.
    let mut recent: alloc::collections::VecDeque<(f32, f32)> = alloc::collections::VecDeque::new();
    let mut recent_delta = 0.0f32;

    while anchor_distance < label_length / 2.0 {
        if index + 1 >= line.len() {
            return false;
        }
        let (previous, current, next) = (line[index - 1], line[index], line[index + 1]);

        let delta = f64::from(angle_to(previous, current)) - f64::from(angle_to(current, next));
        // Wrapped into -pi..pi before taking the magnitude: a turn of 359 degrees is a turn of
        // one, and summing the unwrapped value would fail a line that barely bends.
        let pi = core::f64::consts::PI;
        #[allow(clippy::cast_possible_truncation)]
        let delta = ((delta + 3.0 * pi) % (pi * 2.0) - pi).abs() as f32;

        recent.push_back((anchor_distance, delta));
        recent_delta += delta;

        while recent
            .front()
            .is_some_and(|(at, _)| anchor_distance - at > window_size)
        {
            if let Some((_, delta)) = recent.pop_front() {
                recent_delta -= delta;
            }
        }

        if recent_delta > max_angle {
            return false;
        }

        index += 1;
        anchor_distance += distance(current, next);
    }

    true
}

/// Walks the line dropping an anchor every `spacing`, keeping the ones a label fits at.
#[allow(clippy::too_many_arguments)]
fn resample(
    line: &[(f32, f32)],
    offset: f32,
    spacing: f32,
    angle_window: f32,
    max_angle: f32,
    label_length: f32,
    continued_line: bool,
    place_at_middle: bool,
) -> Vec<Anchor> {
    debug_assert!(spacing > 0.0, "a spacing of zero never advances");

    let half_label = label_length / 2.0;
    let length = line_length(line);
    let mut distance_so_far = 0.0f32;
    let mut marked = offset - spacing;
    let mut anchors = Vec::new();

    for (segment, pair) in line.windows(2).enumerate() {
        let (a, b) = (pair[0], pair[1]);
        let segment_distance = distance(a, b);
        let angle = angle_to(b, a);

        while marked + spacing < distance_so_far + segment_distance {
            marked += spacing;
            let t = (marked - distance_so_far) / segment_distance;
            let x = a.0 + (b.0 - a.0) * t;
            let y = a.1 + (b.1 - a.1) * t;

            // Inside the tile, and with the whole label between the line's ends.
            if (0.0..EXTENT).contains(&x)
                && (0.0..EXTENT).contains(&y)
                && marked - half_label >= 0.0
                && marked + half_label <= length
            {
                let anchor = Anchor {
                    point: (x.round(), y.round()),
                    angle,
                    segment,
                };
                if angle_window == 0.0
                    || check_max_angle(line, &anchor, label_length, angle_window, max_angle)
                {
                    anchors.push(anchor);
                }
            }
        }

        distance_so_far += segment_distance;
    }

    if !place_at_middle && anchors.is_empty() && !continued_line {
        // Nothing fitted. Try once more with a single anchor at the middle, which is what saves
        // a short line in an overscaled tile: the offset there is chosen to line labels up with
        // the parent tile's rather than to fit them as early as possible.
        return resample(
            line,
            distance_so_far / 2.0,
            spacing,
            angle_window,
            max_angle,
            label_length,
            continued_line,
            true,
        );
    }

    anchors
}

/// Where a label repeats along a line.
///
/// `spacing` is `symbol-spacing`; `max_angle` is `text-max-angle` in radians; the four extents
/// are the shaped label's, and `overscaling` is the tile's overscale factor.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn get_anchors(
    line: &[(f32, f32)],
    spacing: f32,
    max_angle: f32,
    text_left: f32,
    text_right: f32,
    icon_left: f32,
    icon_right: f32,
    glyph_size: f32,
    box_scale: f32,
    overscaling: f32,
) -> Vec<Anchor> {
    if line.is_empty() {
        return Vec::new();
    }

    let angle_window = angle_window_size(text_left, text_right, glyph_size, box_scale);
    let shaped_length = (text_right - text_left).max(icon_right - icon_left);
    let label_length = shaped_length * box_scale;

    // A line touching the tile's edge is the middle of a line that continues into the next tile.
    let continued_line =
        line[0].0 == 0.0 || line[0].0 == EXTENT || line[0].1 == 0.0 || line[0].1 == EXTENT;

    // A long label at a short spacing would overlap its neighbours, so the spacing is widened
    // to leave a quarter of it as a gap between edges. Collision would otherwise throw most of
    // them away, which is work done to be discarded.
    let mut spacing = spacing;
    if spacing - label_length < spacing / 4.0 {
        spacing = label_length + spacing / 4.0;
    }

    // Two glyph widths of slack, which mbgl attributes to T intersections: a name printed right
    // at a junction collides with the crossing road's.
    let fixed_extra_offset = glyph_size * 2.0;
    let offset = if continued_line {
        (spacing / 2.0 * overscaling) % spacing
    } else {
        ((shaped_length / 2.0 + fixed_extra_offset) * box_scale * overscaling) % spacing
    };

    resample(
        line,
        offset,
        spacing,
        angle_window,
        max_angle,
        label_length,
        continued_line,
        false,
    )
}

/// The single anchor at a line's midpoint, for `symbol-placement: line-center`.
///
/// One label per line rather than a repeating run, which is what a river or a boundary wants:
/// the name appears once, in the middle, and does not march along the feature.
///
/// `None` when the line is empty, or when it bends too sharply at its middle. The second is not
/// a failure to fall back from — a caller asked for the centre specifically, and putting the
/// label somewhere else instead would silently answer a different question.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn get_center_anchor(
    line: &[(f32, f32)],
    max_angle: f32,
    text_left: f32,
    text_right: f32,
    icon_left: f32,
    icon_right: f32,
    glyph_size: f32,
    box_scale: f32,
) -> Option<Anchor> {
    if line.is_empty() {
        return None;
    }

    let angle_window = angle_window_size(text_left, text_right, glyph_size, box_scale);
    let label_length = (text_right - text_left).max(icon_right - icon_left) * box_scale;
    let centre = line_length(line) / 2.0;
    let mut travelled = 0.0f32;

    for (segment, pair) in line.windows(2).enumerate() {
        let (a, b) = (pair[0], pair[1]);
        let segment_distance = distance(a, b);

        if travelled + segment_distance > centre {
            let t = (centre - travelled) / segment_distance;
            let anchor = Anchor {
                point: (
                    (a.0 + (b.0 - a.0) * t).round(),
                    (a.1 + (b.1 - a.1) * t).round(),
                ),
                angle: angle_to(b, a),
                segment,
            };

            // Note there is no tile-bounds test here, unlike the repeating case. A centred
            // label belongs to its feature rather than to a position, so a river whose middle
            // falls outside this tile still gets its name — which is mbgl's behaviour and is
            // why its own test asserts an anchor at (-3, -3).
            return (angle_window == 0.0
                || check_max_angle(line, &anchor, label_length, angle_window, max_angle))
            .then_some(anchor);
        }

        travelled += segment_distance;
    }

    None
}
