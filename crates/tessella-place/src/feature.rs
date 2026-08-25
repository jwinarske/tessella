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

use crate::grid::Bounds;

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
/// Along-line placement is not built here. It replaces the single box with a run of circles
/// following the line, which needs the anchors along that line — a separate piece.
#[must_use]
pub fn collision_box(
    extent: Extent,
    anchor: (f32, f32),
    box_scale: f32,
    padding: Padding,
    rotate_degrees: f32,
) -> Option<CollisionBox> {
    if extent.is_empty() {
        return None;
    }

    // Scale first, then pad: the padding is a number of screen pixels and must not grow with
    // the label.
    let y1 = extent.top * box_scale - padding.top;
    let y2 = extent.bottom * box_scale + padding.bottom;
    let x1 = extent.left * box_scale - padding.left;
    let x2 = extent.right * box_scale + padding.right;

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
