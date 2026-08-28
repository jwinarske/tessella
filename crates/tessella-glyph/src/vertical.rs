//! Vertical writing: which characters stay upright, and which punctuation changes shape.
//!
//! A label in a vertical writing mode runs down the screen rather than across it, and what
//! happens to each character on the way down is not one rule. A CJK ideograph is drawn exactly
//! as it is horizontally, and the pen moves down by one em. A Latin letter is rotated a quarter
//! turn, so it keeps its horizontal metrics and the *line* is what turned. And a handful of
//! punctuation marks are neither: they are replaced outright by a different character that was
//! drawn for vertical text — a horizontal ellipsis becomes a vertical one.
//!
//! mbgl decides all three from [`crate::generated::vertical`], whose tables come from asking
//! mbgl itself rather than from reading it. What is here is the logic over them.

use crate::generated::vertical::{COMPLEX_SHAPING, NEUTRAL, PUNCTUATION, Range, UPRIGHT};

/// Whether a sorted, non-overlapping range table holds a code unit.
fn in_ranges(ranges: &[Range], codepoint: u32) -> bool {
    ranges
        .binary_search_by(|&(first, last)| {
            if codepoint < first {
                core::cmp::Ordering::Greater
            } else if codepoint > last {
                core::cmp::Ordering::Less
            } else {
                core::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// Whether a character is drawn as it is when the line runs downwards.
///
/// mbgl's `hasUprightVerticalOrientation`.
#[must_use]
pub fn is_upright(codepoint: u32) -> bool {
    in_ranges(&UPRIGHT, codepoint)
}

/// Whether a character is one mbgl neither rotates nor calls upright.
///
/// mbgl's `hasNeutralVerticalOrientation`. It exists only to make [`is_rotated`] the complement
/// of the two together, and is not consulted anywhere else.
#[must_use]
pub fn is_neutral(codepoint: u32) -> bool {
    in_ranges(&NEUTRAL, codepoint)
}

/// Whether a character turns a quarter turn when the line runs downwards.
///
/// mbgl's `hasRotatedVerticalOrientation`: everything that is neither upright nor neutral.
#[must_use]
pub fn is_rotated(codepoint: u32) -> bool {
    !(is_upright(codepoint) || is_neutral(codepoint))
}

/// Whether a character belongs to a script whose shaping depends on its neighbours.
///
/// mbgl's `isCharInComplexShapingScript`. Such a character is never verticalized when vertical
/// placement is allowed: it is drawn by joining to what surrounds it, and turning one of a run
/// on its side breaks the join.
#[must_use]
pub fn is_complex_shaping(codepoint: u32) -> bool {
    in_ranges(&COMPLEX_SHAPING, codepoint)
}

/// The vertical form of a punctuation mark, if it has one.
///
/// mbgl's single-character `verticalizePunctuation`.
#[must_use]
pub fn punctuation_form(codepoint: u32) -> Option<u32> {
    PUNCTUATION
        .binary_search_by_key(&codepoint, |&(from, _)| from)
        .ok()
        .map(|at| PUNCTUATION[at].1)
}

/// Replaces horizontal punctuation with its vertical form, in place and without reordering.
///
/// mbgl's string `verticalizePunctuation`, and the neighbour test is the whole of it: a mark is
/// only replaced when *neither* neighbour is a character that would be rotated — unless that
/// neighbour is itself a mark with a vertical form, which is how a run of them converts
/// together. A comma between two ideographs becomes a vertical comma; the same comma between two
/// Latin letters, which are rotated, stays as it is, because the line around it is lying on its
/// side and a vertical comma there would be the one thing pointing the wrong way.
///
/// The length never changes. mbgl says the same in a comment, because its `TaggedString` keeps
/// per-character section indices alongside and a substitution that grew would desynchronize them.
#[must_use]
pub fn verticalize_punctuation(text: &[u32]) -> Vec<u32> {
    // mbgl reads a missing neighbour as the code unit zero and tests `!nextCharCode` first, so
    // an actual NUL inside the label counts as no neighbour too. Degenerate input, but the two
    // cases are one branch there and are one here.
    let convertible = |at: Option<&u32>| match at {
        None | Some(0) => true,
        Some(&neighbour) => !is_rotated(neighbour) || punctuation_form(neighbour).is_some(),
    };
    text.iter()
        .enumerate()
        .map(|(at, &codepoint)| {
            let replaceable = convertible(text.get(at + 1))
                && convertible(at.checked_sub(1).and_then(|before| text.get(before)));
            if replaceable {
                punctuation_form(codepoint).unwrap_or(codepoint)
            } else {
                codepoint
            }
        })
        .collect()
}
