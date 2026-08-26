//! An icon's box and the quad that draws it — mbgl's `shapeIcon` and `getIconQuad`.
//!
//! Two steps rather than one, and the difference between them is a pixel on every side. The box
//! is what collision measures; the quad is what draws, and it is padded because a ten-pixel icon
//! that is not aligned to the pixel grid covers eleven actual pixels.

use tessella_glyph::atlas::Rect;
use tessella_glyph::quads::{ICON_QUAD_BORDER, icon_quad, shape_icon};
use tessella_glyph::shaping::Anchor;

/// A 24-pixel icon somewhere in a sheet.
fn tex() -> Rect {
    Rect {
        x: 64,
        y: 32,
        width: 24,
        height: 24,
    }
}

/// A centred icon straddles its anchor.
#[test]
fn a_centred_icon_straddles_its_anchor() {
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
/// neighbouring icon.
#[test]
fn the_quad_is_the_box_grown_by_a_pixel() {
    let icon = shape_icon((24.0, 24.0), [0.0, 0.0], Anchor::Center);
    let quad = icon_quad(icon, tex(), 0.0);

    assert_eq!(quad.tl, (-13.0, -13.0));
    assert_eq!(quad.br, (13.0, 13.0));
    assert_eq!(
        quad.tr.0 - quad.tl.0,
        24.0 + 2.0 * ICON_QUAD_BORDER,
        "the pad is on one side only"
    );

    assert_eq!(quad.tex, tex(), "the rectangle grew with the quad");
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
    let quad = icon_quad(icon, tex(), 0.0);

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
    let quad = icon_quad(icon, tex(), quarter);

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
    let icon = shape_icon((24.0, 18.0), [3.0, -2.0], Anchor::TopLeft);
    let turned = icon_quad(icon, tex(), 0.0);
    assert_eq!(turned.tl, (icon.left - 1.0, icon.top - 1.0));
    assert_eq!(turned.br, (icon.right + 1.0, icon.bottom + 1.0));
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

    /// Fitting one axis centres the other rather than stretching it.
    ///
    /// mbgl's `else` branches, and the reason they are not "leave it alone": the icon has to move
    /// to sit on the text even where it does not resize, or a width-fitted shield stretches
    /// across the label while sitting above it.
    #[test]
    fn fitting_one_axis_centres_the_other() {
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
        // Centred on the text's vertical middle, which is zero here.
        assert_eq!(wide.top + wide.bottom, 0.0, "{wide:?} is not centred");
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
