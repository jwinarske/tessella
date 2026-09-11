//! Laying a label out, against mbgl's own bounding boxes.
//!
//! mbgl's `Shaping.ZWSP` asserts `top`, `bottom`, `left` and `right` for four inputs. That box
//! is what placement collides against and what the quads are built from, so it being right by
//! coincidence is not good enough — the four cases between them pin the line count, the line
//! height, the widest line and the anchor's effect on all of it.

use tessella_glyph::shaping::{Anchor, Char, Justify, Options, Y_OFFSET, shape};
use tessella_glyph::text::ONE_EM;

/// The advance mbgl's ZWSP test gives U+4E2D, its only glyph.
const CJK_ADVANCE: f32 = 21.0;

fn cjk(text: &str) -> Vec<Char> {
    text.chars()
        .map(|character| {
            if character == '\u{4e2d}' {
                Char::new(character as u32, CJK_ADVANCE)
            } else {
                Char::blank(character as u32, 0.0)
            }
        })
        .collect()
}

fn latin(text: &str, advance: f32) -> Vec<Char> {
    text.chars()
        .map(|character| {
            if character == ' ' {
                Char::blank(character as u32, advance)
            } else {
                Char::new(character as u32, advance)
            }
        })
        .collect()
}

/// mbgl's options for the ZWSP test: centred, centre-justified, one em line height.
fn centred(max_width_in_chars: f32) -> Options {
    Options {
        max_width: max_width_in_chars * ONE_EM,
        line_height: ONE_EM,
        anchor: Anchor::Center,
        justify: Justify::Center,
        spacing: 0.0,
        ..Options::default()
    }
}

/// mbgl `Shaping.ZWSP`, all four boxes.
#[test]
fn the_bounding_box_matches_mbgl() {
    // Three lines, the widest being six characters.
    let shaping = shape(
        &cjk("中中\u{200b}中中\u{200b}中中\u{200b}中中中中中中\u{200b}中中"),
        &centred(5.0),
    );
    assert_eq!(shaping.lines.len(), 3);
    assert_eq!((shaping.top, shaping.bottom), (-36.0, 36.0));
    assert_eq!((shaping.left, shaping.right), (-63.0, 63.0));

    // Two lines, the widest being two characters.
    let shaping = shape(&cjk("中中\u{200b}中"), &centred(1.0));
    assert_eq!(shaping.lines.len(), 2);
    assert_eq!((shaping.top, shaping.bottom), (-24.0, 24.0));
    assert_eq!((shaping.left, shaping.right), (-21.0, 21.0));

    // One line: the trailing break opportunity is not a break.
    let shaping = shape(&cjk("中中\u{200b}"), &centred(2.0));
    assert_eq!(shaping.lines.len(), 1);
    assert_eq!((shaping.top, shaping.bottom), (-12.0, 12.0));
    assert_eq!((shaping.left, shaping.right), (-21.0, 21.0));

    // Five lines of nothing: they take height and no width.
    let shaping = shape(
        &cjk("\u{200b}\u{200b}\u{200b}\u{200b}\u{200b}"),
        &centred(1.0),
    );
    assert_eq!(shaping.lines.len(), 5);
    assert_eq!((shaping.top, shaping.bottom), (-60.0, 60.0));
    assert_eq!((shaping.left, shaping.right), (0.0, 0.0));
    assert!(shaping.is_empty(), "no glyph was drawable");
}

/// A zero-width space takes no place in the output.
///
/// It is a break opportunity, not a character. Emitting it would put a glyph with no bitmap
/// into the quad builder, which then asks the atlas for a rectangle that does not exist.
#[test]
fn zero_width_spaces_are_not_placed() {
    let shaping = shape(&cjk("中中\u{200b}中"), &centred(1.0));
    let placed: usize = shaping.lines.iter().map(|line| line.glyphs.len()).sum();
    assert_eq!(placed, 3, "three ideographs and no space");
    for line in &shaping.lines {
        for glyph in &line.glyphs {
            assert_ne!(glyph.codepoint, 0x200b);
        }
    }
}

/// The anchor moves the box without changing its size.
///
/// Every anchor describes the same label; what differs is which part of it sits on the point.
/// A shaper that changed the extent per anchor would make placement's collision box depend on
/// where the label happened to be anchored.
#[test]
fn the_anchor_moves_the_box_but_not_its_size() {
    let text = cjk("中中");
    let size = |anchor: Anchor| {
        let shaping = shape(
            &text,
            &Options {
                anchor,
                ..centred(2.0)
            },
        );
        (
            shaping.right - shaping.left,
            shaping.bottom - shaping.top,
            shaping.left,
            shaping.top,
        )
    };

    let (width, height, _, _) = size(Anchor::Center);
    assert_eq!((width, height), (42.0, 24.0));

    // Left-anchored: the box starts at the point.
    let (w, h, left, _) = size(Anchor::Left);
    assert_eq!((w, h), (width, height));
    assert_eq!(left, 0.0);

    // Right-anchored: the box ends at the point.
    let (w, h, left, _) = size(Anchor::Right);
    assert_eq!((w, h), (width, height));
    assert_eq!(left, -width);

    // Top-anchored: the box hangs below the point.
    let (w, h, _, top) = size(Anchor::Top);
    assert_eq!((w, h), (width, height));
    assert_eq!(top, 0.0);

    // Bottom-anchored: it sits above.
    let (w, h, _, top) = size(Anchor::Bottom);
    assert_eq!((w, h), (width, height));
    assert_eq!(top, -height);
}

/// An anchor on an edge justifies toward that edge unless the style says otherwise.
///
/// mbgl's `getAnchorJustification`. Centring a left-anchored label leaves it ragged on the side
/// that touches the point, which is the side a reader's eye follows back to the symbol.
#[test]
fn an_edge_anchor_justifies_toward_its_edge() {
    assert_eq!(Anchor::Left.justification(), Justify::Left);
    assert_eq!(Anchor::TopLeft.justification(), Justify::Left);
    assert_eq!(Anchor::BottomLeft.justification(), Justify::Left);
    assert_eq!(Anchor::Right.justification(), Justify::Right);
    assert_eq!(Anchor::TopRight.justification(), Justify::Right);
    assert_eq!(Anchor::Center.justification(), Justify::Center);
    assert_eq!(Anchor::Top.justification(), Justify::Center);
    assert_eq!(Anchor::Bottom.justification(), Justify::Center);
}

/// Justification decides where a short line sits against a long one.
///
/// Two lines of different length: left-justified they share a left edge, right-justified a
/// right edge, centred neither. This is the assertion that catches a justify factor applied
/// with the wrong sign, which is otherwise invisible on a single-line label.
#[test]
fn justification_places_a_short_line_against_a_long_one() {
    // "aaaa aa" at a width that breaks it into 4 and 2.
    let text = latin("aaaa aa", 12.0);
    let at = |justify: Justify| {
        let shaping = shape(
            &text,
            &Options {
                justify,
                anchor: Anchor::Center,
                max_width: 4.0 * 12.0,
                ..Options::default()
            },
        );
        assert_eq!(shaping.lines.len(), 2, "{shaping:?}");
        let first = shaping.lines[0].glyphs.first().expect("a glyph").x;
        let second = shaping.lines[1].glyphs.first().expect("a glyph").x;
        (first, second)
    };

    let (long, short) = at(Justify::Left);
    assert_eq!(long, short, "left-justified lines share a left edge");

    let (long, short) = at(Justify::Right);
    assert!(short > long, "right-justified, the short line starts later");

    let (long, short) = at(Justify::Center);
    assert!(
        short > long && short - long < 4.0 * 12.0,
        "centred sits between: {long} {short}"
    );
}

/// Trailing whitespace does not shift a centred line.
///
/// A line that ends at a space keeps that space in the break's output. Measuring it would
/// centre the line as though it were a character wider, putting every wrapped label slightly
/// left of where it belongs.
#[test]
fn a_trailing_space_does_not_shift_the_line() {
    let with_space = shape(
        &latin("aa aa", 12.0),
        &Options {
            max_width: 2.0 * 12.0,
            ..Options::default()
        },
    );
    let without = shape(
        &latin("aa", 12.0),
        &Options {
            max_width: 2.0 * 12.0,
            ..Options::default()
        },
    );

    assert_eq!(with_space.lines.len(), 2);
    assert_eq!(
        with_space.lines[0].glyphs.first().expect("a glyph").x,
        without.lines[0].glyphs.first().expect("a glyph").x,
        "the space at the break must not move the line"
    );
}

/// The baseline offset is what the ecosystem's glyphs were encoded against.
#[test]
fn the_baseline_offset_is_what_mbgl_uses() {
    assert_eq!(Y_OFFSET, -17.0);
}

/// Spacing sits *between* characters, so a line does not carry a trailing gap.
///
/// The pen takes the spacing after every glyph including the last, but that last gap is the
/// space before a character that never came. Counting it makes every line measure one gap too
/// wide, and a centred label is then shifted by half a gap.
#[test]
fn the_trailing_spacing_is_not_part_of_the_line() {
    let shaping = shape(
        &latin("aa", 12.0),
        &Options {
            spacing: 3.0,
            ..Options::default()
        },
    );

    // Two glyphs of 12 with one 3-unit gap between them.
    assert_eq!(shaping.right - shaping.left, 27.0);
}

/// A right-justified line ends where its last glyph's pen ends.
///
/// The indent is the line's drawn extent, which includes the final advance — a line justified
/// to where the last glyph *starts* hangs one character past its own right edge.
#[test]
fn right_justification_counts_the_final_advance() {
    let shaping = shape(
        &latin("aaa", 12.0),
        &Options {
            justify: Justify::Right,
            anchor: Anchor::Center,
            ..Options::default()
        },
    );

    // Three glyphs of 12: the line is 36 wide, centred on the anchor, so it runs -18..18.
    assert_eq!(shaping.left, -18.0);
    assert_eq!(shaping.lines[0].glyphs.first().expect("a glyph").x, -18.0);
    assert_eq!(shaping.lines[0].glyphs.last().expect("a glyph").x, 6.0);
}

/// Leading whitespace is dropped before the line is laid out.
///
/// A line that begins after a break begins with the space that caused it. Laying that out
/// indents the line by a character it does not draw, which on a centred label moves everything
/// by half of that.
#[test]
fn leading_whitespace_is_trimmed() {
    let indented = shape(&latin("  aa", 12.0), &Options::default());
    let plain = shape(&latin("aa", 12.0), &Options::default());

    assert_eq!(
        indented.right - indented.left,
        plain.right - plain.left,
        "the spaces must not widen the line"
    );
    assert_eq!(
        indented.lines[0].glyphs.first().expect("a glyph").x,
        plain.lines[0].glyphs.first().expect("a glyph").x,
        "nor move it"
    );
}

/// `text-radial-offset`: a distance, and the anchor says which way it points.
///
/// mbgl's `evaluateRadialOffset`, transcribed, and the numbers in it are chosen rather than
/// derived -- the seven-pixel baseline shift especially -- so every arm is pinned here. Getting
/// one wrong moves a class of labels by a few pixels, which is a diff nothing else names.
mod radial_offset {
    use tessella_glyph::shaping::{Anchor, radial_offset};

    /// The offset the tests are written against, and the leg of the square it implies.
    const OFFSET: f32 = 24.0;
    const LEG: f32 = 24.0 / core::f32::consts::SQRT_2;
    const BASELINE: f32 = 7.0;

    fn near(a: [f32; 2], b: [f32; 2]) -> bool {
        (a[0] - b[0]).abs() < 1e-4 && (a[1] - b[1]).abs() < 1e-4
    }

    /// A label anchored on one side sits the whole offset to the other side of the point, and
    /// takes no baseline shift: the two switches in mbgl are separate for exactly this pair.
    #[test]
    fn the_side_anchors_take_the_whole_offset_and_no_baseline() {
        assert!(near(radial_offset(Anchor::Left, OFFSET), [OFFSET, 0.0]));
        assert!(near(radial_offset(Anchor::Right, OFFSET), [-OFFSET, 0.0]));
    }

    /// Above and below take the whole offset too, and the baseline shift with it.
    #[test]
    fn the_vertical_anchors_take_the_baseline_shift() {
        assert!(near(
            radial_offset(Anchor::Top, OFFSET),
            [0.0, OFFSET - BASELINE]
        ));
        assert!(near(
            radial_offset(Anchor::Bottom, OFFSET),
            [0.0, -OFFSET + BASELINE]
        ));
    }

    /// A corner splits the offset into the legs of a right isosceles triangle, so the label sits
    /// at the offset's distance rather than at the offset on each axis.
    #[test]
    fn a_corner_splits_the_offset_into_two_legs() {
        assert!(near(
            radial_offset(Anchor::TopLeft, OFFSET),
            [LEG, LEG - BASELINE]
        ));
        assert!(near(
            radial_offset(Anchor::TopRight, OFFSET),
            [-LEG, LEG - BASELINE]
        ));
        assert!(near(
            radial_offset(Anchor::BottomLeft, OFFSET),
            [LEG, -LEG + BASELINE]
        ));
        assert!(near(
            radial_offset(Anchor::BottomRight, OFFSET),
            [-LEG, -LEG + BASELINE]
        ));
        // The hypotenuse is the offset, which is the whole point of the split.
        let [x, y] = radial_offset(Anchor::TopLeft, OFFSET);
        assert!((x.hypot(y + BASELINE) - OFFSET).abs() < 1e-4);
    }

    /// A centered label is not moved, whatever the offset.
    #[test]
    fn the_center_does_not_move() {
        assert!(near(radial_offset(Anchor::Center, OFFSET), [0.0, 0.0]));
    }

    /// A negative offset is clamped to zero, not reflected -- but the baseline shift survives it.
    ///
    /// mbgl clamps the *offset* and then reads it, so a `top` anchor asked for a negative offset
    /// still comes out at `-baselineOffset` rather than at nothing. It looks like an oversight and
    /// is not one to diverge from: it is what the oracle draws, and the seven pixels are visible.
    #[test]
    fn a_negative_offset_is_clamped_but_the_baseline_is_not() {
        assert!(near(radial_offset(Anchor::Left, -10.0), [0.0, 0.0]));
        assert!(near(radial_offset(Anchor::Right, -10.0), [0.0, 0.0]));
        assert!(near(radial_offset(Anchor::Top, -10.0), [0.0, -BASELINE]));
        assert!(near(radial_offset(Anchor::Bottom, -10.0), [0.0, BASELINE]));
        // And zero is the same picture, which is what says the clamp is the only thing happening.
        assert_eq!(
            radial_offset(Anchor::Top, -10.0),
            radial_offset(Anchor::Top, 0.0)
        );
    }
}

/// `text-offset` under a variable anchor: a vector, and the anchor chooses its signs.
///
/// The sibling of [`radial_offset`]'s tests. mbgl's `evaluateVariableOffset` takes whichever form
/// the style wrote -- a distance to point, or a vector to sign -- and the branch between them is
/// which property the style *named*, not what it evaluated to.
mod variable_offset {
    use tessella_glyph::shaping::{Anchor, radial_offset, variable_offset};

    const BASELINE: f32 = 7.0;

    fn near(a: [f32; 2], b: [f32; 2]) -> bool {
        (a[0] - b[0]).abs() < 1e-4 && (a[1] - b[1]).abs() < 1e-4
    }

    /// The radial form is `radial_offset`, unchanged, whatever is in the second component.
    #[test]
    fn the_radial_form_defers_to_radial_offset() {
        for anchor in [
            Anchor::Left,
            Anchor::TopRight,
            Anchor::Bottom,
            Anchor::Center,
        ] {
            assert!(near(
                variable_offset(anchor, [24.0, 99.0], true),
                radial_offset(anchor, 24.0)
            ));
        }
    }

    /// The vector form keeps both components and lets the anchor point them.
    #[test]
    fn the_vector_form_is_signed_by_the_anchor() {
        assert!(near(
            variable_offset(Anchor::Left, [10.0, 4.0], false),
            [10.0, 0.0]
        ));
        assert!(near(
            variable_offset(Anchor::Right, [10.0, 4.0], false),
            [-10.0, 0.0]
        ));
        // A vertical anchor takes the y and the baseline shift, and drops the x.
        assert!(near(
            variable_offset(Anchor::Top, [10.0, 4.0], false),
            [0.0, 4.0 - BASELINE]
        ));
        assert!(near(
            variable_offset(Anchor::Bottom, [10.0, 4.0], false),
            [0.0, -4.0 + BASELINE]
        ));
        // A corner takes both.
        assert!(near(
            variable_offset(Anchor::TopLeft, [10.0, 4.0], false),
            [10.0, 4.0 - BASELINE]
        ));
        assert!(near(
            variable_offset(Anchor::BottomRight, [10.0, 4.0], false),
            [-10.0, -4.0 + BASELINE]
        ));
    }

    /// A negative offset does not flip the label to the other side.
    ///
    /// mbgl takes the magnitudes first, so `[-1, 0]` on a `left` anchor puts the label to the
    /// *right* of the point, exactly as `[1, 0]` does. The anchor owns the direction and the
    /// style owns only the distance -- which is the same division the radial form makes, arrived
    /// at differently.
    #[test]
    fn the_sign_is_the_anchors_and_not_the_styles() {
        assert_eq!(
            variable_offset(Anchor::Left, [-10.0, -4.0], false),
            variable_offset(Anchor::Left, [10.0, 4.0], false)
        );
        assert_eq!(
            variable_offset(Anchor::TopRight, [-10.0, -4.0], false),
            variable_offset(Anchor::TopRight, [10.0, 4.0], false)
        );
    }

    /// A centered anchor is not moved by either form.
    #[test]
    fn the_center_does_not_move() {
        assert!(near(
            variable_offset(Anchor::Center, [10.0, 4.0], false),
            [0.0, 0.0]
        ));
        assert!(near(
            variable_offset(Anchor::Center, [24.0, 0.0], true),
            [0.0, 0.0]
        ));
    }
}
