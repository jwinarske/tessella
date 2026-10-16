//! An icon's box and the quads that draw it — mbgl's `shapeIcon` and `getIconQuads`.
//!
//! Two steps rather than one, and the difference between them is a pixel on every side. The box
//! is what collision measures; the quads are what draw, and they are padded because a ten-pixel
//! icon that is not aligned to the pixel grid covers eleven actual pixels.

use tessella_glyph::atlas::Rect;
use tessella_glyph::quads::{Quad, icon_quads, shape_icon};
use tessella_glyph::shaping::Anchor;
use tessella_glyph::sprite::{IconPosition, Stretch};

/// A 24-logical-pixel icon somewhere in the atlas.
///
/// The rectangle is 26 square because [`IconPosition::padded_rect`] includes the pixel of
/// padding on each side; the picture inside it is 24.
fn tex() -> Rect {
    Rect {
        x: 64,
        y: 32,
        width: 26,
        height: 26,
    }
}

/// An atlas entry for `tex()` with no stretch zones, at the given pixel ratio.
fn plain(pixel_ratio: f64) -> IconPosition {
    IconPosition {
        padded_rect: tex(),
        pixel_ratio,
        sdf: false,
        content: None,
        stretch_x: Vec::new(),
        stretch_y: Vec::new(),
        text_fit_width: None,
        text_fit_height: None,
    }
}

/// The one quad a plain icon draws as.
fn only(quads: Vec<Quad>) -> Quad {
    assert_eq!(quads.len(), 1, "a plain icon is one quad");
    quads.into_iter().next().unwrap()
}

/// A centered icon straddles its anchor.
#[test]
fn a_centered_icon_straddles_its_anchor() {
    let icon = shape_icon((24.0, 24.0), [0.0, 0.0], Anchor::Center);
    assert_eq!((icon.left, icon.right), (-12.0, 12.0));
    assert_eq!((icon.top, icon.bottom), (-12.0, 12.0));
}

/// The anchor names the part of the icon that touches the point.
///
/// So `Top` puts the icon *below* the point, which is the reading that catches people out and
/// the one mbgl uses. Getting it inverted places every marker on the wrong side of what it
/// marks, consistently, which reads as a style problem rather than a layout one.
#[test]
fn the_anchor_names_the_part_that_touches_the_point() {
    let at = |anchor| shape_icon((24.0, 24.0), [0.0, 0.0], anchor);

    let top = at(Anchor::Top);
    assert_eq!((top.top, top.bottom), (0.0, 24.0), "Top is below the point");

    let bottom = at(Anchor::Bottom);
    assert_eq!(
        (bottom.top, bottom.bottom),
        (-24.0, 0.0),
        "Bottom is above it"
    );

    let left = at(Anchor::Left);
    assert_eq!((left.left, left.right), (0.0, 24.0), "Left is right of it");

    let corner = at(Anchor::BottomRight);
    assert_eq!((corner.left, corner.right), (-24.0, 0.0));
    assert_eq!((corner.top, corner.bottom), (-24.0, 0.0));
}

/// The offset moves the icon and does not resize it.
#[test]
fn the_offset_moves_the_box() {
    let icon = shape_icon((24.0, 24.0), [10.0, -6.0], Anchor::Center);
    assert_eq!((icon.left, icon.right), (-2.0, 22.0));
    assert_eq!((icon.top, icon.bottom), (-18.0, 6.0));
    assert_eq!(icon.right - icon.left, 24.0, "the offset resized it");
}

/// A retina sprite is placed at its logical size, not its sheet size.
///
/// `shape_icon` takes logical pixels, and handing it the sheet size draws every `@2x` icon at
/// twice the size — which looks like a broken sprite sheet rather than a unit mix-up.
#[test]
fn a_retina_icon_is_placed_at_its_logical_size() {
    let sheet = tessella_glyph::sprite::Sprite {
        x: 0,
        y: 0,
        width: 48,
        height: 48,
        pixel_ratio: 2.0,
        sdf: false,
        stretch_x: Vec::new(),
        stretch_y: Vec::new(),
        content: None,
        text_fit_width: None,
        text_fit_height: None,
    };
    let (width, height) = sheet.logical_size();
    #[allow(clippy::cast_possible_truncation)]
    let icon = shape_icon((width as f32, height as f32), [0.0, 0.0], Anchor::Center);
    assert_eq!(icon.right - icon.left, 24.0);
    assert_eq!(icon.bottom - icon.top, 24.0);
}

/// The quad is the box grown by a pixel on every side.
///
/// The pad is on the quad and not on the texture rectangle: the extra pixel samples the atlas
/// padding, which is why the atlas reserves it. Padding the rectangle instead would sample the
/// neighboring icon.
#[test]
fn the_quad_is_the_box_grown_by_a_pixel() {
    let icon = shape_icon((24.0, 24.0), [0.0, 0.0], Anchor::Center);
    let quad = only(icon_quads(icon, &plain(1.0), 0.0, false));

    assert_eq!(quad.tl, (-13.0, -13.0));
    assert_eq!(quad.br, (13.0, 13.0));
    assert_eq!(
        quad.tr.0 - quad.tl.0,
        24.0 + 2.0,
        "the pad is on one side only"
    );

    assert_eq!(quad.tex, tex(), "the rectangle grew with the quad");
    assert_eq!(
        (quad.pixel_offset, quad.min_font_scale),
        (((0.0, 0.0), (0.0, 0.0)), (0.0, 0.0)),
        "a plain icon holds nothing at a fixed pixel size"
    );
    assert_eq!(
        quad.glyph_offset,
        (0.0, 0.0),
        "an icon has no place along a line"
    );
}

/// The corners keep their pairing, so the icon is not drawn mirrored.
#[test]
fn the_corners_are_in_mbgls_order() {
    let icon = shape_icon((10.0, 6.0), [0.0, 0.0], Anchor::Center);
    let quad = only(icon_quads(icon, &plain(1.0), 0.0, false));

    assert!(quad.tl.0 < quad.tr.0, "top-left is left of top-right");
    assert!(quad.tl.1 < quad.bl.1, "top-left is above bottom-left");
    assert_eq!(quad.tl.1, quad.tr.1, "the top edge is level");
    assert_eq!(quad.bl.1, quad.br.1, "and so is the bottom");
    assert_eq!(quad.tl.0, quad.bl.0, "the left edge is straight");
}

/// `icon-rotate` turns the corners and leaves the rectangle alone.
#[test]
fn rotation_turns_the_corners_and_not_the_texture() {
    let icon = shape_icon((24.0, 24.0), [0.0, 0.0], Anchor::Center);
    let quarter = core::f32::consts::FRAC_PI_2;
    let quad = only(icon_quads(icon, &plain(1.0), quarter, false));

    // A quarter turn takes the top-left corner to where the bottom-left was.
    assert!((quad.tl.0 - 13.0).abs() < 0.001, "{:?}", quad.tl);
    assert!((quad.tl.1 + 13.0).abs() < 0.001, "{:?}", quad.tl);
    assert_eq!(quad.tex, tex(), "the atlas rectangle turned with the icon");

    // The quad is still a square of the same size, just turned.
    let side = ((quad.tr.0 - quad.tl.0).powi(2) + (quad.tr.1 - quad.tl.1).powi(2)).sqrt();
    assert!((side - 26.0).abs() < 0.001, "{side}");
}

/// A zero rotation leaves the corners exactly where they were.
///
/// Not a rounding question: `sin_cos` of zero is exact, but a rotation applied unconditionally
/// would still pass every value through a multiply, and the branch is what mbgl has.
#[test]
fn no_rotation_leaves_the_corners_untouched() {
    let mut position = plain(1.0);
    position.padded_rect.height = 20;
    let icon = shape_icon((24.0, 18.0), [3.0, -2.0], Anchor::TopLeft);
    let turned = only(icon_quads(icon, &position, 0.0, false));
    assert_eq!(turned.tl, (icon.left - 1.0, icon.top - 1.0));
    assert_eq!(turned.br, (icon.right + 1.0, icon.bottom + 1.0));
}

/// The pad is a *texture* pixel, so a retina sprite pads by half a logical one.
///
/// mbgl writes the border as a cut rather than as an addition to the corners, and the difference
/// only shows where the icon and its picture are not the same size. A whole logical pixel around
/// a 2x sprite is twice the border the sheet's author drew, which reads as a fuzzy icon rather
/// than as a units mistake — and it is the same arithmetic that holds a stretched shield's fixed
/// parts at their own size, so getting it wrong here is getting it wrong there.
#[test]
fn a_retina_icons_pad_is_half_a_logical_pixel() {
    let mut position = plain(2.0);
    // A 48-pixel picture at 2x displays at 24 logical, in a 50-square padded rectangle.
    position.padded_rect.width = 50;
    position.padded_rect.height = 50;

    let icon = shape_icon((24.0, 24.0), [0.0, 0.0], Anchor::Center);
    let quad = only(icon_quads(icon, &position, 0.0, false));

    assert_eq!(quad.tl, (-12.5, -12.5));
    assert_eq!(quad.br, (12.5, 12.5));
    assert_eq!(
        quad.tex, position.padded_rect,
        "the whole rectangle is read"
    );
}

/// Stretching an icon around its label — mbgl's `icon-text-fit` and `applyTextFit`.
///
/// This is what draws a route shield: a sprite whose middle stretches to hold a number, and
/// whose border must not stretch with it. Two mechanisms, and they are not the same one. The
/// *layer* says `icon-text-fit` — which axes to stretch — and the *sprite* says `textFitWidth`
/// and `textFitHeight`, which constrain how far that stretch may distort it.
mod text_fit {
    use tessella_glyph::quads::{IconTextFit, apply_text_fit, content_padding, fit_icon_to_text};
    use tessella_glyph::sprite::TextFit;

    /// mbgl's `Shaping.applyTextFit` setup: 4-unit text at font scale 4, so a 16x16 icon.
    const FONT_SCALE: f32 = 4.0;
    const TEXT: (f32, f32, f32, f32) = (-2.0, 2.0, -2.0, 2.0);
    const FITTED: f32 = 16.0;

    /// An icon fitted to that text in both directions.
    fn fitted() -> tessella_glyph::quads::PositionedIcon {
        fit_icon_to_text(
            tessella_glyph::quads::PositionedIcon::default(),
            (FITTED, FITTED),
            TEXT,
            IconTextFit::Both,
            (0.0, 0.0, 0.0, 0.0),
            [0.0, 0.0],
            FONT_SCALE,
        )
    }

    /// Fitting both axes gives the text's box.
    #[test]
    fn fitting_both_axes_takes_the_texts_box() {
        let icon = fitted();
        assert_eq!(icon.right - icon.left, FITTED);
        assert_eq!(icon.bottom - icon.top, FITTED);
    }

    /// Fitting one axis centers the other rather than stretching it.
    ///
    /// mbgl's `else` branches, and the reason they are not "leave it alone": the icon has to move
    /// to sit on the text even where it does not resize, or a width-fitted shield stretches
    /// across the label while sitting above it.
    #[test]
    fn fitting_one_axis_centers_the_other() {
        let wide = fit_icon_to_text(
            tessella_glyph::quads::PositionedIcon::default(),
            (40.0, 10.0),
            TEXT,
            IconTextFit::Width,
            (0.0, 0.0, 0.0, 0.0),
            [0.0, 0.0],
            FONT_SCALE,
        );
        assert_eq!(wide.right - wide.left, FITTED, "the width did not fit");
        assert_eq!(wide.bottom - wide.top, 10.0, "the height was stretched");
        // Centered on the text's vertical middle, which is zero here.
        assert_eq!(wide.top + wide.bottom, 0.0, "{wide:?} is not centered");
    }

    /// `icon-text-fit-padding` grows the fitted box, per side.
    #[test]
    fn the_fit_padding_grows_each_side_on_its_own() {
        let padded = fit_icon_to_text(
            tessella_glyph::quads::PositionedIcon::default(),
            (FITTED, FITTED),
            TEXT,
            IconTextFit::Both,
            (1.0, 2.0, 3.0, 4.0),
            [0.0, 0.0],
            FONT_SCALE,
        );
        assert_eq!(padded.top, -8.0 - 1.0);
        assert_eq!(padded.bottom, 8.0 + 2.0);
        assert_eq!(padded.left, -8.0 - 3.0);
        assert_eq!(padded.right, 8.0 + 4.0);
    }

    /// mbgl `Shaping.applyTextFit`, horizontal: a 100x20 sprite with a 5,5,95,15 content box.
    ///
    /// The content is 90x10, so its aspect is nine to one. Fitted to a square, an axis marked
    /// `proportional` pulls the icon back to that aspect: 144 by 16.
    #[test]
    fn a_proportional_axis_restores_the_content_aspect() {
        let content = (5.0, 5.0, 95.0, 15.0);

        // Neither field set: nothing happens.
        let untouched = apply_text_fit(fitted(), content, None, None);
        assert_eq!(untouched.right - untouched.left, FITTED);
        assert_eq!(untouched.bottom - untouched.top, FITTED);

        // Both stretchOrShrink: still nothing, because the content already matches.
        let free = apply_text_fit(
            fitted(),
            content,
            Some(TextFit::StretchOrShrink),
            Some(TextFit::StretchOrShrink),
        );
        assert_eq!(free.right - free.left, FITTED);
        assert_eq!(free.bottom - free.top, FITTED);

        // stretchOnly width against a proportional height: widened to nine times the height.
        let corrected = apply_text_fit(
            fitted(),
            content,
            Some(TextFit::StretchOnly),
            Some(TextFit::Proportional),
        );
        assert_eq!(corrected.right - corrected.left, FITTED * 9.0);
        assert_eq!(corrected.bottom - corrected.top, FITTED);
    }

    /// mbgl `Shaping.applyTextFit`, vertical: a 20x100 sprite with a 5,5,15,95 content box.
    ///
    /// The mirror image, and worth having separately: the two branches are written out rather
    /// than shared, so one can be right while the other is not.
    #[test]
    fn the_vertical_branch_mirrors_the_horizontal_one() {
        let content = (5.0, 5.0, 15.0, 95.0);
        let corrected = apply_text_fit(
            fitted(),
            content,
            Some(TextFit::Proportional),
            Some(TextFit::StretchOnly),
        );
        assert_eq!(corrected.right - corrected.left, FITTED);
        assert_eq!(corrected.bottom - corrected.top, FITTED * 9.0);
    }

    /// The content margins are in logical pixels, so a retina shield's border matches a plain
    /// one's.
    ///
    /// `content` is in the sprite's own pixels. Leaving the ratio out gives a 2x shield twice
    /// the border, which reads as two differently drawn shields rather than as a units mistake.
    #[test]
    fn the_content_margins_divide_out_the_pixel_ratio() {
        // A 100x20 sprite at ratio 1 displays at 100x20; its content inset is 5 all round.
        let plain = content_padding((100.0, 20.0), (5.0, 5.0, 95.0, 15.0), 1.0);
        assert_eq!(plain, (5.0, 5.0, 5.0, 5.0));

        // The same drawing at 2x is a 200x40 sprite displaying at 100x20, and its border is
        // still five logical pixels.
        let retina = content_padding((100.0, 20.0), (10.0, 10.0, 190.0, 30.0), 2.0);
        assert_eq!(retina, (5.0, 5.0, 5.0, 5.0));
    }
}

/// Cutting a stretchable picture into its grid — mbgl's `getIconQuads` stretched branch.
///
/// The sprites are maplibre-native's own `sprites/stretch`, which is what `tests/sprite-fixtures`
/// carries: a sprite names the columns and rows of itself that may stretch, and under
/// `icon-text-fit` the picture is drawn as a grid of quads rather than one. The point of the
/// grid is that a cell in a fixed column keeps its pixel width whatever the label does.
mod stretch {
    use super::{Anchor, IconPosition, Quad, Rect, Stretch, icon_quads, shape_icon};
    use tessella_glyph::quads::{IconTextFit, fit_icon_to_text};
    use tessella_glyph::sprite::Content;

    fn zone(from: f64, to: f64) -> Stretch {
        Stretch { from, to }
    }

    /// `nine-part-content` from maplibre-native's `sprites/stretch.json`, as it reaches layout.
    ///
    /// A 40-pixel square at ratio 2 — so 20 logical — whose middle twenty pixels stretch on both
    /// axes, with a content box over the same middle.
    fn nine_part() -> IconPosition {
        IconPosition {
            padded_rect: Rect {
                x: 8,
                y: 16,
                width: 42,
                height: 42,
            },
            pixel_ratio: 2.0,
            sdf: false,
            content: Some(Content {
                left: 10.0,
                top: 10.0,
                right: 30.0,
                bottom: 30.0,
            }),
            stretch_x: vec![zone(10.0, 30.0)],
            stretch_y: vec![zone(10.0, 30.0)],
            text_fit_width: None,
            text_fit_height: None,
        }
    }

    /// `fifteen-part`: 70x40 at ratio 1, with two stretchable columns and one stretchable row.
    fn fifteen_part() -> IconPosition {
        IconPosition {
            padded_rect: Rect {
                x: 100,
                y: 0,
                width: 72,
                height: 42,
            },
            pixel_ratio: 1.0,
            sdf: false,
            content: None,
            stretch_x: vec![zone(10.0, 30.0), zone(40.0, 60.0)],
            stretch_y: vec![zone(10.0, 30.0)],
            text_fit_width: None,
            text_fit_height: None,
        }
    }

    /// An icon of `position` stretched around a label `width` logical pixels across.
    fn fitted(position: &IconPosition, width: f32) -> Vec<Quad> {
        let (w, h) = position.display_size();
        #[allow(clippy::cast_possible_truncation)]
        let size = (w as f32, h as f32);
        let placed = fit_icon_to_text(
            shape_icon(size, [0.0, 0.0], Anchor::Center),
            size,
            (-6.0, 6.0, -width / 2.0, width / 2.0),
            IconTextFit::Both,
            (0.0, 0.0, 0.0, 0.0),
            [0.0, 0.0],
            1.0,
        );
        icon_quads(placed, position, 0.0, true)
    }

    /// One stretchable zone per axis cuts the picture into nine.
    #[test]
    fn one_zone_per_axis_cuts_the_picture_into_nine() {
        assert_eq!(fitted(&nine_part(), 60.0).len(), 9);
    }

    /// Two zones along one axis cut it into five there, so fifteen cells in all.
    ///
    /// Not a restatement of the one above: the cut list has *two* entries per zone, at the same
    /// point, so a second zone adds two cells rather than one. A builder that emitted one cut
    /// per zone would give nine here too.
    #[test]
    fn two_zones_along_an_axis_make_five_cells_there() {
        let quads = fitted(&fifteen_part(), 90.0);
        assert_eq!(quads.len(), 15);

        // Five distinct columns and three distinct rows, by the left edge of each cell.
        let mut lefts: Vec<_> = quads.iter().map(|q| q.tex.x).collect();
        lefts.sort_unstable();
        lefts.dedup();
        let mut tops: Vec<_> = quads.iter().map(|q| q.tex.y).collect();
        tops.sort_unstable();
        tops.dedup();
        assert_eq!((lefts.len(), tops.len()), (5, 3));
    }

    /// The cells tile the picture: no texel is read twice and none is skipped.
    ///
    /// The rectangles overlap by the one-pixel pad at the outer edge, which is deliberate, so
    /// this asks about the picture's own area rather than the padded one.
    #[test]
    fn the_cells_tile_the_picture() {
        let position = nine_part();
        let quads = fitted(&position, 60.0);
        let inside = |q: &Quad| {
            let x0 = q.tex.x.max(position.padded_rect.x + 1);
            let y0 = q.tex.y.max(position.padded_rect.y + 1);
            let x1 = (q.tex.x + q.tex.width).min(position.padded_rect.x + 41);
            let y1 = (q.tex.y + q.tex.height).min(position.padded_rect.y + 41);
            (x1 - x0) * (y1 - y0)
        };
        let covered: u32 = quads.iter().map(inside).sum();
        assert_eq!(
            covered,
            40 * 40,
            "the cells do not tile the 40-pixel picture"
        );
    }

    /// A fixed cell reads the same texels and keeps its size however wide the label is.
    ///
    /// This is the whole point of the mechanism. The corner cell's picture is the shield's
    /// rounded end, and it must not be drawn wider because the number inside grew.
    #[test]
    fn a_fixed_cell_keeps_its_size_as_the_label_grows() {
        let position = nine_part();
        let narrow = fitted(&position, 30.0);
        let wide = fitted(&position, 120.0);

        // The grid is built x-major, so the first cell is the top-left corner of both.
        assert_eq!(narrow[0].tex, wide[0].tex, "it reads different texels");

        // Em span plus pixel span, which is what the shader adds. The em part is in the symbol's
        // own units and the pixel part is not, so they are compared at a font scale of one.
        let span = |q: &Quad| (q.tr.0 - q.tl.0) + (q.pixel_offset.1.0 - q.pixel_offset.0.0);
        assert!(
            (span(&narrow[0]) - span(&wide[0])).abs() < 0.001,
            "the corner was stretched: {} against {}",
            span(&narrow[0]),
            span(&wide[0])
        );

        // And the middle cell — index 4 of the x-major grid — did grow.
        assert!(
            span(&wide[4]) > span(&narrow[4]) + 1.0,
            "the stretchable middle did not grow"
        );
    }

    /// A cell's fixed part rides in the pixel offset, not in the corner.
    ///
    /// The two terms are what the attribute is for: the corner scales with `icon-size` and the
    /// offset does not. A builder that folded the pixel term into the corner would draw a
    /// correct shield at one size and a wrong one at every other.
    #[test]
    fn the_fixed_part_rides_in_the_pixel_offset() {
        let quads = fitted(&nine_part(), 60.0);
        assert!(
            quads
                .iter()
                .any(|q| q.pixel_offset.0.0 != 0.0 || q.pixel_offset.1.0 != 0.0),
            "no cell carries a pixel term"
        );
        // The picture is 2x, and the offset is in logical pixels: the fixed border is ten sprite
        // pixels, which is five of them.
        assert!(
            (quads[0].pixel_offset.1.0 - quads[0].pixel_offset.0.0 - 5.5).abs() < 0.001,
            "{:?}",
            quads[0].pixel_offset
        );
    }

    /// The minimum font scale is the fixed part measured against the whole icon.
    ///
    /// It is the floor the shader raises `icon-size` to, so a shield cannot be drawn smaller
    /// than the parts of it that do not scale. Zero for a plain icon, which has no fixed part.
    #[test]
    fn the_min_font_scale_is_the_fixed_part_of_the_icon() {
        let quads = fitted(&nine_part(), 60.0);
        // The content box is 20 pixels wide and all of it stretches, so the fixed content width
        // is zero across, and twenty high by the same argument.
        assert_eq!(quads[0].min_font_scale, (0.0, 0.0));

        // A sprite whose content box reaches past the stretchable middle has a fixed part.
        let mut position = nine_part();
        position.content = Some(Content {
            left: 5.0,
            top: 5.0,
            right: 35.0,
            bottom: 35.0,
        });
        let quads = fitted(&position, 60.0);
        // Ten fixed pixels of content, at ratio 2, against an icon 60 wide.
        assert!(
            (quads[0].min_font_scale.0 - 5.0 / 60.0).abs() < 0.001,
            "{:?}",
            quads[0].min_font_scale
        );
    }

    /// Without `icon-text-fit` a stretchable sprite is one quad, exactly as before.
    ///
    /// mbgl asks the layer, not the sprite: a stretchable picture drawn as a plain marker is
    /// drawn whole, and this is the branch every icon on a map still takes.
    #[test]
    fn without_text_fit_a_stretchable_sprite_is_one_quad() {
        let position = nine_part();
        let icon = shape_icon((20.0, 20.0), [0.0, 0.0], Anchor::Center);
        let quads = icon_quads(icon, &position, 0.0, false);
        assert_eq!(quads.len(), 1);
        assert_eq!(quads[0].tex, position.padded_rect);
    }

    /// And a sprite with no zones is one quad even when the layer asks to fit text.
    #[test]
    fn with_no_zones_text_fit_is_still_one_quad() {
        let mut position = nine_part();
        position.stretch_x.clear();
        position.stretch_y.clear();
        assert_eq!(fitted(&position, 60.0).len(), 1);
    }
}

/// An inverted content box comes out as zero rather than as an abort.
///
/// Nothing in the sprite parser requires `content` to read left-to-right: the four numbers are
/// taken as they are spelled. A window measured with `clamp` would panic on one spelled the
/// other way round, which turns a malformed style into a crash rather than a wrong picture.
#[test]
fn an_inverted_content_box_does_not_abort() {
    use tessella_glyph::sprite::Content;

    let mut position = plain(1.0);
    position.stretch_x = vec![Stretch {
        from: 8.0,
        to: 16.0,
    }];
    position.content = Some(Content {
        left: 20.0,
        top: 20.0,
        right: 4.0,
        bottom: 4.0,
    });

    let icon = shape_icon((24.0, 24.0), [0.0, 0.0], Anchor::Center);
    let quads = icon_quads(icon, &position, 0.0, true);
    assert!(!quads.is_empty(), "the grid was not built");
}
