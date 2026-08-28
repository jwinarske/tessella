//! Building glyph quads, against mbgl's own numbers.
//!
//! mbgl's `getGlyphQuads.DoubleResolutionGlyphPreservesLogicalQuadSize` states the 1x case
//! exactly: a 24x24 glyph with `left` 0, `top` -8 and a 32x32 atlas rectangle produces a quad
//! 32 units on each side. That fixes the buffer, the sign of `top` and the cancellation of the
//! half-advance all at once, which is why it is worth having even though the test it comes from
//! is about something else.

use tessella_glyph::atlas::Rect;
use tessella_glyph::pbf::Metrics;
use tessella_glyph::quads::{Options, Placed, Quad, RECT_BUFFER, glyph_quads};
use tessella_glyph::shaping::{Line, PositionedGlyph, Shaping};

/// mbgl's 1x glyph: 24 by 24, sitting eight above the baseline, advancing 24.
fn metrics_24() -> Metrics {
    Metrics {
        width: 24,
        height: 24,
        left: 0,
        top: -8,
        advance: 24,
    }
}

/// The atlas rectangle mbgl's test uses: the 30-pixel bitmap plus one pixel each side.
fn rect_32() -> Rect {
    Rect {
        x: 0,
        y: 0,
        width: 32,
        height: 32,
    }
}

/// A one-glyph label placed at the origin.
fn one_glyph(x: f32, y: f32) -> Shaping {
    Shaping {
        lines: vec![Line::from(vec![PositionedGlyph {
            codepoint: u32::from(b'A'),
            x,
            y,
            scale: 1.0,
            vertical: false,
            image: None,
        }])],
        ..Shaping::default()
    }
}

fn placed(_: u32) -> Option<Placed> {
    Some(Placed {
        rect: rect_32(),
        metrics: metrics_24(),
    })
}

/// mbgl's 1x expectations: a 32-unit quad, positioned where its arithmetic puts it.
#[test]
fn the_quad_matches_mbgl() {
    let quads = glyph_quads(&one_glyph(0.0, 0.0), placed, &Options::default());
    assert_eq!(quads.len(), 1);
    let quad = quads[0];

    // The size mbgl asserts.
    assert_eq!(quad.br.0 - quad.tl.0, 32.0);
    assert_eq!(quad.br.1 - quad.tl.1, 32.0);

    // And where it lands. `left` is 0 and the buffer is 4, so the quad starts four left of the
    // pen; `top` is -8, so negating it and subtracting the buffer puts the top at 4.
    assert_eq!(quad.tl, (-4.0, 4.0));
    assert_eq!(quad.br, (28.0, 36.0));
    assert_eq!(quad.tr, (28.0, 4.0));
    assert_eq!(quad.bl, (-4.0, 36.0));
}

/// The quad reaches four pixels outside the ink on every side.
///
/// Three from the encoder's border and one the atlas keeps. Sizing the quad to the ink clips
/// the distance field's falloff, which is where the antialiasing lives — thin letters at small
/// sizes and visibly cut edges at large ones.
#[test]
fn the_quad_covers_the_border_as_well_as_the_ink() {
    assert_eq!(RECT_BUFFER, 4.0);

    let quads = glyph_quads(&one_glyph(0.0, 0.0), placed, &Options::default());
    let quad = quads[0];
    let metrics = metrics_24();

    // The ink runs from `left` to `left + width`; the quad from four before to four after.
    #[allow(clippy::cast_precision_loss)]
    let ink_left = metrics.left as f32;
    #[allow(clippy::cast_precision_loss)]
    let ink_right = ink_left + metrics.width as f32;
    assert_eq!(quad.tl.0, ink_left - RECT_BUFFER);
    assert_eq!(quad.br.0, ink_right + RECT_BUFFER);
}

/// The glyph's position moves the quad one for one.
#[test]
fn the_quad_follows_the_glyph() {
    let at_origin = glyph_quads(&one_glyph(0.0, 0.0), placed, &Options::default())[0];
    let moved = glyph_quads(&one_glyph(10.0, -5.0), placed, &Options::default())[0];

    assert_eq!(moved.tl.0 - at_origin.tl.0, 10.0);
    assert_eq!(moved.tl.1 - at_origin.tl.1, -5.0);
    assert_eq!(moved.br.0 - at_origin.br.0, 10.0);
}

/// `text-offset` shifts the whole quad.
#[test]
fn the_text_offset_shifts_the_quad() {
    let plain = glyph_quads(&one_glyph(0.0, 0.0), placed, &Options::default())[0];
    let offset = glyph_quads(
        &one_glyph(0.0, 0.0),
        placed,
        &Options {
            text_offset: [3.0, 7.0],
            ..Options::default()
        },
    )[0];

    assert_eq!(offset.tl.0 - plain.tl.0, 3.0);
    assert_eq!(offset.tl.1 - plain.tl.1, 7.0);
}

/// A glyph the atlas does not have is skipped, not drawn from nowhere.
///
/// A label whose glyphs have not all arrived draws the ones that have, which is what keeps a
/// map readable while a font loads. Drawing the rest from a rectangle that is not there samples
/// whatever is at the origin of the atlas.
#[test]
fn an_unplaced_glyph_is_skipped() {
    let shaping = Shaping {
        lines: vec![Line::from(vec![
            PositionedGlyph {
                codepoint: u32::from(b'A'),
                x: 0.0,
                y: 0.0,
                scale: 1.0,
                vertical: false,
                image: None,
            },
            PositionedGlyph {
                codepoint: u32::from(b'B'),
                x: 24.0,
                y: 0.0,
                scale: 1.0,
                vertical: false,
                image: None,
            },
        ])],
        ..Shaping::default()
    };

    let quads = glyph_quads(
        &shaping,
        |codepoint| {
            if codepoint == u32::from(b'A') {
                placed(codepoint)
            } else {
                None
            }
        },
        &Options::default(),
    );
    assert_eq!(quads.len(), 1, "only the glyph that is in the atlas");
}

/// A zero-area rectangle produces no quad.
#[test]
fn an_empty_rectangle_produces_no_quad() {
    let quads = glyph_quads(
        &one_glyph(0.0, 0.0),
        |_| {
            Some(Placed {
                rect: Rect::default(),
                metrics: metrics_24(),
            })
        },
        &Options::default(),
    );
    assert!(quads.is_empty());
}

/// Rotation turns the quad about the origin and keeps it a rectangle.
///
/// A quarter turn is exact in floating point, so the corners can be asserted rather than
/// approximated — and it catches a sine and cosine transposed, which a small angle would not.
#[test]
fn rotation_turns_the_quad_about_the_origin() {
    let plain = glyph_quads(&one_glyph(0.0, 0.0), placed, &Options::default())[0];
    let turned = glyph_quads(
        &one_glyph(0.0, 0.0),
        placed,
        &Options {
            text_rotate: core::f32::consts::FRAC_PI_2,
            ..Options::default()
        },
    )[0];

    // A quarter turn sends (x, y) to (-y, x).
    let quarter = |point: (f32, f32)| (-point.1, point.0);
    let close = |one: (f32, f32), other: (f32, f32)| {
        (one.0 - other.0).abs() < 1e-4 && (one.1 - other.1).abs() < 1e-4
    };
    assert!(close(turned.tl, quarter(plain.tl)), "{turned:?}");
    assert!(close(turned.br, quarter(plain.br)), "{turned:?}");

    // Still a rectangle: opposite corners share their diagonal midpoint.
    let mid =
        |one: (f32, f32), other: (f32, f32)| ((one.0 + other.0) / 2.0, (one.1 + other.1) / 2.0);
    assert!(close(mid(turned.tl, turned.br), mid(turned.tr, turned.bl)));
}

/// Along a line, the position moves out of the corners and into the offset.
///
/// The shader projects a line-following label before placing each glyph, so the along-line
/// distance has to reach it separately. A builder that left it in the corners would lay the
/// label out flat and then bend it, putting every glyph but the first in the wrong place.
#[test]
fn along_a_line_the_position_moves_to_the_offset() {
    let quads = glyph_quads(
        &one_glyph(10.0, 2.0),
        placed,
        &Options {
            along_line: true,
            ..Options::default()
        },
    );
    let quad = quads[0];

    // Half the advance is 12, so the offset is the pen position plus that.
    assert_eq!(quad.glyph_offset, (22.0, 2.0));
    // And the corners are as though the glyph sat at the origin.
    let at_origin: Quad = glyph_quads(&one_glyph(0.0, 0.0), placed, &Options::default())[0];
    assert_eq!(quad.tl.0, at_origin.tl.0 - 12.0);
}

/// A point label carries no separate offset.
#[test]
fn a_point_label_has_no_glyph_offset() {
    let quads = glyph_quads(&one_glyph(10.0, 2.0), placed, &Options::default());
    assert_eq!(quads[0].glyph_offset, (0.0, 0.0));
}
