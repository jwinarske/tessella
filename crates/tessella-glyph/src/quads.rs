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
    /// Whether it samples a signed distance field rather than a picture.
    ///
    /// True for every glyph, and for an image only when its sprite is an SDF. It is per quad
    /// because a label with an image in it draws both from one buffer, and the shader has to
    /// recolor one and not the other.
    pub sdf: bool,
    /// Which section of the label the glyph came from.
    ///
    /// Carried so the caller can group a label's quads by the face each section is set in: a
    /// rectangle is only meaningful against the atlas it was packed into, and a drawable binds
    /// one texture.
    pub section: u16,
    /// The part of the corner offset that does *not* scale with the symbol, in logical pixels,
    /// as the top-left corner's pair and then the bottom-right corner's.
    ///
    /// mbgl's `pixelOffsetTL` and `pixelOffsetBR`. Zero for every quad but a cell of a
    /// stretched icon's grid, which is what the pair is for: the picture's fixed parts must keep
    /// their own size while the stretched band grows with the label, so a cell's corner is an em
    /// term plus a pixel term and the shader adds them separately.
    pub pixel_offset: ((f32, f32), (f32, f32)),
    /// The floor the shader raises this quad's font scale to.
    ///
    /// mbgl's `minFontScale`. Zero outside a stretched icon's grid. It is what stops a shield
    /// shrinking below the size its fixed parts already occupy.
    pub min_font_scale: (f32, f32),
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
    /// re-centered in its column, which is what keeps a scaled glyph or an image from sitting
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
    F: FnMut(&crate::shaping::PositionedGlyph) -> Option<Placed>,
{
    let mut quads = Vec::new();
    let (sin, cos) = if options.text_rotate == 0.0 {
        (0.0, 1.0)
    } else {
        options.text_rotate.sin_cos()
    };

    for line in &shaping.lines {
        for glyph in &line.glyphs {
            // An image section is not in the font and has no codepoint to ask about: its
            // rectangle is in the icon atlas and its metrics are the sprite's own size. mbgl
            // synthesizes exactly these — a left bearing of the atlas padding, a top of minus
            // the glyph border, and the display size as the width and height — so that
            // everything below can treat a sprite as a very large glyph.
            let (rect, metrics, rect_buffer, pixel_ratio, sdf) = match glyph.image {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                Some(image) => (
                    image.rect,
                    Metrics {
                        width: image.size.0 as u32,
                        height: image.size.1 as u32,
                        left: SPRITE_PADDING,
                        top: -(BORDER as i32),
                        // The advance is the side it occupies, which is not the same side
                        // going down as going across.
                        advance: if glyph.vertical {
                            image.size.1 as u32
                        } else {
                            image.size.0 as u32
                        },
                    },
                    // The padding is in texture pixels and everything here is in logical ones,
                    // so a sprite drawn at two texture pixels per logical one carries half the
                    // buffer a glyph does.
                    #[allow(clippy::cast_precision_loss)]
                    {
                        SPRITE_PADDING as f32 / image.pixel_ratio
                    },
                    image.pixel_ratio,
                    image.sdf,
                ),
                None => {
                    let Some(Placed { rect, metrics }) = placed(glyph) else {
                        continue;
                    };
                    (rect, metrics, RECT_BUFFER, 1.0, true)
                }
            };
            // A glyph with no area has nothing to sample. It should not have been placed, but
            // an atlas that ran out of room can hand one back.
            if rect.width == 0 || rect.height == 0 {
                continue;
            }

            // Scaled by the section's `font-scale`, as every measurement of this glyph is:
            // mbgl reads `positionedGlyph.scale` at each of them. A glyph twice the size is
            // twice as wide, sits twice as far from its own center, and covers twice as much of
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

            // Every glyph on a vertical line is re-centered in its column. The column is one em
            // wide and what sits in it need not be: a scaled glyph is wider or narrower than its
            // line's cell, and the correction is the difference. mbgl folds the line's own offset
            // in here as well, which is what pushes a line down that an oversized image grew.
            let line_offset = if options.allow_vertical_placement && shaping.verticalizable {
                #[allow(clippy::cast_precision_loss)]
                let centered = match glyph.image {
                    // An image is centered on its own width in the column.
                    Some(_) => (crate::text::ONE_EM - metrics.width as f32 * glyph.scale) / 2.0,
                    // A scaled glyph is offset by how much bigger than the column it is, with
                    // the sign the other way round — which is why mbgl writes the two as one
                    // subtraction and negates the image half.
                    None => -(glyph.scale - 1.0) * crate::text::ONE_EM,
                };
                line.offset / 2.0 + centered
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
            let x1 = (metrics.left as f32 - rect_buffer) * glyph.scale - half_advance + built_in.0;
            #[allow(clippy::cast_precision_loss)]
            let y1 = (-(metrics.top as f32) - rect_buffer) * glyph.scale + built_in.1;
            #[allow(clippy::cast_precision_loss)]
            let x2 = x1 + rect.width as f32 * glyph.scale / pixel_ratio;
            #[allow(clippy::cast_precision_loss)]
            let y2 = y1 + rect.height as f32 * glyph.scale / pixel_ratio;

            let mut quad = Quad {
                tl: (x1, y1),
                tr: (x2, y1),
                bl: (x1, y2),
                br: (x2, y2),
                tex: rect,
                glyph_offset,
                sdf,
                section: glyph.section,
                // A glyph's corners are all em: nothing in a label is held at a fixed pixel size.
                pixel_offset: ((0.0, 0.0), (0.0, 0.0)),
                min_font_scale: (0.0, 0.0),
            };

            if rotate_vertical {
                // A glyph that stays upright on a vertical line is drawn from a horizontal
                // layout, so the label is rotated a quarter turn clockwise and each such glyph a
                // quarter turn back. The center is the middle of the left edge of its own em
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
                // An image turned on its side is pulled up by the same difference the
                // correction pushes it along by, because its box is not a square em and the two
                // axes do not cancel the way a glyph's do.
                let image_correction = if glyph.image.is_some() {
                    half_width
                } else {
                    0.0
                };
                let correction = (5.0 - Y_OFFSET - half_width, -image_correction);
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

/// The padding the icon atlas reserves around each sprite, in texture pixels.
///
/// mbgl's `ImagePosition::padding`. It is the sprite's counterpart to [`RECT_BUFFER`] and it is
/// three pixels smaller, because a picture needs only enough room that linear filtering cannot
/// reach a neighbor, while a distance field needs a border wide enough to carry the field
/// itself. An image drawn inline in a label uses this where a glyph uses the other.
pub const SPRITE_PADDING: i32 = 1;

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
    /// Stretch it to the text's width, and center it vertically.
    Width,
    /// Stretch it to the text's height, and center it horizontally.
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
/// statement about where the icon goes relative to the text, and honoring the anchor as well
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

/// One edge of a cell of a stretched icon's grid, split into its two terms.
///
/// mbgl's `Cut`. `stretch` is the distance along the picture measured in the columns that may
/// stretch, `fixed` the distance measured in the ones that may not — and keeping them apart is
/// the whole mechanism: the first scales with the label and the second does not.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Cut {
    fixed: f32,
    stretch: f32,
}

/// How much of `zones` lies between `min` and `max` — mbgl's `sumWithinRange`.
///
/// Each end is held inside the window before it is measured, so a zone that straddles an edge
/// contributes only the part inside and one entirely outside contributes nothing.
///
/// Written as mbgl writes it rather than as a `clamp`, which panics when a window is inverted.
/// Nothing validates that a sprite index's `content` reads left-to-right, so an entry spelling
/// it the other way round has to come out as zero rather than as an abort.
fn sum_within_range(zones: &[(f32, f32)], min: f32, max: f32) -> f32 {
    let held = |value: f32| value.min(max).max(min);
    zones.iter().map(|&(from, to)| held(to) - held(from)).sum()
}

/// The cuts one axis of a stretchable picture is divided at — mbgl's `stretchZonesToCuts`.
///
/// Two cuts per zone, at the same point: one closing the fixed run before it and one opening the
/// stretchable run. That is what makes the grid's cells alternate fixed, stretched, fixed, and
/// why a picture with two zones along an axis is cut into five parts rather than three.
///
/// The first and last cut reach a pixel outside the picture, which is the border every icon quad
/// carries so that a sample at the edge lands on the atlas padding rather than on a neighbor.
fn stretch_zones_to_cuts(zones: &[(f32, f32)], fixed_size: f32, stretch_size: f32) -> Vec<Cut> {
    let border = SPRITE_PADDING as f32;
    let mut cuts = Vec::with_capacity(2 * zones.len() + 2);
    cuts.push(Cut {
        fixed: -border,
        stretch: 0.0,
    });
    for &(from, to) in zones {
        let last = cuts[cuts.len() - 1].stretch;
        cuts.push(Cut {
            fixed: from - last,
            stretch: last,
        });
        cuts.push(Cut {
            fixed: from - last,
            stretch: last + (to - from),
        });
    }
    cuts.push(Cut {
        fixed: fixed_size + border,
        stretch: stretch_size,
    });
    cuts
}

/// The quads an icon draws as — mbgl's `getIconQuads`.
///
/// One quad for a plain icon, and a grid of them for a stretchable one under `icon-text-fit`.
///
/// # Why a shield is more than one quad
///
/// A sprite may name the columns and rows of itself that are allowed to stretch. Drawing it as a
/// single quad scaled to the label stretches its rounded ends and its border along with the
/// middle, which is what a route shield must not do. So the picture is cut along those zones and
/// each cell is drawn as its own quad: a cell in a fixed column keeps its pixel width whatever
/// the label does, and only the cells in a stretchable one grow.
///
/// A cell's corner is therefore two terms rather than one — a distance in em that scales with
/// the symbol, and a distance in pixels that does not. [`Quad::pixel_offset`] carries the
/// second, and the shader adds them after scaling the first.
///
/// `has_text_fit` is the layer's `icon-text-fit`, not whether a label was found: mbgl asks the
/// layer, and an icon told to fit text it does not have still takes this path.
///
/// `radians` is `icon-rotate`, applied about the anchor the way a glyph's is — which is why it
/// rotates the corners rather than the box: the atlas rectangle does not turn with the icon.
#[must_use]
pub fn icon_quads(
    icon: PositionedIcon,
    position: &crate::sprite::IconPosition,
    radians: f32,
    has_text_fit: bool,
) -> Vec<Quad> {
    let border = SPRITE_PADDING as f32;
    #[allow(clippy::cast_possible_truncation)]
    let pixel_ratio = position.pixel_ratio as f32;
    #[allow(clippy::cast_precision_loss)]
    let image_width = position.padded_rect.width as f32 - 2.0 * border;
    #[allow(clippy::cast_precision_loss)]
    let image_height = position.padded_rect.height as f32 - 2.0 * border;

    // A picture that names no zones is treated as one zone covering all of it, which is what
    // makes the single-quad branch below fall out of the same arithmetic rather than needing
    // its own.
    #[allow(clippy::cast_possible_truncation)]
    let zones = |named: &[crate::sprite::Stretch], whole: f32| -> Vec<(f32, f32)> {
        if named.is_empty() {
            vec![(0.0, whole)]
        } else {
            named.iter().map(|s| (s.from as f32, s.to as f32)).collect()
        }
    };
    let stretch_x = zones(&position.stretch_x, image_width);
    let stretch_y = zones(&position.stretch_y, image_height);

    let stretch_width: f32 = stretch_x.iter().map(|&(from, to)| to - from).sum();
    let stretch_height: f32 = stretch_y.iter().map(|&(from, to)| to - from).sum();
    let fixed_width = image_width - stretch_width;
    let fixed_height = image_height - stretch_height;

    let mut stretch_offset_x = 0.0;
    let mut stretch_content_width = stretch_width;
    let mut stretch_offset_y = 0.0;
    let mut stretch_content_height = stretch_height;
    let mut fixed_offset_x = 0.0;
    let mut fixed_content_width = fixed_width;
    let mut fixed_offset_y = 0.0;
    let mut fixed_content_height = fixed_height;

    // Fitting measures the *content* box, so the grid's arithmetic has to be relative to it
    // rather than to the whole picture: a shield's label grows the columns inside the content
    // area, and the border outside it is carried along unchanged.
    if has_text_fit && let Some(content) = position.content {
        #[allow(clippy::cast_possible_truncation)]
        let (left, top, right, bottom) = (
            content.left as f32,
            content.top as f32,
            content.right as f32,
            content.bottom as f32,
        );
        stretch_offset_x = sum_within_range(&stretch_x, 0.0, left);
        stretch_offset_y = sum_within_range(&stretch_y, 0.0, top);
        stretch_content_width = sum_within_range(&stretch_x, left, right);
        stretch_content_height = sum_within_range(&stretch_y, top, bottom);
        fixed_offset_x = left - stretch_offset_x;
        fixed_offset_y = top - stretch_offset_y;
        fixed_content_width = (right - left) - stretch_content_width;
        fixed_content_height = (bottom - top) - stretch_content_height;
    }

    let icon_left = icon.left;
    let icon_top = icon.top;
    let icon_width = icon.right - icon_left;
    let icon_height = icon.bottom - icon_top;
    let turn = (radians != 0.0).then(|| radians.sin_cos());

    let mut quads = Vec::new();
    let mut make_box = |left: Cut, top: Cut, right: Cut, bottom: Cut| {
        let em = |stretch: f32, offset: f32, content: f32, size: f32, start: f32| {
            start + size * (stretch - offset) / content
        };
        let px = |fixed: f32, offset: f32, content: f32, stretch: f32, size: f32| {
            (fixed - offset) - content * stretch / size
        };

        let left_em = em(
            left.stretch,
            stretch_offset_x,
            stretch_content_width,
            icon_width,
            icon_left,
        );
        let right_em = em(
            right.stretch,
            stretch_offset_x,
            stretch_content_width,
            icon_width,
            icon_left,
        );
        let top_em = em(
            top.stretch,
            stretch_offset_y,
            stretch_content_height,
            icon_height,
            icon_top,
        );
        let bottom_em = em(
            bottom.stretch,
            stretch_offset_y,
            stretch_content_height,
            icon_height,
            icon_top,
        );

        let left_px = px(
            left.fixed,
            fixed_offset_x,
            fixed_content_width,
            left.stretch,
            stretch_width,
        );
        let right_px = px(
            right.fixed,
            fixed_offset_x,
            fixed_content_width,
            right.stretch,
            stretch_width,
        );
        let top_px = px(
            top.fixed,
            fixed_offset_y,
            fixed_content_height,
            top.stretch,
            stretch_height,
        );
        let bottom_px = px(
            bottom.fixed,
            fixed_offset_y,
            fixed_content_height,
            bottom.stretch,
            stretch_height,
        );

        let mut corners = [
            (left_em, top_em),
            (right_em, top_em),
            (left_em, bottom_em),
            (right_em, bottom_em),
        ];
        if let Some((sin, cos)) = turn {
            for corner in &mut corners {
                *corner = rotate(*corner, sin, cos);
            }
        }

        // The cell's rectangle in the atlas, measured from the same two terms: a cut's distance
        // along the picture is its stretched part plus its fixed part, whatever the label did.
        let x1 = left.stretch + left.fixed;
        let x2 = right.stretch + right.fixed;
        let y1 = top.stretch + top.fixed;
        let y2 = bottom.stretch + bottom.fixed;
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let tex = Rect {
            x: (position.padded_rect.x as f32 + border + x1) as u32,
            y: (position.padded_rect.y as f32 + border + y1) as u32,
            width: (x2 - x1) as u32,
            height: (y2 - y1) as u32,
        };

        quads.push(Quad {
            tl: corners[0],
            tr: corners[1],
            bl: corners[2],
            br: corners[3],
            tex,
            glyph_offset: (0.0, 0.0),
            // An icon layer's drawable is bound to one shader or the other by the sprite's own
            // flag, so its quads do not each need to carry one. The field is here because the
            // type is shared with a label's, where a quad *is* what decides.
            sdf: false,
            // An icon is not a text section, and the label's own face answers for it.
            section: 0,
            pixel_offset: (
                (left_px / pixel_ratio, top_px / pixel_ratio),
                (right_px / pixel_ratio, bottom_px / pixel_ratio),
            ),
            min_font_scale: (
                fixed_content_width / pixel_ratio / icon_width,
                fixed_content_height / pixel_ratio / icon_height,
            ),
        });
    };

    if !has_text_fit || (position.stretch_x.is_empty() && position.stretch_y.is_empty()) {
        // One quad over the whole picture, a texture pixel proud of it on every side. mbgl's
        // comment says why: a ten-pixel icon that is not aligned to the pixel grid covers eleven
        // actual pixels, so a quad sized to the icon clips a sliver off one edge, and the extra
        // pixel samples the atlas padding the packer reserved for it.
        //
        // It is written as a cut rather than added to the corners because the pad is a *texture*
        // pixel: the arithmetic below carries it through the icon-to-image ratio, so an icon
        // drawn from a retina sprite pads by half a logical pixel rather than a whole one.
        make_box(
            Cut {
                fixed: 0.0,
                stretch: -1.0,
            },
            Cut {
                fixed: 0.0,
                stretch: -1.0,
            },
            Cut {
                fixed: 0.0,
                stretch: image_width + 1.0,
            },
            Cut {
                fixed: 0.0,
                stretch: image_height + 1.0,
            },
        );
    } else {
        let x_cuts = stretch_zones_to_cuts(&stretch_x, fixed_width, stretch_width);
        let y_cuts = stretch_zones_to_cuts(&stretch_y, fixed_height, stretch_height);
        for xi in 0..x_cuts.len() - 1 {
            for yi in 0..y_cuts.len() - 1 {
                make_box(x_cuts[xi], y_cuts[yi], x_cuts[xi + 1], y_cuts[yi + 1]);
            }
        }
    }

    quads
}
