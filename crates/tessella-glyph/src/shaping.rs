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
    /// Whether there is a glyph to draw for it.
    ///
    /// False for a zero-width space and for any codepoint the font stack does not have. mbgl
    /// expresses this by failing the glyph lookup and skipping the character; here the lookup
    /// has already happened, so the outcome is carried. It is not the same as a zero advance —
    /// a space has no glyph *and* an advance, and both facts matter.
    pub drawable: bool,
    /// The `font-scale` of the section this character came from.
    ///
    /// One for an ordinary label. A `["format", …]` may set it per section, which changes the
    /// glyph's drawn size, its advance, and — through the largest scale on a line — that line's
    /// height and where every glyph on it sits against the baseline.
    ///
    /// The advance in [`Self::advance`] is already scaled, because the caller computes it from
    /// metrics this type does not carry. This is kept beside it for the two things the caller
    /// cannot do: the line's height, which depends on its neighbours, and the quad's size.
    pub scale: f32,
}

impl Char {
    /// A character with a glyph behind it.
    #[must_use]
    pub const fn new(codepoint: u32, advance: f32) -> Self {
        Self {
            codepoint,
            advance,
            drawable: true,
            scale: 1.0,
        }
    }

    /// A character with an advance but nothing to draw — a space, or a codepoint the font
    /// stack does not carry.
    #[must_use]
    pub const fn blank(codepoint: u32, advance: f32) -> Self {
        Self {
            codepoint,
            advance,
            drawable: false,
            scale: 1.0,
        }
    }

    /// The same character, marked as belonging to a section at `scale`.
    ///
    /// Only the mark: the advance is the caller's, and deliberately, because mbgl scales the
    /// glyph's *metric* and adds letter spacing to the result — `metrics.advance * scale +
    /// spacing`. Scaling the finished advance would scale the spacing too, and a double-size
    /// word would sit twice as loosely as the text beside it.
    #[must_use]
    pub const fn at_scale(self, scale: f32) -> Self {
        Self { scale, ..self }
    }
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

/// Where a label sits relative to its anchor point.
///
/// mbgl's `SymbolAnchorType`. The name says which part of the label touches the anchor, so
/// `Top` puts the label *below* the point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Anchor {
    /// Centred on the point.
    #[default]
    Center,
    /// The label's left edge is at the point.
    Left,
    /// Its right edge.
    Right,
    /// Its top edge.
    Top,
    /// Its bottom edge.
    Bottom,
    /// Its top-left corner.
    TopLeft,
    /// Its top-right corner.
    TopRight,
    /// Its bottom-left corner.
    BottomLeft,
    /// Its bottom-right corner.
    BottomRight,
}

impl Anchor {
    /// How far along the label's width and height the anchor sits, each in 0..1.
    #[must_use]
    pub const fn alignment(self) -> (f32, f32) {
        let horizontal = match self {
            Self::Right | Self::TopRight | Self::BottomRight => 1.0,
            Self::Left | Self::TopLeft | Self::BottomLeft => 0.0,
            _ => 0.5,
        };
        let vertical = match self {
            Self::Bottom | Self::BottomLeft | Self::BottomRight => 1.0,
            Self::Top | Self::TopLeft | Self::TopRight => 0.0,
            _ => 0.5,
        };
        (horizontal, vertical)
    }

    /// The justification a label takes when its style does not state one.
    ///
    /// mbgl's `getAnchorJustification`: text anchored on its left edge reads left-justified,
    /// and the alternative — centring a left-anchored label — leaves it ragged on the side that
    /// touches the point.
    #[must_use]
    pub const fn justification(self) -> Justify {
        match self {
            Self::Right | Self::TopRight | Self::BottomRight => Justify::Right,
            Self::Left | Self::TopLeft | Self::BottomLeft => Justify::Left,
            _ => Justify::Center,
        }
    }
}

/// How lines are aligned against each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Justify {
    /// Ragged right.
    Left,
    /// Ragged both.
    #[default]
    Center,
    /// Ragged left.
    Right,
}

impl Justify {
    /// The factor mbgl multiplies a line's length by: left 0, centre a half, right 1.
    const fn factor(self) -> f32 {
        match self {
            Self::Left => 0.0,
            Self::Center => 0.5,
            Self::Right => 1.0,
        }
    }
}

/// The baseline's offset from the top of a line box.
///
/// mbgl's `Shaping::yOffset`, and it is -17 for the same reason the border is 3: it is what the
/// ecosystem's glyphs were encoded against.
pub const Y_OFFSET: f32 = -17.0;

/// One glyph, placed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PositionedGlyph {
    /// Which glyph.
    pub codepoint: u32,
    /// Horizontal position, relative to the anchor.
    pub x: f32,
    /// Vertical position, relative to the anchor.
    pub y: f32,
    /// Its section's `font-scale`, which the quad builder needs and the shaper cannot apply.
    ///
    /// Shaping places a glyph; the quad is where its size is decided, and a scaled glyph is
    /// larger as well as further along. Carrying it here is how the two stay one decision.
    pub scale: f32,
}

/// A shaped label: its glyphs in lines, and the box they occupy.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Shaping {
    /// The lines, each a list of placed glyphs.
    pub lines: Vec<Vec<PositionedGlyph>>,
    /// Top of the bounding box, relative to the anchor.
    pub top: f32,
    /// Bottom of the bounding box.
    pub bottom: f32,
    /// Left of the bounding box.
    pub left: f32,
    /// Right of the bounding box.
    pub right: f32,
}

impl Shaping {
    /// Whether anything was placed.
    ///
    /// A label of nothing but zero-width spaces shapes into lines that hold no glyphs, which is
    /// not the same as no lines: the lines still take vertical space, and mbgl's own test
    /// asserts five of them.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.iter().all(Vec::is_empty)
    }
}

/// What a label is being shaped against.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Wrap at this width, in the same units as the advances. Zero means never.
    pub max_width: f32,
    /// Distance between baselines.
    pub line_height: f32,
    /// Where the label sits relative to its point.
    pub anchor: Anchor,
    /// How the lines align against each other.
    pub justify: Justify,
    /// Extra tracking between characters.
    pub spacing: f32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            max_width: 0.0,
            line_height: crate::text::ONE_EM,
            anchor: Anchor::Center,
            justify: Justify::Center,
            spacing: 0.0,
        }
    }
}

/// Drops leading and trailing whitespace from a line.
///
/// A line that ends at a space keeps that space, and a line that starts after one begins with
/// it. Neither is drawn, and leaving them in shifts the line by their width — which is exactly
/// the amount a centred label would be off by.
fn trim(line: &[Char]) -> &[Char] {
    let start = line
        .iter()
        .position(|character| !text::is_whitespace(character.codepoint));
    let Some(start) = start else {
        return &line[..0];
    };
    let end = line
        .iter()
        .rposition(|character| !text::is_whitespace(character.codepoint))
        .map_or(start, |index| index + 1);
    &line[start..end]
}

/// Shifts a line so its justification lands where it should.
///
/// mbgl's `justifyLine`. The indent is the line's *drawn* extent times the justify factor, and
/// the drawn extent includes the last glyph's advance — a right-justified line ends where its
/// last glyph's pen ends, not where that glyph starts.
fn justify_line(glyphs: &mut [PositionedGlyph], last_advance: f32, justify: f32) {
    if justify == 0.0 {
        return;
    }
    let Some(last) = glyphs.last() else {
        return;
    };
    let indent = (last.x + last_advance) * justify;
    for glyph in glyphs.iter_mut() {
        glyph.x -= indent;
    }
}

/// Rewrites a label's Arabic letters into their contextual forms.
///
/// [`crate::arabic::shape`] over the whole label, carrying the advances. A lam-alef ligature
/// replaces two letters with one, so the result can be shorter than the input — the advance kept
/// is the *lam's*, since the ligature is drawn where the lam was and the alef is folded into it.
///
/// Borrowed when the label has no Arabic in it, which is most of them.
#[must_use]
pub fn apply_arabic(text: &[Char]) -> std::borrow::Cow<'_, [Char]> {
    use std::borrow::Cow;

    let codepoints: Vec<u32> = text.iter().map(|character| character.codepoint).collect();
    let shaped = crate::arabic::shape(&codepoints);
    if shaped == codepoints {
        return Cow::Borrowed(text);
    }

    // Walk the two in step. A ligature consumed two inputs for one output, so the input index
    // advances by more than the output's — matching on the codepoint would be wrong, since a
    // presentation form does not equal the base it came from.
    let mut out = Vec::with_capacity(shaped.len());
    let mut input = 0usize;
    for codepoint in shaped {
        // The alef the ligature swallowed, if this output stands for two inputs.
        let consumed = usize::from(
            input + 1 < text.len()
                && codepoint != text[input].codepoint
                && crate::arabic::is_lam_alef(codepoint),
        );
        out.push(Char {
            codepoint,
            ..text[input]
        });
        input += 1 + consumed;
    }

    Cow::Owned(out)
}

/// Reorders one line's characters from logical order into display order.
///
/// The Unicode bidirectional algorithm, UAX #9, through `unicode-bidi`. Text arrives in the
/// order it is *stored* — which for Hebrew and Arabic is the order it is read, right to left —
/// and a shaper that positioned it as stored draws every such label backwards. That is not a
/// missing feature but a wrong answer: the letters are all correct and the word is not.
///
/// Runs, not characters. A line is cut into stretches of one direction and the stretches are
/// laid out in visual order, each right-to-left one reversed within itself; reversing the whole
/// line instead would put an embedded Latin word or a number backwards, which is the failure
/// that looks *nearly* right. mbgl reaches the same place through ICU's `ubidi_setLine` and
/// `ubidi_getVisualRun`.
///
/// Left alone when the line is entirely left-to-right, which is most of them: the algorithm's
/// answer is the identity there, and building a string to be told so is work for nothing.
///
/// Arabic *shaping* — the contextual letter forms — is a separate step mbgl runs before this one
/// and is not ported. Without it Arabic reorders correctly and each letter is drawn in its
/// isolated form rather than joined to its neighbours.
#[must_use]
pub fn reorder(line: &[Char]) -> std::borrow::Cow<'_, [Char]> {
    use std::borrow::Cow;

    let text: String = line
        .iter()
        .filter_map(|character| char::from_u32(character.codepoint))
        .collect();
    // A codepoint that is not a character cannot be reordered against, and the mapping below
    // depends on the two sequences having the same length.
    if text.chars().count() != line.len() {
        return Cow::Borrowed(line);
    }

    let info = unicode_bidi::ParagraphBidiInfo::new(&text, None);
    if !info.has_rtl() {
        return Cow::Borrowed(line);
    }

    // Byte offsets to positions in `line`, since the runs are byte ranges and the characters are
    // not one byte each — the scripts this exists for are two and three bytes in UTF-8.
    let mut at_byte = vec![0usize; text.len() + 1];
    for (index, (offset, _)) in text.char_indices().enumerate() {
        at_byte[offset] = index;
    }
    at_byte[text.len()] = line.len();

    let (levels, runs) = info.visual_runs(0..text.len());
    let mut out = Vec::with_capacity(line.len());
    for run in runs {
        let (from, to) = (at_byte[run.start], at_byte[run.end]);
        if levels[run.start].is_rtl() {
            out.extend(line[from..to].iter().rev().copied());
        } else {
            out.extend_from_slice(&line[from..to]);
        }
    }

    Cow::Owned(out)
}

/// Lays a label out: breaks it into lines, places its glyphs, and aligns the result.
///
/// A transcription of mbgl's `shapeLines` for horizontal text in one font stack. Vertical
/// writing, images in text and per-section scaling are not implemented; each of those changes
/// the line's height as well as its width.
///
/// # Why the capture cannot check them, which is not the reason first given
///
/// This used to say they had no oracle "until R3 brings the sprite atlas". R3 brought it and
/// they still have none, for a different reason: what a scaled section changes is the *glyph
/// vertex buffer*, and that buffer is elided from every symbol capture because mbgl packs its
/// glyph atlas in the order glyphs arrive and that order is not deterministic.
///
/// Measured rather than assumed. A capture of `["format", "Big", {"font-scale": 2}, "small",
/// {"font-scale": 0.5}]` and one of the same label at scale one produce *byte-identical*
/// comparable data — same vertex count, same index buffer hash, same both per-frame buffers.
/// The two maps differ visibly and the capture cannot tell them apart at all.
///
/// So implementing this against the oracle is not possible as the oracle stands. What would
/// change that is the probe, which is ours: a dump of the shaped extent — the line heights and
/// advances `shapeLines` computes — would be comparable where the packed vertices are not, and
/// is the number these features actually decide.
#[must_use]
pub fn shape(text: &[Char], options: &Options) -> Shaping {
    let justify = options.justify.factor();
    let (horizontal_align, vertical_align) = options.anchor.alignment();

    let mut shaping = Shaping::default();
    let mut y = Y_OFFSET;
    let mut max_line_length = 0.0f32;
    let mut max_line_height = 0.0f32;

    // Contextual forms first, then breaking, then reordering — mbgl's order, and each step
    // depends on the one before. The forms come from *logical* neighbours, so reordering first
    // would join every letter to whatever ended up beside it on screen; breaking is decided on
    // the logical order too.
    let shaped_text = apply_arabic(text);
    let text: &[Char] = &shaped_text;

    let broken = split_lines(text, options.max_width);
    let line_count = broken.len();

    for line in &broken {
        // Logical order until here; display order from here on. Breaking is decided on the
        // logical order — mbgl's note, and the reason this is not done before `split_lines`.
        let line = reorder(trim(line));
        let line = &*line;
        let mut glyphs: Vec<PositionedGlyph> = Vec::with_capacity(line.len());
        let mut x = 0.0f32;
        let mut last_advance = 0.0f32;

        // The largest scale on this line, which is what its height is set by and what every
        // glyph on it is offset against. mbgl's `line.getMaxScale()`: a line holding one
        // double-size word is a double-height line, and the small text on it sits on the same
        // baseline rather than floating at the top of it.
        let line_scale = line
            .iter()
            .map(|character| character.scale)
            .fold(1.0f32, f32::max);

        for character in line {
            if character.drawable {
                glyphs.push(PositionedGlyph {
                    codepoint: character.codepoint,
                    x,
                    // mbgl's `baselineOffset`. Laid out at one em, a glyph scaled differently
                    // from its line has to drop by the difference to keep its feet on the
                    // baseline; at the line's own scale it is zero, which is every ordinary
                    // label.
                    y: y + (line_scale - character.scale) * crate::text::ONE_EM,
                    scale: character.scale,
                });
                last_advance = character.advance;
            }
            // A zero-advance glyph is a combining mark sitting on the one before it, and must
            // not push the pen along or take the spacing.
            if character.advance > 0.01 {
                x += character.advance + options.spacing;
            }
        }

        if !glyphs.is_empty() {
            // The trailing spacing is not part of the line: it is the gap before a character
            // that never came.
            max_line_length = max_line_length.max(x - options.spacing);
            justify_line(&mut glyphs, last_advance, justify);
        }

        // `lineHeight * lineMaxScale`, so a line with a bigger section is a taller line.
        let line_height = options.line_height * line_scale;
        y += line_height;
        max_line_height = max_line_height.max(line_height);
        shaping.lines.push(glyphs);
    }

    let height = y - Y_OFFSET;
    let shift_x = (justify - horizontal_align) * max_line_length;
    // With every line the same height the offset is a whole number of lines from the middle;
    // the other branch is for lines that grew, which needs the per-section scaling this does
    // not implement yet.
    let shift_y = if (max_line_height - options.line_height).abs() > f32::EPSILON {
        -height * vertical_align - Y_OFFSET
    } else {
        #[allow(clippy::cast_precision_loss)]
        let lines = line_count as f32;
        (-vertical_align * lines + 0.5) * options.line_height
    };

    for line in &mut shaping.lines {
        for glyph in line.iter_mut() {
            glyph.x += shift_x;
            glyph.y += shift_y;
        }
    }

    shaping.top = -vertical_align * height;
    shaping.bottom = shaping.top + height;
    shaping.left = -horizontal_align * max_line_length;
    shaping.right = shaping.left + max_line_length;
    shaping
}
