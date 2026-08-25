//! Placing a line-following label's glyphs along the projected line.
//!
//! A transcription of mbgl's `symbol_projection.cpp`: `placeGlyphAlongLine` and the
//! `placeGlyphsAlongLine` around it. This runs per view per frame rather than per tile, and it
//! has to: which way a road runs *on screen* is a property of the camera, so the same label is
//! upright at one bearing and upside down at another.
//!
//! # Why layout cannot do this
//!
//! Layout gives each glyph one number — how far along the line it sits (`glyph_offsets`). Turning
//! that into a position means walking the line, and the line has to be walked *after* projection,
//! not before. A label laid out flat and then bent puts every glyph but the first in the wrong
//! place, because bending does not preserve the distances the layout assumed.
//!
//! # Keeping text upright
//!
//! `text-keep-upright` defaults to true and is not cosmetic: a label placed along a line that
//! runs right to left on screen is drawn mirrored and reads backwards. mbgl detects it by
//! placing the first and last glyph and comparing them — if the first ends up to the *right* of
//! the last, the label needs flipping — and then re-runs the placement walking the line the
//! other way. That is why [`place_glyphs_along_line`] can answer
//! [`Placement::NeedsFlipping`] rather than just doing it: the caller owns the retry, exactly as
//! mbgl's does, and a caller that has turned `text-keep-upright` off gets the unflipped answer.

use alloc::vec::Vec;

/// One glyph, placed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlacedGlyph {
    /// Where it goes, in the space the line was given in.
    pub point: (f32, f32),
    /// Which way it faces, in radians.
    pub angle: f32,
}

/// What placing a label along a line produced.
#[derive(Debug, Clone, PartialEq)]
pub enum Placement {
    /// Every glyph found a place.
    Placed(Vec<PlacedGlyph>),
    /// The line ended before the label did, so the label is not drawn at all.
    ///
    /// Not "drawn short": half a road name is worse than none, and mbgl drops the whole symbol.
    NotEnoughRoom,
    /// The label would read right to left. Run again with `flip` set.
    NeedsFlipping,
}

/// How a label is placed along its line.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LineOffsets {
    /// `text-offset` along the line, in ems, scaled by the font size before use.
    pub along: f32,
    /// `text-offset` perpendicular to it, which is what lifts a label off the road it names.
    pub across: f32,
    /// The font size the offsets and the glyph distances are scaled by.
    pub font_scale: f32,
    /// Walk the line the other way, drawing the label reversed.
    pub flip: bool,
    /// `text-keep-upright`: answer [`Placement::NeedsFlipping`] rather than draw it mirrored.
    pub keep_upright: bool,
}

impl Default for LineOffsets {
    fn default() -> Self {
        Self {
            along: 0.0,
            across: 0.0,
            font_scale: 1.0,
            flip: false,
            // The style default, and the one that matters: false draws half the labels on a
            // street tile backwards.
            keep_upright: true,
        }
    }
}

/// Places one glyph at `offset` along `line` from `anchor`.
///
/// `segment` is which pair of line vertices the anchor lies between. `None` when the line runs
/// out before the offset does.
///
/// Returns the point and the angle of the segment it landed on, which is what rotates the glyph
/// to follow the line.
#[must_use]
pub fn place_glyph_along_line(
    line: &[(f32, f32)],
    anchor: (f32, f32),
    segment: usize,
    offset: f32,
    offsets: &LineOffsets,
) -> Option<PlacedGlyph> {
    // mbgl folds the along-line offset in with the sign flipped when the label is reversed,
    // because a reversed label's glyphs run backwards through the same distances.
    let combined = if offsets.flip {
        offset - offsets.along
    } else {
        offset + offsets.along
    };

    // Which way to walk, and the half turn a reversed or leftward glyph needs so it is not drawn
    // mirrored. Both are mbgl's, and the second is easy to drop: without it a label placed
    // leftwards is in the right place and inside out.
    let mut direction: isize = if combined > 0.0 { 1 } else { -1 };
    let mut angle = 0.0f32;
    if offsets.flip {
        direction = -direction;
        angle = core::f32::consts::PI;
    }
    if direction < 0 {
        angle += core::f32::consts::PI;
    }

    // Walking forward starts at the segment's far vertex; walking back, at its near one.
    #[allow(clippy::cast_possible_wrap)]
    let mut index: isize = if direction > 0 {
        segment as isize
    } else {
        segment as isize + 1
    };

    let mut current = anchor;
    let mut previous = anchor;
    let mut to_previous = 0.0f32;
    let mut segment_distance = 0.0f32;
    let wanted = combined.abs();

    while to_previous + segment_distance <= wanted {
        index += direction;
        #[allow(clippy::cast_possible_wrap)]
        if index < 0 || index >= line.len() as isize {
            // The offset does not fit on the line.
            return None;
        }

        previous = current;
        #[allow(clippy::cast_sign_loss)]
        {
            current = line[index as usize];
        }
        to_previous += segment_distance;
        segment_distance = (current.0 - previous.0).hypot(current.1 - previous.1);
    }

    // The glyph falls inside the segment just stepped onto; interpolate to it.
    let t = (wanted - to_previous) / segment_distance;
    let run = (current.0 - previous.0, current.1 - previous.1);
    let mut point = (previous.0 + run.0 * t, previous.1 + run.1 * t);

    // Then lift it off the line, along the segment's normal. Scaled by the segment length
    // because `run` is not a unit vector, and signed by the direction so a label above the road
    // stays above it when the walk reverses.
    let magnitude = run.0.hypot(run.1);
    if magnitude > 0.0 {
        #[allow(clippy::cast_precision_loss)]
        let across = offsets.across * direction as f32 / magnitude;
        point = (point.0 - run.1 * across, point.1 + run.0 * across);
    }

    Some(PlacedGlyph {
        point,
        angle: angle + run.1.atan2(run.0),
    })
}

/// Places every glyph of one label along one line.
///
/// `glyph_offsets` is the label's slice of [`SymbolBuffers::glyph_offsets`], one distance per
/// glyph. The line and the anchor are in whatever space the caller projected into — placement
/// happens in screen space, so for a real frame that is pixels.
///
/// [`SymbolBuffers::glyph_offsets`]: tessella_layout::symbol_bucket::SymbolBuffers::glyph_offsets
#[must_use]
pub fn place_glyphs_along_line(
    line: &[(f32, f32)],
    anchor: (f32, f32),
    segment: usize,
    glyph_offsets: &[f32],
    offsets: &LineOffsets,
) -> Placement {
    let place = |offset: f32| {
        place_glyph_along_line(line, anchor, segment, offset * offsets.font_scale, offsets)
    };

    // The upright test, before anything else is placed. mbgl runs it on the two end glyphs
    // rather than on the anchor's angle, and the difference is real: a label spanning a bend can
    // sit on a segment that runs one way while the label as a whole reads the other.
    if offsets.keep_upright && !offsets.flip && glyph_offsets.len() > 1 {
        let first = place(glyph_offsets[0]);
        let last = place(*glyph_offsets.last().expect("checked non-empty"));
        match (first, last) {
            (Some(first), Some(last)) if first.point.0 > last.point.0 => {
                return Placement::NeedsFlipping;
            }
            (Some(_), Some(_)) => {}
            // If either end has no room the label does not fit, and answering `NeedsFlipping`
            // would send the caller round the loop to find that out again.
            _ => return Placement::NotEnoughRoom,
        }
    }

    let mut placed = Vec::with_capacity(glyph_offsets.len());
    for offset in glyph_offsets {
        let Some(glyph) = place(*offset) else {
            return Placement::NotEnoughRoom;
        };
        placed.push(glyph);
    }
    Placement::Placed(placed)
}

/// Places a label, flipping it if that is what keeps it upright.
///
/// The retry mbgl's caller performs, in one call: place, and if the answer is that the label
/// reads backwards, place it again walking the line the other way. Returns the placement and
/// whether it was flipped.
#[must_use]
pub fn place_upright(
    line: &[(f32, f32)],
    anchor: (f32, f32),
    segment: usize,
    glyph_offsets: &[f32],
    offsets: &LineOffsets,
) -> (Placement, bool) {
    match place_glyphs_along_line(line, anchor, segment, glyph_offsets, offsets) {
        Placement::NeedsFlipping => {
            let flipped = LineOffsets {
                flip: true,
                ..*offsets
            };
            (
                place_glyphs_along_line(line, anchor, segment, glyph_offsets, &flipped),
                true,
            )
        }
        other => (other, false),
    }
}
