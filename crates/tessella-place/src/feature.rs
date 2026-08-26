//! The box a label occupies for collision purposes.
//!
//! A transcription of mbgl's `CollisionFeature` for point-placed labels. The shaper produced a
//! bounding box in label-local pixels; this scales it to the zoom being placed at, pads it, and
//! anchors it — which is the shape the grid indexes and placement tests.
//!
//! # A collision box is not the label's box
//!
//! It is bigger, by `text-padding`, and it is bigger on purpose: two labels that merely fail to
//! overlap still read as crowded, and the padding is what buys the white space between them.
//! Scaling happens before padding, which is what makes the padding a constant number of screen
//! pixels rather than something that grows with the zoom.
//!
//! # A rotated label reserves its envelope, not its box
//!
//! The grid holds axis-aligned rectangles, so a rotated label is indexed by the upright box that
//! contains it. mbgl says as much and notes it "may be quite large for wide labels rotated 45
//! degrees" — a long label on a diagonal reserves close to a square. That is a real cost of
//! keeping the index axis-aligned, and it is transcribed rather than worked around: the
//! alternative is oriented-box intersection in the inner loop of placement.
//!
//! # An empty label has no box, and that is not the same as an empty box
//!
//! A label that shaped to nothing — every glyph still loading, or a feature whose text evaluated
//! to an empty string — produces no collision box at all. A zero-sized box at the anchor would
//! still collide with anything covering that point, so an invisible label would push a visible
//! one off the map.

use crate::grid::{Bounds, Circle};

/// Padding around a collision box, in screen pixels.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Padding {
    /// Above.
    pub top: f32,
    /// To the right.
    pub right: f32,
    /// Below.
    pub bottom: f32,
    /// To the left.
    pub left: f32,
}

impl Padding {
    /// The same padding on every side, which is what `text-padding` is.
    #[must_use]
    pub const fn uniform(amount: f32) -> Self {
        Self {
            top: amount,
            right: amount,
            bottom: amount,
            left: amount,
        }
    }
}

/// A label's collision box: an anchor and the extent around it.
///
/// The extent is kept separate from the anchor rather than folded in, because placement moves
/// the anchor — along a line, or by `text-translate` — and the extent does not move with it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CollisionBox {
    /// Where the label is anchored, in tile coordinates.
    pub anchor: (f32, f32),
    /// Left of the anchor.
    pub x1: f32,
    /// Above the anchor.
    pub y1: f32,
    /// Right of the anchor.
    pub x2: f32,
    /// Below the anchor.
    pub y2: f32,
}

impl CollisionBox {
    /// The rectangle this occupies, with the anchor applied.
    #[must_use]
    pub fn bounds(&self) -> Bounds {
        Bounds::new(
            (self.anchor.0 + self.x1, self.anchor.1 + self.y1),
            (self.anchor.0 + self.x2, self.anchor.1 + self.y2),
        )
    }

    /// Its width and height.
    #[must_use]
    pub fn size(&self) -> (f32, f32) {
        (self.x2 - self.x1, self.y2 - self.y1)
    }
}

/// The extent a shaped label occupies, as the shaper reports it.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Extent {
    /// Top edge, relative to the anchor.
    pub top: f32,
    /// Bottom edge.
    pub bottom: f32,
    /// Left edge.
    pub left: f32,
    /// Right edge.
    pub right: f32,
}

impl Extent {
    /// Whether the label occupies nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.top == 0.0 && self.bottom == 0.0 && self.left == 0.0 && self.right == 0.0
    }
}

/// Rotates a point about the origin.
fn rotate(point: (f32, f32), radians: f32) -> (f32, f32) {
    let (sin, cos) = radians.sin_cos();
    (cos * point.0 - sin * point.1, sin * point.0 + cos * point.1)
}

/// The collision box for a point-placed label.
///
/// `box_scale` takes the label from its shaped size to the size it draws at this zoom;
/// `padding` is `text-padding` in screen pixels; `rotate` is `text-rotate` in degrees.
///
/// `None` when the label occupies nothing, which is not the same as an empty box: a label that
/// shaped to nothing must not collide with anything, and a zero-sized box at the anchor still
/// would.
///
/// Along-line placement is [`line_circles`] instead: a run of circles following the line.
#[must_use]
pub fn collision_box(
    extent: Extent,
    anchor: (f32, f32),
    box_scale: f32,
    padding: Padding,
    rotate_degrees: f32,
) -> Option<CollisionBox> {
    collision_box_with(extent, anchor, box_scale, padding, None, rotate_degrees)
}

/// The collision box for a label, with an icon's content margins added.
///
/// `content` is what [`crate::feature::Padding`] cannot express on its own: `text-padding` is a
/// number of *screen* pixels and does not scale, while an icon's margins are part of the picture
/// and do. mbgl keeps them apart for that reason and so does this — they are added after the
/// scale, each at its own.
///
/// The margins *grow* the box, which reads backwards until the fitting is in view: once
/// `icon-text-fit` has stretched a shield around its label, the extent is the shield's *content*
/// area and the drawn picture reaches further out by its border. Collision reserves the picture.
///
/// # Errors
///
/// `None` under the same condition as [`collision_box`].
#[must_use]
pub fn collision_box_with(
    extent: Extent,
    anchor: (f32, f32),
    box_scale: f32,
    padding: Padding,
    content: Option<(f32, f32, f32, f32)>,
    rotate_degrees: f32,
) -> Option<CollisionBox> {
    if extent.is_empty() {
        return None;
    }

    // Scale first, then pad: the padding is a number of screen pixels and must not grow with
    // the label.
    let mut y1 = extent.top * box_scale - padding.top;
    let mut y2 = extent.bottom * box_scale + padding.bottom;
    let mut x1 = extent.left * box_scale - padding.left;
    let mut x2 = extent.right * box_scale + padding.right;

    if let Some((top, bottom, left, right)) = content {
        y1 -= top * box_scale;
        y2 += bottom * box_scale;
        x1 -= left * box_scale;
        x2 += right * box_scale;
    }
    let (y1, y2, x1, x2) = (y1, y2, x1, x2);

    if rotate_degrees == 0.0 {
        return Some(CollisionBox {
            anchor,
            x1,
            y1,
            x2,
            y2,
        });
    }

    // The grid is axis-aligned, so a rotated label is reserved by the upright box containing it.
    let radians = rotate_degrees.to_radians();
    let corners = [
        rotate((x1, y1), radians),
        rotate((x2, y1), radians),
        rotate((x1, y2), radians),
        rotate((x2, y2), radians),
    ];
    let min_x = corners
        .iter()
        .map(|corner| corner.0)
        .fold(f32::MAX, f32::min);
    let max_x = corners
        .iter()
        .map(|corner| corner.0)
        .fold(f32::MIN, f32::max);
    let min_y = corners
        .iter()
        .map(|corner| corner.1)
        .fold(f32::MAX, f32::min);
    let max_y = corners
        .iter()
        .map(|corner| corner.1)
        .fold(f32::MIN, f32::max);

    Some(CollisionBox {
        anchor,
        x1: min_x,
        y1: min_y,
        x2: max_x,
        y2: max_y,
    })
}

/// One circle of a line label's collision run.
///
/// The distance is what lets placement use a *prefix* of the run: a label that is only partly
/// on screen, or that shrinks with pitch, tests the circles near its anchor and ignores the
/// rest, and it needs to know how far out each one is to do that.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LineCircle {
    /// Where it sits, in the coordinates the line was given in.
    pub circle: Circle,
    /// How far from the anchor it is, signed and padded down by a fifth.
    ///
    /// mbgl's `signedDistanceFromAnchor`. The fifth is deliberate slack — the comment calls it
    /// "a little bit of conservative padding in choosing which boxes to use" — so a circle near
    /// the edge of what is being tested is included rather than dropped.
    pub distance_from_anchor: f32,
}

/// The circles a line-following label collides as.
///
/// A transcription of mbgl's `CollisionFeature::bboxifyLabel`. `label_length` is the label's
/// width and `box_size` its height, both already scaled to the zoom being placed at;
/// `overscaling` is the tile's, which widens the padding run.
///
/// # Why a run of circles rather than one box
///
/// A road name follows the road. Its upright bounding box is most of a square once the road
/// bends, and reserving that square would stop labels appearing on any of the streets nearby —
/// the same cost the point path pays for a rotated label, except that a line label is rotated by
/// definition and often more than once within its own length. Circles follow the bend, and a
/// circle-circle test is one distance.
///
/// # The run extends past the label
///
/// mbgl adds padding circles either side, because a pitched camera makes distant labels *larger*
/// on screen than the box they were laid out for, and a label that has grown past its collision
/// shape overlaps its neighbour with nothing detecting it. The padding grows with overscaling,
/// slowly — `1 + 0.4 * log2(overscaling)` — because an overscaled tile places labels closer
/// together and each extra circle costs a query.
///
/// Empty when the line is too short to hold the run, which is not the same as "collides with
/// nothing": a label that could not be bboxified was already refused by the bend check.
#[must_use]
pub fn line_circles(
    line: &[(f32, f32)],
    anchor: (f32, f32),
    segment: usize,
    label_length: f32,
    box_size: f32,
    overscaling: f32,
) -> Vec<LineCircle> {
    let mut out = Vec::new();
    if box_size <= 0.0 || line.len() < 2 {
        return out;
    }

    let step = box_size / 2.0;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let count = ((label_length / step).floor() as isize).max(1);

    // The padding run, widened for overscaled tiles.
    let padding_factor = 0.4f32.mul_add(overscaling.max(1.0).log2(), 1.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    #[allow(clippy::cast_precision_loss)]
    let padding = (count as f32 * padding_factor / 2.0).floor() as isize;

    // The first circle's centre is half a box in, so the *edge* of the run is the edge of the
    // label rather than half a box past it.
    let first_offset = -box_size / 2.0;
    let label_start = -label_length / 2.0;

    // Walk backwards to the segment the label actually begins on. The anchor is somewhere in the
    // middle of the label, so the run starts before it, possibly several segments before.
    //
    // It starts at the vertex *after* the anchor's segment, so the first step back measures from
    // the anchor itself to that segment's near vertex. Starting at the segment skips that step,
    // and an anchor most of the way along a long segment is then treated as if it sat at the
    // near end of it — which puts the whole run at the start of the line.
    let mut index = (segment + 1).min(line.len() - 1);
    let mut point = anchor;
    let mut anchor_distance = first_offset;
    loop {
        if index == 0 {
            if anchor_distance > label_start {
                // The line does not reach back far enough to hold the label at all. mbgl notes
                // the bend check should already have caught this.
                return out;
            }
            // Far enough for the label, if not for all of the padding.
            break;
        }
        index -= 1;
        anchor_distance -= (line[index].0 - point.0).hypot(line[index].1 - point.1);
        point = line[index];
        if anchor_distance <= label_start {
            break;
        }
    }

    let mut segment_length =
        (line[index + 1].0 - line[index].0).hypot(line[index + 1].1 - line[index].1);

    for i in -padding..count + padding {
        #[allow(clippy::cast_precision_loss)]
        let box_offset = i as f32 * step;
        let mut distance = label_start + box_offset;

        // The padding circles are spread further apart than the label's own, which is what makes
        // a short run of them cover the distance a pitched label grows by.
        if box_offset < 0.0 {
            distance += box_offset;
        }
        if box_offset > label_length {
            distance += box_offset - label_length;
        }

        if distance < anchor_distance {
            // The line does not extend back this far; skip rather than refuse.
            continue;
        }

        // Advance to the segment this circle falls on.
        while anchor_distance + segment_length < distance {
            anchor_distance += segment_length;
            index += 1;
            if index + 1 >= line.len() {
                // The line ran out before the run did.
                return out;
            }
            segment_length =
                (line[index + 1].0 - line[index].0).hypot(line[index + 1].1 - line[index].1);
        }

        if segment_length <= 0.0 {
            continue;
        }
        let into_segment = distance - anchor_distance;
        let (from, to) = (line[index], line[index + 1]);
        let t = into_segment / segment_length;
        let center = (
            (to.0 - from.0).mul_add(t, from.0),
            (to.1 - from.1).mul_add(t, from.1),
        );

        // A circle within one step of the anchor is forced to distance zero, so even a
        // zero-width label reserves one circle rather than none.
        let from_anchor = distance - first_offset;
        let distance_from_anchor = if from_anchor.abs() < step {
            0.0
        } else {
            from_anchor * 0.8
        };

        out.push(LineCircle {
            circle: Circle::new(center, box_size / 2.0),
            distance_from_anchor,
        });
    }

    out
}

/// The circles a line-following label collides as, scaled and padded for this zoom.
///
/// mbgl's `CollisionFeature` along-line branch: the same scale-then-pad the point path does,
/// and then [`line_circles`]. `None` when the label occupies nothing, for the same reason
/// [`collision_box`] answers `None`.
///
/// The height has a floor of ten times `box_scale`. A short label — one or two glyphs — would
/// otherwise reserve circles smaller than the gap between them, and a run of circles that does
/// not overlap is a dotted line rather than a covering.
#[must_use]
pub fn collision_circles(
    extent: Extent,
    line: &[(f32, f32)],
    anchor: (f32, f32),
    segment: usize,
    box_scale: f32,
    padding: Padding,
    overscaling: f32,
) -> Option<Vec<LineCircle>> {
    if extent.is_empty() {
        return None;
    }

    let y1 = extent.top * box_scale - padding.top;
    let y2 = extent.bottom * box_scale + padding.bottom;
    let x1 = extent.left * box_scale - padding.left;
    let x2 = extent.right * box_scale + padding.right;

    let height = y2 - y1;
    if height <= 0.0 {
        return None;
    }
    let height = height.max(10.0 * box_scale);

    let circles = line_circles(line, anchor, segment, x2 - x1, height, overscaling);
    if circles.is_empty() {
        return None;
    }
    Some(circles)
}
