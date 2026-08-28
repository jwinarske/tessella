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
    /// How far the pen moves for it: the glyph's own advance at its section's scale.
    ///
    /// Letter spacing is *not* in here, and mbgl is the reason. `shapeLines` lays out with
    /// `metrics.advance * scale` and adds the spacing itself; `getGlyphAdvance`, which line
    /// breaking measures with, returns the same thing *plus* the spacing. One number cannot be
    /// both, and folding the spacing in here made every spaced label a little wider than mbgl's
    /// — the shaper added it a second time.
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
    /// The sprite this character is, if it is an `["image", …]` section rather than text.
    ///
    /// Everything an image section needs travels with it, because nothing downstream can look a
    /// sprite up: the shaper is given advances rather than a font, and the quad builder asks for
    /// a glyph by codepoint — which an image does not have. mbgl reaches the sprite index from
    /// inside `shapeLines`; here the caller has already resolved it, and the resolved answer is
    /// what moves.
    pub image: Option<Image>,
}

/// A sprite drawn inline in a label.
///
/// Its size is in *logical* pixels, which is what `ImagePosition::displaySize` returns and is
/// the padded rectangle divided by the sprite's own pixel ratio. Both the shaper and the quad
/// builder need it and they need it for different things — the shaper for the line's height,
/// the builder for the quad's — so it is carried once rather than recomputed from the rectangle
/// at each.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Image {
    /// Its display size in logical pixels.
    pub size: (f32, f32),
    /// Its padded rectangle in the icon atlas.
    pub rect: crate::atlas::Rect,
    /// How many texture pixels the sprite has per logical one.
    pub pixel_ratio: f32,
    /// Whether it is a signed distance field rather than a picture.
    ///
    /// The shader has to know: an SDF is recoloured and haloed like a glyph, and a picture is
    /// drawn as it is. It is per *quad* rather than per drawable, because a label with an image
    /// in it draws both from one buffer.
    pub sdf: bool,
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
            image: None,
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
            image: None,
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
fn average_line_width(text: &[Char], max_width: f32, spacing: f32) -> f32 {
    let total: f32 = text
        .iter()
        .map(|character| character.advance + spacing)
        .sum();
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
pub fn line_breaks(text: &[Char], max_width: f32, spacing: f32) -> BTreeSet<usize> {
    if max_width <= 0.0 || text.is_empty() {
        return BTreeSet::new();
    }

    let target = average_line_width(text, max_width, spacing);
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut x = 0.0f32;

    // A server that has inserted zero-width spaces has told us where it wants breaks; breaking
    // between ideographs instead is then second-best rather than merely allowed.
    let suggested = text.iter().any(|character| character.codepoint == ZWSP);

    for (index, character) in text.iter().enumerate() {
        // Whitespace at a break is dropped, so it does not count toward the line's width.
        if !text::is_whitespace(character.codepoint) {
            // `getGlyphAdvance`, which is the advance *and* the spacing: breaking measures the
            // line as it will be set, and the gap after each character is part of that.
            x += character.advance + spacing;
        }

        if index + 1 >= text.len() {
            continue;
        }
        let ideographic = text::allows_ideographic_breaking(character.codepoint);
        // An image is always somewhere a line may break. mbgl says so directly — a sprite is a
        // unit of its own, and there is no reason to keep it on the same line as the word beside
        // it the way there is for two letters.
        if character.image.is_none()
            && !ideographic
            && !text::allows_word_breaking(character.codepoint)
        {
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
pub fn split_lines(text: &[Char], max_width: f32, spacing: f32) -> Vec<Vec<Char>> {
    let breaks = line_breaks(text, max_width, spacing);
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
    /// Whether this glyph keeps its upright orientation on a vertical line.
    ///
    /// Only ever true in a vertical shaping, and not for every glyph in one: a CJK ideograph
    /// stays upright and advances by a full em, while a Latin letter beside it is left alone so
    /// that rotating the whole line turns it a quarter turn with the line. Which of the two a
    /// character gets is [`crate::vertical`]'s answer, and the quad builder is where it is acted
    /// on.
    pub vertical: bool,
    /// The sprite it is, if it is an image section rather than a glyph.
    pub image: Option<Image>,
}

/// Which way a label's lines run.
///
/// Not a property of the label so much as a question asked of it twice: mbgl shapes a label
/// that permits it both ways and keeps both answers, because which one is drawn is a placement
/// decision that the shaper does not get to make. A vertical shaping is laid out along the same
/// axis as a horizontal one and turned by the quad builder — the glyphs run down the screen
/// because each quad is rotated, not because the shaper moved the pen downwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WritingMode {
    /// Lines run across.
    #[default]
    Horizontal,
    /// Lines run down.
    Vertical,
}

/// One line of a shaped label.
///
/// mbgl's `PositionedLine`, and the offset is why it is a type rather than a list of glyphs. A
/// line is normally as tall as the text on it; an image taller than an em pushes it down, and
/// how far is a property of the *line* — every glyph on it moves together, and the quad builder
/// needs the amount again afterwards to re-centre a vertical column.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Line {
    /// Its placed glyphs.
    pub glyphs: Vec<PositionedGlyph>,
    /// How far something taller than the text pushed this line down, in pixels.
    pub offset: f32,
}

impl From<Vec<PositionedGlyph>> for Line {
    /// A line of glyphs that nothing pushed down, which is every line without an image on it.
    fn from(glyphs: Vec<PositionedGlyph>) -> Self {
        Self {
            glyphs,
            offset: 0.0,
        }
    }
}

/// A shaped label: its glyphs in lines, and the box they occupy.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Shaping {
    /// The lines.
    pub lines: Vec<Line>,
    /// Top of the bounding box, relative to the anchor.
    pub top: f32,
    /// Bottom of the bounding box.
    pub bottom: f32,
    /// Left of the bounding box.
    pub left: f32,
    /// Right of the bounding box.
    pub right: f32,
    /// Whether any glyph in it stayed upright.
    ///
    /// mbgl's `Shaping::verticalizable`, and the quad builder reads it rather than the per-glyph
    /// flag to decide whether the *label* is one being set vertically — a line of rotated Latin
    /// with one ideograph in it is still a vertical line, and every glyph on it is centred as
    /// one.
    pub verticalizable: bool,
    /// Whether any section of it is an image rather than text.
    ///
    /// mbgl's `Shaping::iconsInText`, and it decides which shader the label is drawn with: a
    /// buffer holding both glyphs and sprites samples two textures, so the bucket carrying one
    /// binds `SymbolTextAndIconShader` rather than the plain SDF one.
    pub icons_in_text: bool,
}

impl Shaping {
    /// Whether anything was placed.
    ///
    /// A label of nothing but zero-width spaces shapes into lines that hold no glyphs, which is
    /// not the same as no lines: the lines still take vertical space, and mbgl's own test
    /// asserts five of them.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.iter().all(|line| line.glyphs.is_empty())
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
    /// Which way this shaping's lines run.
    pub writing_mode: WritingMode,
    /// Whether the layer permits vertical placement, which changes *which* glyphs stay upright.
    ///
    /// mbgl's `allowVerticalPlacement`, and the two answers are close to opposites. Without it —
    /// a label following a line that happens to run downwards — only a character with an upright
    /// orientation of its own is kept upright, and everything else turns with the line. With it —
    /// a point label the style asked to set vertically — everything is kept upright *except*
    /// whitespace and the scripts whose letters join, which would be broken by it.
    pub allow_vertical_placement: bool,
    /// `text-size` at this zoom, in pixels.
    ///
    /// Only an image section reads it, and it is the one measurement in the shaper that is not
    /// in ems. A glyph is laid out at one em and scaled by the text size later; a sprite has a
    /// size of its own in pixels, so to sit on a line of 16-pixel text at half height it has to
    /// be rescaled by `ONE_EM / text-size` — which is mbgl's `layoutTextSize` and is why its
    /// image branch computes a section scale of its own rather than using `font-scale` directly.
    pub text_size: f32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            max_width: 0.0,
            line_height: crate::text::ONE_EM,
            anchor: Anchor::Center,
            justify: Justify::Center,
            spacing: 0.0,
            writing_mode: WritingMode::Horizontal,
            allow_vertical_placement: false,
            text_size: 16.0,
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
fn justify_line(glyphs: &mut [PositionedGlyph], last_advance: f32, justify: f32, line_offset: f32) {
    if justify == 0.0 && line_offset == 0.0 {
        return;
    }
    let Some(last) = glyphs.last() else {
        return;
    };
    let indent = (last.x + last_advance) * justify;
    for glyph in glyphs.iter_mut() {
        glyph.x -= indent;
        // The line's own downward shift, applied with the horizontal one because mbgl applies
        // both here — a line an image pushed down moves as a whole, after it is set.
        glyph.y += line_offset;
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

/// Whether a character keeps its upright orientation on a vertical line.
///
/// mbgl's condition, which is written there as the negation of four disqualifications and reads
/// more plainly split in two. Nothing is upright in a horizontal shaping. In a vertical one,
/// what counts depends on *why* the line is vertical: a label following a line that runs
/// downwards keeps upright only the characters that have an upright form of their own, and turns
/// the rest with the line; a label the style asked to set vertically keeps everything upright
/// except whitespace and the scripts whose letters join.
///
/// mbgl has a third disqualification, for a glyph that did not come from a font PBF. That is its
/// HarfBuzz path for locally-installed fonts, where the type is generated per font; every glyph
/// here comes from a PBF, so the test is constant and is not transcribed.
fn is_vertical(codepoint: u32, options: &Options) -> bool {
    if options.writing_mode == WritingMode::Horizontal {
        return false;
    }
    if options.allow_vertical_placement {
        !(text::is_whitespace(codepoint) || crate::vertical::is_complex_shaping(codepoint))
    } else {
        crate::vertical::is_upright(codepoint)
    }
}

/// Lays a label out: breaks it into lines, places its glyphs, and aligns the result.
///
/// A transcription of mbgl's `shapeLines` for one font stack, both writing modes.
///
/// # What it does not do
///
/// Images in text. A `["image", …]` section draws from the sprite atlas rather than the font,
/// and its metrics come from the sprite's display size — so it changes what a line's *height*
/// is as well as its width, and the shaper cannot ask a `Char` for any of it.
///
/// # How the other two came to be checkable
///
/// Per-section scaling and vertical writing were both listed here as unimplementable against the
/// oracle, and the reason given was wrong twice. First it said they waited on the sprite atlas;
/// R3 brought it and nothing changed. Then it said what they change is the *glyph vertex
/// buffer*, which every symbol capture elides because mbgl packs its glyph atlas in the order
/// glyphs arrive and that order is not deterministic. That was true, and measured: a capture of
/// `["format", "Big", {"font-scale": 2}, …]` and one of the same label unscaled produced
/// byte-identical comparable data.
///
/// What changed is the probe, which is this project's. It hashes each attribute over its *own*
/// bytes now rather than over the buffer it shares, so the two attributes that carry no texture
/// coordinates stopped being elided along with the one that does — and those two are exactly
/// where a scaled section and a turned glyph show up.
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

    let broken = split_lines(text, options.max_width, options.spacing);
    let line_count = broken.len();

    for line in &broken {
        // Logical order until here; display order from here on. Breaking is decided on the
        // logical order — mbgl's note, and the reason this is not done before `split_lines`.
        let line = reorder(trim(line));
        let line = &*line;
        let mut glyphs: Vec<PositionedGlyph> = Vec::with_capacity(line.len());
        let mut x = 0.0f32;
        let mut last_advance = 0.0f32;
        // How far an image taller than the line's text pushes this line down. Zero for text.
        let mut line_offset = 0.0f32;

        // The largest scale on this line, which is what its height is set by and what every
        // glyph on it is offset against. mbgl's `line.getMaxScale()`: a line holding one
        // double-size word is a double-height line, and the small text on it sits on the same
        // baseline rather than floating at the top of it.
        let line_scale = line
            .iter()
            .map(|character| character.scale)
            .fold(1.0f32, f32::max);

        // `(lineMaxScale - 1) * ONE_EM`: how far the line's baseline has already dropped to make
        // room for its largest text, which an image has to clear as well as its own height.
        let max_line_offset = (line_scale - 1.0) * crate::text::ONE_EM;

        for character in line {
            let vertical = is_vertical(character.codepoint, options);

            // An image section is measured from the sprite rather than the font, and everything
            // about it differs: its scale is rescaled out of pixels into ems, its baseline
            // offset aligns its *bottom* to the line rather than its feet, and if it is taller
            // than the line it makes the line taller instead of overlapping what is above.
            let (scale, baseline_offset, advance) = match character.image {
                Some(image) => {
                    let scale = character.scale * crate::text::ONE_EM / options.text_size;
                    // The gap between one em and the image's own height, which is what sits
                    // between the image's bottom and the baseline.
                    let image_offset = crate::text::ONE_EM - image.size.1 * scale;
                    let advance = if vertical { image.size.1 } else { image.size.0 };

                    // A sprite bigger than the line's em box pushes the whole line down by the
                    // difference rather than growing upwards into the line above.
                    let over = if vertical { image.size.0 } else { image.size.1 } * scale
                        - crate::text::ONE_EM * line_scale;
                    if over > 0.0 && over > line_offset {
                        line_offset = over;
                    }
                    (scale, max_line_offset + image_offset, advance * scale)
                }
                // mbgl's `baselineOffset`. Laid out at one em, a glyph scaled differently
                // from its line has to drop by the difference to keep its feet on the
                // baseline; at the line's own scale it is zero, which is every ordinary
                // label.
                None => (
                    character.scale,
                    (line_scale - character.scale) * crate::text::ONE_EM,
                    character.advance,
                ),
            };

            if character.drawable {
                glyphs.push(PositionedGlyph {
                    codepoint: character.codepoint,
                    x,
                    y: y + baseline_offset,
                    scale,
                    vertical,
                    image: character.image,
                });
                last_advance = advance;
            }
            if character.image.is_some() {
                shaping.icons_in_text = true;
            }
            if vertical {
                // An upright glyph on a vertical line advances by a full em whatever its own
                // advance is, because the line is a column of square cells and the glyph sits in
                // one. An image is the exception: it advances by its own height, which is what
                // it occupies going down. Unconditionally either way — the zero-advance guard
                // below is about a combining mark riding the glyph before it, and a mark that
                // stays upright still takes its own cell.
                x += if character.image.is_some() {
                    advance
                } else {
                    crate::text::ONE_EM * scale
                } + options.spacing;
                shaping.verticalizable = true;
            } else if advance > 0.01 {
                // A zero-advance glyph is a combining mark sitting on the one before it, and must
                // not push the pen along or take the spacing.
                x += advance + options.spacing;
            }
        }

        if !glyphs.is_empty() {
            // The trailing spacing is not part of the line: it is the gap before a character
            // that never came.
            max_line_length = max_line_length.max(x - options.spacing);
            justify_line(&mut glyphs, last_advance, justify, line_offset);
        }

        // `lineHeight * lineMaxScale`, so a line with a bigger section is a taller line — plus
        // whatever an oversized image added to it.
        let line_height = options.line_height * line_scale + line_offset;
        y += line_height;
        // A line with no characters at all still takes its space, but it does not set the
        // maximum: mbgl returns from the loop before reaching that, and the difference decides
        // which branch the whole block's alignment takes. A label that is nothing but blank
        // lines has a maximum of zero there, which is not its line height, so it aligns as a
        // block that grew rather than as a whole number of lines.
        if !line.is_empty() {
            max_line_height = max_line_height.max(line_height);
        }
        shaping.lines.push(Line {
            glyphs,
            // The larger of the two, because both are reasons this line's glyphs sit lower than
            // the line above's and the quad builder needs the total when it re-centres a column.
            offset: line_offset.max(max_line_offset),
        });
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
        for glyph in &mut line.glyphs {
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
