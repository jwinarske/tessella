//! Where a line of text is allowed to break, and what a break there costs.
//!
//! Transcribed from mbgl's `util::i18n`. These are predicates over single codepoints, and they
//! decide where `shaping` may put a line break — which is why they are here rather than folded
//! into the shaper: a break in the wrong place is not a layout bug, it is a label that reads
//! wrongly in one language and correctly in every other, which is not something a test written
//! in English will catch.

use crate::generated::blocks::IDEOGRAPHIC_BLOCKS;

/// One em, in the internal units layout works in.
///
/// The style spec talks in ems and layout works in points; this is the conversion, and it is
/// 24 because that is what the whole ecosystem's glyph encodings assume.
pub const ONE_EM: f32 = 24.0;

/// Zero-width space: an explicit break opportunity, usually inserted by the tile server.
pub const ZWSP: u32 = 0x200b;

/// Whether a codepoint is whitespace, for the purpose of measuring a line.
///
/// mbgl's list exactly, which is narrower than Unicode's: a non-breaking space is deliberately
/// not here, since it is whitespace that must *not* be dropped at a break.
#[must_use]
pub const fn is_whitespace(codepoint: u32) -> bool {
    matches!(codepoint, 0x20 | 0x09 | 0x0a | 0x0b | 0x0c | 0x0d)
}

/// Whether a line may break *after* this codepoint without a space being present.
///
/// The punctuation that commonly appears without surrounding spaces, plus the newline and the
/// zero-width space. Note that both parentheses are here: a line may break after either, and
/// the penalties in `shaping` are what discourage the ugly cases rather than a flat refusal.
#[must_use]
pub const fn allows_word_breaking(codepoint: u32) -> bool {
    matches!(
        codepoint,
        0x0a       // newline
        | 0x20     // space
        | 0x26     // ampersand
        | 0x28     // open parenthesis
        | 0x29     // close parenthesis
        | 0x2b     // plus sign
        | 0x2d     // hyphen-minus
        | 0x2f     // solidus
        | 0xad     // soft hyphen
        | 0xb7     // middle dot
        | 0x200b   // zero-width space
        | 0x2010   // hyphen
        | 0x2013 // en dash
    )
}

/// Whether a line may break after this codepoint because the script permits it anywhere.
///
/// Chinese, Japanese and Korean text is written without spaces, so a break is allowed between
/// almost any two characters. The blocks are generated from mbgl's own table rather than from
/// Unicode's, because mbgl consults a subset and a table built from the standard would break
/// lines where mbgl does not.
#[must_use]
pub fn allows_ideographic_breaking(codepoint: u32) -> bool {
    // U+2027 interpunct, which mbgl allows for hyphenating Chinese words, and which is below
    // every block in the table.
    if codepoint == 0x2027 {
        return true;
    }
    // Every ideographic block starts at U+2E80, so this returns early for all Latin text —
    // which is most text, and this predicate runs per character per label per tile.
    if codepoint < 0x2E80 {
        return false;
    }
    IDEOGRAPHIC_BLOCKS
        .iter()
        .any(|(first, last)| codepoint >= *first && codepoint <= *last)
}

/// Whether letter spacing may be applied to a label at all.
///
/// mbgl's `allowsLetterSpacing`, which holds when every character passes
/// `charAllowsLetterSpacing` — and that is exactly the Arabic blocks, the same set
/// [`crate::vertical::is_complex_shaping`] names. The reason is the same reason: these letters
/// are drawn joined to their neighbours, so tracking them apart does not loosen the word, it
/// breaks the joins and leaves a row of disconnected forms.
#[must_use]
pub fn allows_letter_spacing(text: &[u32]) -> bool {
    !text
        .iter()
        .copied()
        .any(crate::vertical::is_complex_shaping)
}
