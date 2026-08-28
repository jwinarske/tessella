//! Comparing two strings the way a reader would sort them (UTS #10).
//!
//! # Why codepoint order is not an answer
//!
//! A style may compare strings with a *collator*, and what "less than" means for text is not the
//! order of its bytes. `a` sorts before `A`, and both sort before `ä`, where the codepoints run
//! 0x41 < 0x61 < 0xE4 — so codepoint order gets two of those three pairs backwards. That order
//! is a decision the Unicode Consortium published rather than a property of the characters, so
//! it comes from a table: [`crate::generated::collation`], generated from `allkeys.txt` under
//! DR-6 like the shader tables.
//!
//! An approximation was tried first — casefold, decompose, compare codepoints — and passes five
//! of the suite's twelve cases. The seven it fails are the ones the table exists for. Shipping it
//! would have been a comparison that looks right and is wrong in the way a linear stand-in for
//! `cubic-bezier` is.
//!
//! # Three levels, and what the two switches do
//!
//! Every character has a primary weight (which letter), a secondary (which accent) and a tertiary
//! (which case). Comparison walks the levels in order and stops at the first that differs, which
//! is what makes `a` < `ä` < `b`: `a` and `ä` tie at the primary and part at the secondary, where
//! `b` parts from both at the primary.
//!
//! `case-sensitive: false` ignores the tertiary level, which is the straightforward half.
//!
//! `diacritic-sensitive: false` is not the same move one level up, and the suite is what says so.
//! `accent-lt-en` asks whether `a < ä` with accents insensitive and expects **false** — not
//! merely "not less at the secondary level", but equal. Ignoring the secondary does not give
//! that: `ä`'s weights are the letter's *followed by* an accent-only element, and that element
//! still carries a tertiary, so the two strings differ in length at the third level and `ä` comes
//! out greater.
//!
//! So the accent is removed rather than unranked: elements with no primary weight — which is what
//! an accent contributes — are dropped, and what remains is the letter, compared at full
//! strength. That is the same thing mbgl does by a different route. It has no level control, so
//! it strips accents from the *input* with nunicode's `unaccent` and collates normally; this
//! strips them from the weights, needing no second table to say which characters are accented.
//!
//! # What this does not do
//!
//! **Contractions.** Some sequences collate as one unit — Danish `aa` sorting as `å` is the usual
//! example — and finding them needs a longest-match scan over the input rather than a lookup per
//! character. There are 964 of them and they compare as their parts here. Recorded rather than
//! silently dropped, and the generated table says the number.
//!
//! **Normalization.** UTS #10 sorts a normalized string, and this sorts the one it is given. The
//! table carries precomposed characters with their full weight sequences, so `ä` written as one
//! codepoint compares correctly; the same letter written as `a` plus a combining diaeresis
//! compares correctly too, since the combining mark carries the same secondary. What differs is
//! the order of *several* marks on one base, which normalization would canonicalize and this
//! leaves as written.
//!
//! **The locale.** mbgl's own default collator says in a comment that it ignores the locale and
//! would need ICU to honour it, and its `resolvedLocale` returns the empty string. This does the
//! same, and the suite is why it matters rather than being a detail: `accent-equals-de` asks
//! whether the resolved locale is `de` and *branches on the answer*, comparing `ü` with `ue`
//! where a German tailoring exists and checking the input directly where none does. Reporting
//! back the locale that was asked for would take the first branch without having the tailoring
//! it promises, and answer `false` where the suite expects `true`.

use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;

use crate::generated::collation::{IMPLICIT, MULTI, RUNS, Weights};

/// How a comparison should treat case and accents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collator {
    /// Whether `a` and `A` differ.
    pub case_sensitive: bool,
    /// Whether `a` and `ä` differ.
    pub diacritic_sensitive: bool,
    /// The locale asked for, reported back by `resolved-locale`.
    pub locale: Option<String>,
}

impl Default for Collator {
    /// The spec's defaults: both switches off, which compares letters and ignores how they are
    /// written.
    fn default() -> Self {
        Self {
            case_sensitive: false,
            diacritic_sensitive: false,
            locale: None,
        }
    }
}

impl Collator {
    /// Orders two strings.
    ///
    /// Level by level, stopping at the first that separates them. Ignorable weights — a zero at
    /// some level, which is how a combining mark says it contributes nothing to the letter — are
    /// skipped rather than compared, so `ä` and `a` have the same primary *sequence* and not
    /// merely the same first primary.
    #[must_use]
    pub fn compare(&self, left: &str, right: &str) -> Ordering {
        let left = key(left, self.diacritic_sensitive);
        let right = key(right, self.diacritic_sensitive);

        // Primary and secondary always; the tertiary only where case counts. With accents
        // already removed from the key, comparing the secondary of what is left costs nothing
        // and keeps the levels in the order UTS #10 puts them.
        let levels: &[usize] = if self.case_sensitive {
            &[0, 1, 2]
        } else {
            &[0, 1]
        };
        for &level in levels {
            let ordering = compare_level(&left, &right, level);
            if ordering != Ordering::Equal {
                return ordering;
            }
        }
        Ordering::Equal
    }

    /// Whether two strings are the same under this collator.
    #[must_use]
    pub fn equals(&self, left: &str, right: &str) -> bool {
        self.compare(left, right) == Ordering::Equal
    }

    /// The locale this resolved to, which is none.
    ///
    /// Empty, as mbgl's default collator returns. Reporting back the locale that was asked for
    /// would be claiming a tailoring that was not applied — and a style can tell: the suite's
    /// `accent-equals-de` branches on this answer and takes a different comparison depending on
    /// it, so an implementation that overstates what it resolved answers the wrong question.
    #[must_use]
    pub fn resolved_locale(&self) -> &str {
        ""
    }
}

/// One string's collation elements, in order.
///
/// Without `accents`, the elements that carry only an accent are dropped rather than ranked
/// lower — see the module note on why that is not the same as ignoring the secondary level. An
/// accent-only element is one with no primary weight, which is how the table spells "this
/// contributes nothing to which letter it is".
fn key(text: &str, accents: bool) -> Vec<Weights> {
    let mut out = Vec::with_capacity(text.len());
    for character in text.chars() {
        weights_for(character as u32, &mut out);
    }
    if !accents {
        out.retain(|(primary, ..)| *primary != 0);
    }
    out
}

/// Compares one level, skipping the weights that are ignorable at it.
fn compare_level(left: &[Weights], right: &[Weights], level: usize) -> Ordering {
    let pick = |weights: &Weights| match level {
        0 => weights.0,
        1 => weights.1,
        _ => weights.2,
    };
    let mut left = left.iter().map(pick).filter(|weight| *weight != 0);
    let mut right = right.iter().map(pick).filter(|weight| *weight != 0);
    loop {
        match (left.next(), right.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(a), Some(b)) if a != b => return a.cmp(&b),
            (Some(_), Some(_)) => {}
        }
    }
}

/// A codepoint's weights, appended to `out`.
fn weights_for(codepoint: u32, out: &mut Vec<Weights>) {
    if let Some(weights) = from_runs(codepoint) {
        out.push(weights);
        return;
    }
    if let Ok(index) = MULTI.binary_search_by_key(&codepoint, |(cp, _)| *cp) {
        out.extend_from_slice(MULTI[index].1);
        return;
    }
    implicit(codepoint, out);
}

/// A single-element codepoint, found in the run table.
fn from_runs(codepoint: u32) -> Option<Weights> {
    let index = RUNS
        .binary_search_by(|(start, length, ..)| {
            if codepoint < *start {
                Ordering::Greater
            } else if codepoint >= start + u32::from(*length) {
                Ordering::Less
            } else {
                Ordering::Equal
            }
        })
        .ok()?;
    let (start, _, primary, secondary, tertiary) = RUNS[index];
    #[allow(clippy::cast_possible_truncation)]
    Some((primary + (codepoint - start) as u16, secondary, tertiary))
}

/// Weights for a codepoint no table lists.
///
/// UTS #10 §10.1.3: a script too large to enumerate is given an order by construction, as a
/// primary derived from a per-block base plus the codepoint's high bits, and a second element
/// carrying the low bits. Han is the case that matters for a map — the ideographs are not in
/// `allkeys.txt` at all, so without this every Chinese label would compare equal to every other.
fn implicit(codepoint: u32, out: &mut Vec<Weights>) {
    // The bases UTS #10 names. `allkeys.txt` carries the others in `IMPLICIT`.
    const CORE_HAN: u16 = 0xFB40;
    const OTHER_HAN: u16 = 0xFB80;
    const UNASSIGNED: u16 = 0xFBC0;

    let base = IMPLICIT
        .iter()
        .find(|(first, last, _)| codepoint >= *first && codepoint <= *last)
        .map_or_else(
            || {
                if is_core_han(codepoint) {
                    CORE_HAN
                } else if is_other_han(codepoint) {
                    OTHER_HAN
                } else {
                    UNASSIGNED
                }
            },
            |(_, _, base)| *base,
        );

    #[allow(clippy::cast_possible_truncation)]
    let high = base.wrapping_add((codepoint >> 15) as u16);
    #[allow(clippy::cast_possible_truncation)]
    let low = ((codepoint & 0x7FFF) as u16) | 0x8000;
    out.push((high, 0x0020, 0x0002));
    out.push((low, 0, 0));
}

/// The Unified Ideographs and the compatibility ideographs that UTS #10 calls core.
fn is_core_han(codepoint: u32) -> bool {
    (0x4E00..=0x9FFF).contains(&codepoint) || (0xF900..=0xFAFF).contains(&codepoint)
}

/// The extensions, which sort after the core block rather than among it.
fn is_other_han(codepoint: u32) -> bool {
    (0x3400..=0x4DBF).contains(&codepoint)
        || (0x20000..=0x2A6DF).contains(&codepoint)
        || (0x2A700..=0x2EBEF).contains(&codepoint)
        || (0x2F800..=0x2FA1F).contains(&codepoint)
        || (0x30000..=0x3134F).contains(&codepoint)
}
