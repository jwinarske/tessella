//! Turning shaped glyphs into the quads a symbol shader draws.
//!
//! A transcription of mbgl's `getGlyphQuads` for horizontal text. Each glyph becomes four
//! corners in label-local coordinates plus the atlas rectangle to sample, and the symbol shader
//! turns those into two triangles.
//!
//! # The quad is bigger than the glyph, on purpose
//!
//! A distance field is only useful if the shader can read *outside* the ink: that is where the
//! falloff lives, and it is what makes a glyph stay smooth when the map scales it. So the quad
//! covers the glyph plus the three-pixel border the encoder wrote and the one pixel the atlas
//! kept, four in total on every side. Sizing the quad to the ink instead gives letters with
//! clipped antialiasing — thin at small sizes and visibly cut off at large ones.
//!
//! # Half the advance cancels, and only for point labels
//!
//! mbgl writes the horizontal position as `-halfAdvance + (x + halfAdvance + offset)`, which
//! for a point label is just `x + offset`. It is written that way because the two halves come
//! from different places: for a label following a line the second term moves into
//! `glyphOffset`, the shader applies it after projecting along the line, and the cancellation
//! stops. Keeping mbgl's form means that case is a branch rather than a rewrite.

use crate::atlas::Rect;
use crate::pbf::{BORDER, Metrics};
use crate::shaping::{Anchor, Shaping, Y_OFFSET};
use crate::sprite::TextFit;

/// How far outside the ink a quad reaches, in pixels.
///
/// The encoder's three-pixel border plus the one pixel the atlas keeps inside the rectangle it
/// reports. mbgl spells this `3.0f + glyphPadding`.
pub const RECT_BUFFER: f32 = BORDER as f32 + 1.0;

/// One glyph's quad.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quad {
    /// Top-left corner, in label-local pixels.
    pub tl: (f32, f32),
    /// Top-right corner.
    pub tr: (f32, f32),
    /// Bottom-left corner.
    pub bl: (f32, f32),
    /// Bottom-right corner.
    pub br: (f32, f32),
    /// The atlas rectangle to sample.
    pub tex: Rect,
    /// Where along the label this glyph sits.
    ///
    /// Zero for a point label, where the position is already in the corners. For a label
    /// following a line it is the glyph's distance along that line, which the shader needs
    /// after projecting.
    pub glyph_offset: (f32, f32),
}

/// What a label's quads are built against.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// `text-offset`, in pixels.
    pub text_offset: [f32; 2],
    /// `text-rotate`, in radians.
    pub text_rotate: f32,
    /// Whether the label follows a line rather than sitting at a point.
    pub along_line: bool,
    /// Whether the layer's `text-writing-mode` lists `vertical`.
    ///
    /// mbgl's `allowVerticalPlacement`, and it reaches the quads for two separate reasons. It is
    /// half of whether an upright glyph is turned at all — a label following a line is turned
    /// with the line whether or not the style asked for vertical placement, which is the other
    /// half. And with [`Shaping::verticalizable`] it decides whether every glyph on the line is
    /// re-centred in its column, which is what keeps a scaled glyph or an image from sitting
    /// against one edge of it.
    pub allow_vertical_placement: bool,
}

/// What the atlas and the glyph manager know about one glyph.
#[derive(Debug, Clone, Copy)]
pub struct Placed {
    /// Where it is in the atlas.
    pub rect: Rect,
    /// Its layout metrics.
    pub metrics: Metrics,
}

/// Rotates a point about the origin.
fn rotate(point: (f32, f32), sin: f32, cos: f32) -> (f32, f32) {
    (cos * point.0 - sin * point.1, sin * point.0 + cos * point.1)
}

/// Builds the quads for a shaped label.
///
/// `placed` answers where each glyph is in the atlas. A glyph it does not know is skipped
/// rather than drawn from a rectangle that is not there — a label whose glyphs have not all
/// arrived draws the ones that have, which is what keeps a map readable while a font loads.
///
/// Images in text are not built here: an image's metrics come from the sprite rather than the
/// font, and its rectangle is in the icon atlas.
pub fn glyph_quads<F>(shaping: &Shaping, mut placed: F, options: &Options) -> Vec<Quad>
where
    F: FnMut(u32) -> Option<Placed>,
{
    let mut quads = Vec::new();
    let (sin, cos) = if options.text_rotate == 0.0 {
        (0.0, 1.0)
    } else {
        options.text_rotate.sin_cos()
    };

    for line in &shaping.lines {
        for glyph in line {
            let Some(Placed { rect, metrics }) = placed(glyph.codepoint) else {
                continue;
            };
            // A glyph with no area has nothing to sample. It should not have been placed, but
            // an atlas that ran out of room can hand one back.
            if rect.width == 0 || rect.height == 0 {
                continue;
            }

            // Scaled by the section's `font-scale`, as every measurement of this glyph is:
            // mbgl reads `positionedGlyph.scale` at each of them. A glyph twice the size is
            // twice as wide, sits twice as far from its own centre, and covers twice as much of
            // the atlas rectangle it samples — and getting one of the three wrong shows as text
            // that is the right size in the wrong place, or the wrong size in the right one.
            #[allow(clippy::cast_precision_loss)]
            let half_advance = metrics.advance as f32 * glyph.scale / 2.0;

            // A glyph is turned when it kept its upright orientation *and* the line it is on is
            // one that turns: a label following a line turns with the line, and a point label
            // turns only where the style asked for vertical placement. Both are the label's
            // business rather than the glyph's, which is why the flag alone does not decide it.
            let rotate_vertical =
                (options.along_line || options.allow_vertical_placement) && glyph.vertical;

            // Every glyph on a vertical line is re-centred in its column. The column is one em
            // wide and what sits in it need not be: a scaled glyph is wider or narrower than its
            // line's cell, and the correction is the difference. mbgl folds the line's own offset
            // in here as well, which is what pushes a line down that an oversized image grew.
            let line_offset = if options.allow_vertical_placement && shaping.verticalizable {
                -(glyph.scale - 1.0) * crate::text::ONE_EM
            } else {
                0.0
            };

            // For a point label the position is baked into the corners. Along a line it moves
            // to `glyph_offset`, because the shader has to project it before applying it.
            let (glyph_offset, mut built_in) = if options.along_line {
                ((glyph.x + half_advance, glyph.y), (0.0, 0.0))
            } else {
                (
                    (0.0, 0.0),
                    (
                        glyph.x + half_advance + options.text_offset[0],
                        glyph.y + options.text_offset[1] - line_offset,
                    ),
                )
            };

            // A turned quad is rotated about a point of its own and only then moved to where the
            // label wants it, so the offset comes out of the corners first and is added back
            // after. Rotating a quad that already carried it would swing the label's position
            // around the origin along with the glyph.
            let verticalized_offset = if rotate_vertical {
                let held = built_in;
                built_in = (0.0, 0.0);
                held
            } else {
                (0.0, 0.0)
            };

            #[allow(clippy::cast_precision_loss)]
            let x1 = (metrics.left as f32 - RECT_BUFFER) * glyph.scale - half_advance + built_in.0;
            #[allow(clippy::cast_precision_loss)]
            let y1 = (-(metrics.top as f32) - RECT_BUFFER) * glyph.scale + built_in.1;
            #[allow(clippy::cast_precision_loss)]
            let x2 = x1 + rect.width as f32 * glyph.scale;
            #[allow(clippy::cast_precision_loss)]
            let y2 = y1 + rect.height as f32 * glyph.scale;

            let mut quad = Quad {
                tl: (x1, y1),
                tr: (x2, y1),
                bl: (x1, y2),
                br: (x2, y2),
                tex: rect,
                glyph_offset,
            };

            if rotate_vertical {
                // A glyph that stays upright on a vertical line is drawn from a horizontal
                // layout, so the label is rotated a quarter turn clockwise and each such glyph a
                // quarter turn back. The centre is the middle of the left edge of its own em
                // box, which is where the two rotations cancel: turning about it lands the
                // glyph's middle on the line's midline, so the `Y_OFFSET` that pulled it up
                // there is no longer wanted — and is what the correction below takes out again,
                // along with the pull to the left that the same rotation introduces.
                //
                // The half-width term is for a glyph narrower than the column. A full-width
                // ideograph advances a whole em and it is zero; a half-width character has to
                // come back up by the difference or it hangs below its cell.
                let center = (-half_advance, half_advance - Y_OFFSET);
                let half_width = crate::text::ONE_EM / 2.0 - half_advance;
                let correction = (5.0 - Y_OFFSET - half_width, 0.0);
                let turn = |point: (f32, f32)| {
                    let about = (point.0 - center.0, point.1 - center.1);
                    // A quarter turn anticlockwise, which is `(x, y) -> (y, -x)` and is written
                    // out rather than taken from a sine and a cosine that are exactly one and
                    // zero.
                    let turned = (about.1, -about.0);
                    (
                        turned.0 + center.0 + correction.0 + verticalized_offset.0,
                        turned.1 + center.1 + correction.1 + verticalized_offset.1,
                    )
                };
                quad.tl = turn(quad.tl);
                quad.tr = turn(quad.tr);
                quad.bl = turn(quad.bl);
                quad.br = turn(quad.br);
            }

            if options.text_rotate != 0.0 {
                quad.tl = rotate(quad.tl, sin, cos);
                quad.tr = rotate(quad.tr, sin, cos);
                quad.bl = rotate(quad.bl, sin, cos);
                quad.br = rotate(quad.br, sin, cos);
            }

            quads.push(quad);
        }
    }

    quads
}

/// The one-pixel pad an icon quad carries on every side.
///
/// mbgl's comment says it plainly: a ten-pixel icon that is not perfectly aligned to the pixel
/// grid covers eleven actual pixels, so a quad sized to the icon clips a sliver off one edge.
/// The pad is on the *quad* and not on the texture rectangle — the extra pixel samples the
/// atlas padding, which is why the atlas reserves it.
pub const ICON_QUAD_BORDER: f32 = 1.0;

/// Where an icon sits relative to its anchor, before it becomes a quad.
///
/// mbgl's `PositionedIcon::shapeIcon`. Separate from the quad for the same reason shaping is
/// separate from `glyph_quads`: the box is what collision measures and the quad is what draws,
/// and the quad is a pixel larger on every side.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PositionedIcon {
    /// Left edge, relative to the anchor.
    pub left: f32,
    /// Top edge.
    pub top: f32,
    /// Right edge.
    pub right: f32,
    /// Bottom edge.
    pub bottom: f32,
    /// How far the drawn icon reaches beyond its *content* box, on each side.
    ///
    /// Zero for an icon with no content box. For one that has it — a shield — the box above is
    /// the content area once `icon-text-fit` has stretched it around the label, and the picture
    /// still extends past it by the sprite's border. Collision reserves the picture rather than
    /// the text area, so these are added back at that point rather than here.
    pub collision_padding: (f32, f32, f32, f32),
}

/// Places an icon of `size` logical pixels against its anchor and offset.
///
/// `size` is the sprite's size *after* its pixel ratio — a 48-pixel `@2x` sprite is 24 logical
/// pixels, and using the sheet size here would draw every retina icon at twice its size.
#[must_use]
pub fn shape_icon(size: (f32, f32), offset: [f32; 2], anchor: Anchor) -> PositionedIcon {
    let (horizontal, vertical) = anchor.alignment();
    let left = offset[0] - size.0 * horizontal;
    let top = offset[1] - size.1 * vertical;
    PositionedIcon {
        left,
        top,
        right: left + size.0,
        bottom: top + size.1,
        collision_padding: (0.0, 0.0, 0.0, 0.0),
    }
}

/// The margins between an icon's content box and its own edges, in logical pixels.
///
/// mbgl computes these in `shapeIcon` from `content` and the pixel ratio. `content` is in the
/// sprite's own pixels, so a retina shield's margins are half what the numbers say — dividing by
/// the ratio is what keeps a 2x shield's border the same size on screen as a 1x one's.
///
/// Ordered top, bottom, left, right, matching the extent everything else here carries.
#[must_use]
pub fn content_padding(
    size: (f32, f32),
    content: (f32, f32, f32, f32),
    pixel_ratio: f32,
) -> (f32, f32, f32, f32) {
    let (left, top, right, bottom) = content;
    (
        top / pixel_ratio,
        size.1 - bottom / pixel_ratio,
        left / pixel_ratio,
        size.0 - right / pixel_ratio,
    )
}

/// How `icon-text-fit` stretches an icon around its label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IconTextFit {
    /// Leave the icon alone.
    #[default]
    None,
    /// Stretch it to the text's width, and centre it vertically.
    Width,
    /// Stretch it to the text's height, and centre it horizontally.
    Height,
    /// Stretch it in both directions.
    Both,
}

/// Resizes an icon around the text it holds — mbgl's `PositionedIcon::fitIconToText`.
///
/// `text` is the shaped label's extent as `(top, bottom, left, right)` and `padding` is
/// `icon-text-fit-padding`, in the same order.
///
/// The icon's *anchor* is deliberately ignored, which mbgl says outright: `icon-text-fit` is a
/// statement about where the icon goes relative to the text, and honouring the anchor as well
/// would move it away from the label it is drawn around.
#[must_use]
pub fn fit_icon_to_text(
    icon: PositionedIcon,
    size: (f32, f32),
    text: (f32, f32, f32, f32),
    fit: IconTextFit,
    padding: (f32, f32, f32, f32),
    offset: [f32; 2],
    font_scale: f32,
) -> PositionedIcon {
    let (text_top, text_bottom, text_left, text_right) = text;
    let (pad_top, pad_bottom, pad_left, pad_right) = padding;
    let (left, right) = (text_left * font_scale, text_right * font_scale);
    let (top, bottom) = (text_top * font_scale, text_bottom * font_scale);

    let mut out = icon;
    if matches!(fit, IconTextFit::Width | IconTextFit::Both) {
        out.left = offset[0] + left - pad_left;
        out.right = offset[0] + right + pad_right;
    } else {
        out.left = offset[0] + (left + right - size.0) / 2.0;
        out.right = out.left + size.0;
    }

    if matches!(fit, IconTextFit::Height | IconTextFit::Both) {
        out.top = offset[1] + top - pad_top;
        out.bottom = offset[1] + bottom + pad_bottom;
    } else {
        out.top = offset[1] + (top + bottom - size.1) / 2.0;
        out.bottom = out.top + size.1;
    }
    out
}

/// Corrects a fitted icon's aspect against the sprite's `textFitWidth`/`textFitHeight`.
///
/// mbgl's `PositionedIcon::applyTextFit`. Fitting an icon to its text stretches it in whichever
/// directions `icon-text-fit` names, and a shield stretched freely in both looks wrong — the
/// sprite says which of its axes may stretch and which must keep the content box's proportions.
///
/// Only a `proportional` axis does anything. With both `stretchOrShrink`, or with neither field
/// set, the content rectangle already matches the content and there is nothing to correct.
#[must_use]
pub fn apply_text_fit(
    icon: PositionedIcon,
    content: (f32, f32, f32, f32),
    fit_width: Option<TextFit>,
    fit_height: Option<TextFit>,
) -> PositionedIcon {
    let (Some(fit_width), Some(fit_height)) = (fit_width, fit_height) else {
        return icon;
    };

    let width = icon.right - icon.left;
    let height = icon.bottom - icon.top;
    let (content_left, content_top, content_right, content_bottom) = content;
    let content_width = content_right - content_left;
    let content_height = content_bottom - content_top;
    if content_height == 0.0 || height == 0.0 || width == 0.0 {
        return icon;
    }
    let aspect = content_width / content_height;

    let mut out = icon;
    if fit_height == TextFit::Proportional {
        if (fit_width == TextFit::StretchOnly && (width / height) < aspect)
            || fit_width == TextFit::Proportional
        {
            let new_width = (height * aspect).ceil();
            out.left *= new_width / width;
            out.right = out.left + new_width;
        }
    } else if fit_width == TextFit::Proportional
        && fit_height == TextFit::StretchOnly
        && aspect != 0.0
        && (width / height) > aspect
    {
        let new_height = (width / aspect).ceil();
        out.top *= new_height / height;
        out.bottom = out.top + new_height;
    }
    out
}

/// The quad an icon draws as.
///
/// mbgl's `getIconQuad`. `rotate` is `icon-rotate` in radians, applied about the anchor the way
/// a glyph's is — which is why it rotates the corners rather than the box: the atlas rectangle
/// does not turn with the icon.
#[must_use]
pub fn icon_quad(icon: PositionedIcon, tex: Rect, radians: f32) -> Quad {
    let left = icon.left - ICON_QUAD_BORDER;
    let top = icon.top - ICON_QUAD_BORDER;
    let right = icon.right + ICON_QUAD_BORDER;
    let bottom = icon.bottom + ICON_QUAD_BORDER;

    let mut quad = Quad {
        tl: (left, top),
        tr: (right, top),
        bl: (left, bottom),
        br: (right, bottom),
        tex,
        glyph_offset: (0.0, 0.0),
    };

    if radians != 0.0 {
        let (sin, cos) = radians.sin_cos();
        quad.tl = rotate(quad.tl, sin, cos);
        quad.tr = rotate(quad.tr, sin, cos);
        quad.bl = rotate(quad.bl, sin, cos);
        quad.br = rotate(quad.br, sin, cos);
    }
    quad
}
