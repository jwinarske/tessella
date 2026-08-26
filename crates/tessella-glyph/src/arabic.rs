//! Arabic contextual shaping: choosing each letter's form from its neighbours.
//!
//! mbgl's `applyArabicShaping`, which is ICU's `u_shapeArabic` under `U_SHAPE_LETTERS_SHAPE`.
//! Arabic is written joined, and which of a letter's four shapes is drawn depends on whether the
//! letters either side of it join to it — the same letter is four different pictures. Text is
//! *stored* as unjoined base letters, so a renderer that drew them as stored produces something a
//! reader can decipher and no reader would call written Arabic.
//!
//! It happens before the bidirectional reorder and before line breaking, which is mbgl's order:
//! the forms depend on logical neighbours, and reordering first would join each letter to whatever
//! ended up beside it on screen.
//!
//! # Why the table is generated
//!
//! Four forms for each of seventy-six letters, plus the joining types that select between them.
//! Both halves are in the Unicode Character Database and neither is in anyone's head, so
//! `tools/unicode-codegen/arabic_shaping.py` reads them out of it (DR-6). A hand-written table
//! would pass the three strings mbgl's tests state and be wrong about the rest of the alphabet.
//!
//! # What is not done
//!
//! ICU's other options: no tashkeel folding, no length adjustment beyond the lam-alef ligature,
//! no digit shaping. mbgl asks for none of them — its call is `LETTERS_SHAPE` and
//! `TEXT_DIRECTION_LOGICAL` and nothing else — so a build that did them would diverge from the
//! oracle rather than improve on it.

use crate::generated::arabic::{JOINING, Joining, LAM_ALEF, LETTERS};

/// Which of the four forms a letter is drawn in.
///
/// The order is the table's, and it is the order the code point ranges are laid out in: a form
/// is chosen by indexing rather than by matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Form {
    Isolated = 0,
    Final = 1,
    Initial = 2,
    Medial = 3,
}

/// The joining type of a code point, or `None` when it has none.
///
/// From the joining table rather than from the letters: the two are different sets. Every
/// combining mark is `Transparent` and *has no presentation forms* — `FE76 ARABIC FATHA ISOLATED
/// FORM` decomposes to a space and a fatha, two code points, so it is rightly absent from a forms
/// table. Reading the joining type off the letters instead leaves every mark non-joining, which
/// unjoins any voweled word while unvoweled text stays perfect.
fn joining_of(codepoint: u32) -> Option<Joining> {
    let index = JOINING.partition_point(|(start, _, _)| *start <= codepoint);
    let (start, end, joining) = *JOINING.get(index.checked_sub(1)?)?;
    (start..=end).contains(&codepoint).then_some(joining)
}

/// Whether a letter joins to the one *after* it, so that one takes a joined form.
fn joins_forward(codepoint: u32) -> bool {
    matches!(
        joining_of(codepoint),
        Some(Joining::Dual | Joining::Left | Joining::Causing)
    )
}

/// Whether a letter joins to the one *before* it.
fn joins_backward(codepoint: u32) -> bool {
    matches!(
        joining_of(codepoint),
        Some(Joining::Dual | Joining::Right | Joining::Causing)
    )
}

/// Whether a code point is invisible to joining.
///
/// A diacritic sits *between* two letters without breaking their join, so the context a letter
/// sees has to look past any number of them. Skipping this is the bug that unjoins a word the
/// moment it is voweled — which is most of a Qur'anic text and none of a road sign, so it
/// survives casual testing.
fn is_transparent(codepoint: u32) -> bool {
    matches!(joining_of(codepoint), Some(Joining::Transparent))
}

/// The nearest code point before `at` that is not transparent.
fn previous_visible(text: &[u32], at: usize) -> Option<u32> {
    text[..at]
        .iter()
        .rev()
        .copied()
        .find(|codepoint| !is_transparent(*codepoint))
}

/// The nearest code point after `at` that is not transparent.
fn next_visible(text: &[u32], at: usize) -> Option<u32> {
    text[at + 1..]
        .iter()
        .copied()
        .find(|codepoint| !is_transparent(*codepoint))
}

/// The form a letter takes, given whether its neighbours join to it.
const fn form_for(before: bool, after: bool) -> Form {
    match (before, after) {
        (true, true) => Form::Medial,
        (true, false) => Form::Final,
        (false, true) => Form::Initial,
        (false, false) => Form::Isolated,
    }
}

/// The lam-alef ligature for an alef, if it has one.
fn ligature_of(alef: u32) -> Option<[u32; 2]> {
    LAM_ALEF
        .iter()
        .find(|(base, _)| *base == alef)
        .map(|(_, forms)| *forms)
}

/// Rewrites a run of text into its contextual presentation forms.
///
/// The output is *shorter* than the input wherever a lam-alef ligature replaced two letters,
/// which is why this returns a vector rather than rewriting in place — mbgl's call leaves ICU's
/// length option at its default, which shrinks.
///
/// Codepoints the table does not know are passed through untouched: Latin, digits, punctuation
/// and every other script.
#[must_use]
pub fn shape(text: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(text.len());
    let mut index = 0usize;

    while index < text.len() {
        let codepoint = text[index];
        let Some(joining) = joining_of(codepoint) else {
            out.push(codepoint);
            index += 1;
            continue;
        };

        // A diacritic keeps its own code point and does not disturb the join around it.
        if joining == Joining::Transparent {
            out.push(codepoint);
            index += 1;
            continue;
        }

        let before = previous_visible(text, index).is_some_and(joins_forward);

        // Lam followed by alef is one ligature rather than two letters. It is checked before the
        // ordinary form, because the pair has forms of its own and choosing the lam's first
        // would draw a lam that the alef then replaced.
        if codepoint == LAM
            && let Some(alef) = next_visible(text, index)
            && let Some(forms) = ligature_of(alef)
        {
            {
                // The ligature joins backward like a lam and forward like an alef — which is to
                // say not at all — so only what precedes it chooses between its two forms.
                out.push(forms[usize::from(before)]);
                // Step over the alef, and over any diacritics between the two.
                index += 1;
                while index < text.len() && is_transparent(text[index]) {
                    out.push(text[index]);
                    index += 1;
                }
                index += 1;
                continue;
            }
        }

        let after = next_visible(text, index).is_some_and(joins_backward);
        let entry = &LETTERS[LETTERS
            .binary_search_by_key(&codepoint, |letter| letter.base)
            .expect("the joining type was found by the same key")];

        // A letter that joins on one side only cannot take the forms of the other. The table
        // repeats its isolated and final for those, so the index is safe rather than the caller
        // having to know.
        let form = match joining {
            Joining::Dual => form_for(before, after),
            Joining::Right => form_for(before, false),
            Joining::Left => form_for(false, after),
            _ => Form::Isolated,
        };
        out.push(entry.forms[form as usize]);
        index += 1;
    }

    out
}

/// Arabic letter lam, the first half of the only ligature this substitutes.
const LAM: u32 = 0x0644;

/// Whether a code point is one of the four lam-alef ligatures.
///
/// The caller needs it to walk an input and an output in step: a ligature stands for *two* input
/// characters, and nothing else in this rewrite does.
#[must_use]
pub fn is_lam_alef(codepoint: u32) -> bool {
    LAM_ALEF.iter().any(|(_, forms)| forms.contains(&codepoint))
}
