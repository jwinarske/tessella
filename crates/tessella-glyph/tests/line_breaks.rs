//! Line breaking, against mbgl's own expectations.
//!
//! mbgl's `Shaping.ZWSP` fixes the line *count* for four specific inputs at four specific
//! widths. Those are the cases worth having: they are chosen so that a greedy fill gets a
//! different answer than the badness-minimising path, which is the whole reason the algorithm is
//! not a greedy fill.

use tessella_glyph::shaping::{Char, line_breaks, split_lines};
use tessella_glyph::text::ONE_EM;

/// The advance mbgl's ZWSP test gives its one glyph, U+4E2D.
const CJK_ADVANCE: f32 = 21.0;

/// Builds the character list mbgl's test shapes.
///
/// Only U+4E2D is in its glyph map, so every other codepoint — the zero-width spaces — has an
/// advance of zero, which is what `getGlyphAdvance` returns for a glyph it does not hold.
fn cjk(text: &str) -> Vec<Char> {
    text.chars()
        .map(|character| Char {
            codepoint: character as u32,
            advance: if character == '\u{4e2d}' {
                CJK_ADVANCE
            } else {
                0.0
            },
        })
        .collect()
}

/// Latin text with one advance per character, for the cases mbgl does not cover.
fn latin(text: &str, advance: f32) -> Vec<Char> {
    text.chars()
        .map(|character| Char {
            codepoint: character as u32,
            advance,
        })
        .collect()
}

/// mbgl `Shaping.ZWSP`, all four cases, by line count.
#[test]
fn zero_width_spaces_break_where_mbgl_breaks() {
    // 中中 中中 中中 中中中中中中 中中, at five ems: three lines.
    let text = cjk("中中\u{200b}中中\u{200b}中中\u{200b}中中中中中中\u{200b}中中");
    assert_eq!(split_lines(&text, 5.0 * ONE_EM).len(), 3);

    // 中中 中, at one em: two lines.
    let text = cjk("中中\u{200b}中");
    assert_eq!(split_lines(&text, ONE_EM).len(), 2);

    // 中中, at two ems: one line — the trailing break opportunity is not a break.
    let text = cjk("中中\u{200b}");
    assert_eq!(split_lines(&text, 2.0 * ONE_EM).len(), 1);

    // Five zero-width spaces and nothing else, at one em: five lines.
    let text = cjk("\u{200b}\u{200b}\u{200b}\u{200b}\u{200b}");
    assert_eq!(split_lines(&text, ONE_EM).len(), 5);
}

/// A width of zero means no wrapping, which is how a style asks for one line.
#[test]
fn no_maximum_width_means_one_line() {
    let text = latin("a long label that would otherwise wrap", 10.0);
    assert!(line_breaks(&text, 0.0).is_empty());
    assert_eq!(split_lines(&text, 0.0).len(), 1);
}

/// Empty text has no breaks and no lines.
#[test]
fn empty_text_has_no_lines() {
    assert!(line_breaks(&[], 100.0).is_empty());
    assert!(split_lines(&[], 100.0).is_empty());
}

/// Text with no break opportunities stays on one line however wide it is.
///
/// mbgl's comment says so in as many words: lines longer than the maximum are allowed when
/// there is nowhere to break. A breaker that forced a break mid-word would hyphenate a name.
#[test]
fn text_with_nowhere_to_break_overflows_rather_than_splitting() {
    let text = latin("Llanfairpwllgwyngyll", 10.0);
    assert!(line_breaks(&text, ONE_EM).is_empty());
    assert_eq!(split_lines(&text, ONE_EM).len(), 1);
}

/// A newline is a break the author asked for, and it happens wherever it appears.
///
/// Its penalty is -10000, which the badness function squares and *subtracts*: no amount of
/// raggedness outweighs it. A breaker that treated it as merely one more opportunity would put
/// a two-line name on one line whenever that balanced better.
#[test]
fn an_explicit_newline_always_breaks() {
    // Short enough that balance would prefer a single line by a wide margin.
    let text = latin("a\nb", 10.0);
    let lines = split_lines(&text, 100.0 * ONE_EM);
    assert_eq!(lines.len(), 2, "the newline must win");
}

/// The result balances rather than filling greedily.
///
/// The case the algorithm exists for. Four equal words at a width that fits three: a greedy
/// fill takes three then one, and the badness path takes two and two, because the target is the
/// *average* line width rather than the maximum.
#[test]
fn lines_balance_instead_of_filling() {
    // Four words of one unit each plus separating spaces, at a width fitting three words.
    let text = latin("aa aa aa aa", 12.0);
    let lines = split_lines(&text, 3.0 * ONE_EM);

    assert_eq!(
        lines.len(),
        2,
        "{:?}",
        lines.iter().map(Vec::len).collect::<Vec<_>>()
    );

    // Counted without spaces: the break keeps the trailing space on the line it ended, so the
    // raw lengths differ by one even when the drawn widths do not. It is the drawn width that
    // is balanced, and the shaper drops the trailing space when it positions the line.
    let drawn: Vec<usize> = lines
        .iter()
        .map(|line| {
            line.iter()
                .filter(|character| !tessella_glyph::text::is_whitespace(character.codepoint))
                .count()
        })
        .collect();
    assert_eq!(
        drawn[0], drawn[1],
        "a greedy fill would have taken three words on the first line: {drawn:?}"
    );
}

/// Breaking after an opening parenthesis is legal and discouraged.
///
/// Both parentheses are break opportunities — that is mbgl's `allowsWordBreaking` — so what
/// keeps "(" off the end of a line is the 50-point penalty rather than a refusal. Here the two
/// options are close enough in raggedness that only the penalty separates them: without it the
/// break moves from after the space to after the parenthesis, and the first line reads "ab (".
#[test]
fn an_opening_parenthesis_is_penalised_at_the_end_of_a_line() {
    let text = latin("ab (cd ef", 12.0);
    let breaks = line_breaks(&text, 2.0 * ONE_EM);

    assert!(
        breaks.contains(&3),
        "the break belongs after the space: {breaks:?}"
    );
    assert!(
        !breaks.contains(&4),
        "a line was left ending in an open parenthesis: {breaks:?}"
    );
}

/// A last line shorter than average is ordinary; longer than average is lopsided.
///
/// `calculateBadness` halves the cost of the first and doubles the second, which is what makes
/// a two-line label out of text that would otherwise be left on one long line. Without the
/// asymmetry this label does not break at all.
#[test]
fn a_short_last_line_is_preferred_to_a_long_one() {
    let text = latin("ab (cd ef", 12.0);
    let breaks = line_breaks(&text, 3.0 * ONE_EM);

    assert_eq!(
        breaks.iter().copied().collect::<Vec<_>>(),
        [7],
        "the label should break rather than run long: {breaks:?}"
    );
}

/// Whitespace at a break does not count toward the line width being measured.
///
/// A space at the end of a line is dropped when the line is drawn, so measuring it makes the
/// line look wider than it is and pulls the break earlier. Here that is the difference between
/// splitting after the second word and after the first.
#[test]
fn trailing_whitespace_is_not_measured() {
    let text = latin("aa aa aa", 12.0);
    let breaks = line_breaks(&text, 2.0 * ONE_EM);

    assert_eq!(
        breaks.iter().copied().collect::<Vec<_>>(),
        [6],
        "measuring the spaces would have broken after the first word: {breaks:?}"
    );
}
