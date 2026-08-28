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
    let placed: usize = shaping.lines.iter().map(Vec::len).sum();
    assert_eq!(placed, 3, "three ideographs and no space");
    for line in &shaping.lines {
        for glyph in line {
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
        let first = shaping.lines[0].first().expect("a glyph").x;
        let second = shaping.lines[1].first().expect("a glyph").x;
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
        with_space.lines[0].first().expect("a glyph").x,
        without.lines[0].first().expect("a glyph").x,
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
    assert_eq!(shaping.lines[0].first().expect("a glyph").x, -18.0);
    assert_eq!(shaping.lines[0].last().expect("a glyph").x, 6.0);
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
        indented.lines[0].first().expect("a glyph").x,
        plain.lines[0].first().expect("a glyph").x,
        "nor move it"
    );
}
