//! Where to break a label into lines.
//!
//! A transcription of mbgl's `determineLineBreaks`. Given a label's text, the advance of each
//! of its glyphs and a maximum width, this decides where the lines end.
//!
//! # It is a shortest-path problem, not a greedy fill
//!
//! The obvious algorithm puts as much on each line as fits and moves on. That produces the
//! classic ragged last line — "New York City Department of" / "Parks" — because a greedy fill
//! cannot know that taking one word less on line one would have balanced the whole label. Map
//! labels are short and sit under a symbol, so a lopsided one is conspicuous in a way a
//! paragraph of prose is not.
//!
//! So every break opportunity is a node, the cost of a line is how far its width is from the
//! average, and the answer is the cheapest path through them. mbgl calls the cost *badness*.
//!
//! # The target width is an average, and that is what makes it balance
//!
//! Not the maximum width: the target is the total width divided by the number of lines it will
//! take at that maximum. Aiming at the maximum would fill every line to the brim and leave the
//! last one short, which is the greedy result by another route.
//!
//! # Penalties are what encode typographic taste
//!
//! Breaking after an opening parenthesis is legal and ugly, so it costs 50. A newline is a
//! break the author asked for, so it is worth -10000 — negative, which the badness function
//! then *subtracts the square of*, making such a break essentially free wherever it appears.
//! And a break between two ideographs costs 150 when the server has already suggested breaks
//! with zero-width spaces, since those suggestions are better than anything guessed here.

use std::collections::BTreeSet;

use crate::text::{self, ZWSP};

/// One character of a label, as line breaking needs it.
///
/// Advance rather than a glyph, because breaking needs only the width: the glyph itself is the
/// shaper's business, and a label whose glyphs have not arrived can still be measured against
/// the ones that have.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Char {
    /// The codepoint.
    pub codepoint: u32,
    /// How far the pen moves for it, already scaled and spaced.
    pub advance: f32,
}

/// A break candidate and the cheapest path that reaches it.
#[derive(Debug, Clone, Copy)]
struct Candidate {
    /// Index into the text at which the new line starts.
    index: usize,
    /// Width of the text up to this point.
    x: f32,
    /// The candidate before this one on the cheapest path, by position in the list.
    prior: Option<usize>,
    /// Cost of the cheapest path ending here.
    badness: f32,
}

/// How bad a line of `width` is, against a target.
///
/// Squared distance from the target, so two lines slightly off beat one line badly off. The
/// last line is special: shorter than average is normal typography and costs half, longer than
/// average is the lopsided case and costs double.
fn badness(width: f32, target: f32, penalty: f32, is_last: bool) -> f32 {
    let raggedness = (width - target) * (width - target);
    if is_last {
        return if width < target {
            raggedness / 2.0
        } else {
            raggedness * 2.0
        };
    }
    // A negative penalty is an *encouragement*, and squaring then subtracting is what makes an
    // author's newline overwhelm any raggedness it causes.
    if penalty < 0.0 {
        return raggedness - penalty * penalty;
    }
    raggedness + penalty * penalty
}

/// What breaking between these two codepoints costs, before raggedness.
fn penalty(codepoint: u32, next: u32, penalizable_ideographic: bool) -> f32 {
    let mut penalty = 0.0;
    // A newline is a break the author asked for; nothing should outweigh it.
    if codepoint == 0x0a {
        penalty -= 10000.0;
    }
    // An opening parenthesis at the end of a line, or a closing one at the start of the next.
    if codepoint == 0x28 || codepoint == 0xff08 {
        penalty += 50.0;
    }
    if next == 0x29 || next == 0xff09 {
        penalty += 50.0;
    }
    // A break between ideographs is legal but worse than one at a space the server suggested.
    if penalizable_ideographic {
        penalty += 150.0;
    }
    penalty
}

/// The width a line should aim for: the total, divided by how many lines it will take.
fn average_line_width(text: &[Char], max_width: f32) -> f32 {
    let total: f32 = text.iter().map(|character| character.advance).sum();
    let lines = (total / max_width).ceil().max(1.0);
    total / lines
}

/// Finds the cheapest candidate to precede a break at `x`, and its total cost.
fn evaluate(
    index: usize,
    x: f32,
    target: f32,
    candidates: &[Candidate],
    penalty: f32,
    is_last: bool,
) -> Candidate {
    // The baseline is starting a new line here with nothing before it: one line, zero to `x`.
    let mut best_prior = None;
    let mut best = badness(x, target, penalty, is_last);

    for (position, candidate) in candidates.iter().enumerate() {
        let line_width = x - candidate.x;
        let cost = badness(line_width, target, penalty, is_last) + candidate.badness;
        // `<=` and not `<`, as mbgl has it: among equally cheap paths this takes the *latest*
        // candidate, which is the one that puts more on the earlier lines.
        if cost <= best {
            best_prior = Some(position);
            best = cost;
        }
    }

    Candidate {
        index,
        x,
        prior: best_prior,
        badness: best,
    }
}

/// The indices at which lines start, for text that must fit `max_width`.
///
/// Indices are into `text`, and the set never contains 0 — the first line starts there by
/// definition. A `max_width` of zero means no wrapping at all, which is how a style asks for a
/// single-line label.
///
/// # Why this works in logical order
///
/// mbgl's note, and it matters: breaking is decided before any bidirectional reordering,
/// because the visual order depends on where the lines break. Deciding in visual order would
/// need the answer to compute the question.
#[must_use]
pub fn line_breaks(text: &[Char], max_width: f32) -> BTreeSet<usize> {
    if max_width <= 0.0 || text.is_empty() {
        return BTreeSet::new();
    }

    let target = average_line_width(text, max_width);
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut x = 0.0f32;

    // A server that has inserted zero-width spaces has told us where it wants breaks; breaking
    // between ideographs instead is then second-best rather than merely allowed.
    let suggested = text.iter().any(|character| character.codepoint == ZWSP);

    for (index, character) in text.iter().enumerate() {
        // Whitespace at a break is dropped, so it does not count toward the line's width.
        if !text::is_whitespace(character.codepoint) {
            x += character.advance;
        }

        if index + 1 >= text.len() {
            continue;
        }
        let ideographic = text::allows_ideographic_breaking(character.codepoint);
        if !ideographic && !text::allows_word_breaking(character.codepoint) {
            continue;
        }
        let next = text[index + 1].codepoint;
        let candidate = evaluate(
            index + 1,
            x,
            target,
            &candidates,
            penalty(character.codepoint, next, ideographic && suggested),
            false,
        );
        candidates.push(candidate);
    }

    // The end of the text is the last break, and walking back from it gives the whole path.
    let last = evaluate(text.len(), x, target, &candidates, 0.0, true);
    let mut breaks = BTreeSet::new();
    let mut step = last.prior;
    while let Some(position) = step {
        breaks.insert(candidates[position].index);
        step = candidates[position].prior;
    }
    breaks
}

/// Splits text at the breaks [`line_breaks`] chose.
///
/// A convenience over the index set, and the form a shaper wants.
#[must_use]
pub fn split_lines(text: &[Char], max_width: f32) -> Vec<Vec<Char>> {
    let breaks = line_breaks(text, max_width);
    let mut lines = Vec::new();
    let mut start = 0;
    for boundary in breaks.iter().copied().chain(core::iter::once(text.len())) {
        if boundary > start {
            lines.push(text[start..boundary].to_vec());
        }
        start = boundary;
    }
    if lines.is_empty() && !text.is_empty() {
        lines.push(text.to_vec());
    }
    lines
}
