//! Whether a string is in a script this renderer can shape.
//!
//! mbgl's `util::i18n::isStringInSupportedScript`, which `["is-supported-script", s]` is the whole
//! of. It is a rough heuristic and mbgl says so: whether a script "can be rendered" really depends
//! on the font, and what counts as a semantically significant difference from the ideal is a
//! judgment. What the ranges below capture is the set of scripts that need complex shaping --
//! reordering, ligature substitution, contextual forms -- which a renderer laying glyphs out from
//! an SDF atlas cannot do.
//!
//! Styles use it to pick which name to draw: Protomaps' basemap asks it of every label, and where
//! the answer is no it falls back to a transliterated field the fonts do cover. A build without
//! it does not draw those labels badly, it fails to parse the `text-field` and draws nothing at
//! all.
//!
//! # Why the ranges are written here rather than generated
//!
//! [`crate::generated`] mirrors mbgl's *tables*. These are not one: mbgl writes them as literals
//! inside `charInSupportedScript`, two as bare comparisons and the third through the block macro,
//! so there is nothing for `mbgl-codegen` to read. They are pinned by the tests instead, at every
//! boundary.
//!
//! Source revision: b5a2922844c9.

/// Blocks whose scripts need shaping this build does not do, in codepoint order.
///
/// mbgl's comment names the criterion: the common scripts with "Web Rank <= 32" and "Shaping
/// Required = YES" in Unicode's `scriptMetadata.txt`.
const NEEDS_SHAPING: [(u32, u32); 3] = [
    // Devanagari through Sinhala -- the main blocks for the Indic scripts.
    (0x0900, 0x0DFF),
    // Tibetan through Myanmar.
    (0x0F00, 0x109F),
    // Khmer.
    (0x1780, 0x17FF),
];

/// Whether one character is in a script this build can lay out.
#[must_use]
pub fn char_is_supported(codepoint: u32) -> bool {
    !NEEDS_SHAPING
        .iter()
        .any(|&(first, last)| codepoint >= first && codepoint <= last)
}

/// Whether every character of `text` is one [`char_is_supported`] admits.
///
/// Empty is supported, which is mbgl's answer too: the loop it runs has nothing to reject.
///
/// mbgl walks UTF-16 code units and this walks code points, and the two agree because every range
/// above sits below the basic plane. A surrogate is in none of them, so a pair mbgl checks twice
/// and admits is one character here, admitted once.
#[must_use]
pub fn is_supported(text: &str) -> bool {
    text.chars().all(|c| char_is_supported(c as u32))
}
