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
