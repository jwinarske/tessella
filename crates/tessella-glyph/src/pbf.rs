//! The SDF glyph range format: `{fontstack}/{start}-{end}.pbf`.
//!
//! A transcription of mbgl's `parseGlyphPBF`. A range is 256 codepoints of one font stack, each
//! glyph carrying its metrics and a signed-distance-field bitmap; a label is shaped by looking
//! its codepoints up here and a range is fetched whenever a label needs one it does not have.
//!
//! # Almost all of this function is rejection
//!
//! The parse itself is six fields. What matters is which glyphs are refused, because the
//! consequence of accepting a bad one is not an error — it is a bitmap read past its end, or a
//! label laid out against metrics that do not describe it.
//!
//! Every one of the six fields is required. Proto2 makes them all optional on the wire, so a
//! glyph missing `advance` parses perfectly and then lays out on top of its neighbour. The
//! metric bounds are the same: mbgl checks `width < 256`, `left` in `-128..128` and so on, not
//! because the wire type cannot hold more but because a glyph outside those ranges is a
//! misencoded file rather than an unusual letter.
//!
//! The bitmap check is the one that would otherwise be a read past the end. A glyph declares
//! its own width and height, and the bitmap it carries must be exactly
//! `(width + 2 * BORDER) * (height + 2 * BORDER)` bytes. A glyph whose bitmap disagrees with
//! its metrics is dropped rather than clamped: clamping produces a letter with a torn edge and
//! no indication of why.
//!
//! # Zero-area glyphs are legitimate and keep their metrics
//!
//! A space has an advance and no pixels. Those carry no bitmap and are *not* rejected — the
//! shaper needs the advance, and a range that dropped its spaces would set text with the words
//! run together.

use tessella_source::protobuf::{Reader, WireError};

/// The border, in pixels, around every glyph's SDF.
///
/// Three, and not a choice: the whole Mapbox GL ecosystem encodes glyphs with this border, and
/// mbgl says in as many words that changing it means re-encoding the glyphs.
pub const BORDER: u32 = 3;

/// How many codepoints one range file covers.
pub const RANGE_SIZE: u32 = 256;

/// The highest codepoint the range scheme addresses.
///
/// mbgl stops at the Basic Multilingual Plane; a codepoint above it is served by the local
/// rasterizer rather than by a range file.
pub const MAX_CODEPOINT: u32 = 65535;

/// Why a glyph range could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GlyphError {
    /// The protobuf did not decode.
    #[error("reading the glyph range: {0}")]
    Wire(#[from] WireError),
}

/// One glyph's layout metrics, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Metrics {
    /// Bitmap width, excluding the border.
    pub width: u32,
    /// Bitmap height, excluding the border.
    pub height: u32,
    /// Horizontal bearing: where the bitmap sits relative to the pen.
    pub left: i32,
    /// Vertical bearing, from the baseline.
    pub top: i32,
    /// How far the pen advances after this glyph.
    pub advance: u32,
}

/// One glyph: its codepoint, its metrics, and its distance field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Glyph {
    /// The codepoint.
    pub id: u32,
    /// Layout metrics.
    pub metrics: Metrics,
    /// The signed distance field, `(width + 2 * BORDER) * (height + 2 * BORDER)` bytes, single
    /// channel. Empty for a glyph with no pixels, which a space legitimately is.
    pub bitmap: Vec<u8>,
}

impl Glyph {
    /// The bitmap's dimensions including the border, or `None` when it has no pixels.
    #[must_use]
    pub const fn bitmap_size(&self) -> Option<(u32, u32)> {
        if self.metrics.width == 0 || self.metrics.height == 0 {
            return None;
        }
        Some((
            self.metrics.width + 2 * BORDER,
            self.metrics.height + 2 * BORDER,
        ))
    }
}

/// A block of 256 codepoints, which is the unit a glyph file is served in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Range {
    /// First codepoint, always a multiple of [`RANGE_SIZE`].
    pub first: u32,
    /// Last codepoint, always `first + RANGE_SIZE - 1`.
    pub last: u32,
}

impl Range {
    /// The range holding `codepoint`.
    ///
    /// `None` above [`MAX_CODEPOINT`]: those are not served as ranges. Returning a range for
    /// them would produce a URL no origin answers, and a label that silently lost its glyphs.
    #[must_use]
    pub const fn of(codepoint: u32) -> Option<Self> {
        if codepoint > MAX_CODEPOINT {
            return None;
        }
        let first = codepoint / RANGE_SIZE * RANGE_SIZE;
        Some(Self {
            first,
            last: first + RANGE_SIZE - 1,
        })
    }

    /// Whether this range covers `codepoint`.
    #[must_use]
    pub const fn contains(&self, codepoint: u32) -> bool {
        codepoint >= self.first && codepoint <= self.last
    }
}

impl core::fmt::Display for Range {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(formatter, "{}-{}", self.first, self.last)
    }
}

/// Whether a glyph's metrics are within the bounds a range file may state.
///
/// mbgl's conditions exactly. A glyph outside them is a misencoded file rather than an unusual
/// letter, and is dropped.
const fn metrics_are_sane(metrics: &Metrics) -> bool {
    metrics.width < 256
        && metrics.height < 256
        && metrics.left >= -128
        && metrics.left < 128
        && metrics.top >= -128
        && metrics.top < 128
        && metrics.advance < 256
}

/// Reads one glyph message, returning it only if every required field was present and sane.
fn glyph(bytes: &[u8], range: Range) -> Result<Option<Glyph>, GlyphError> {
    let mut reader = Reader::new(bytes);
    let mut metrics = Metrics::default();
    let mut id = None;
    let mut bitmap: &[u8] = &[];
    // Proto2 makes every field optional on the wire, so presence is tracked rather than
    // inferred from a zero: a glyph with no `advance` is malformed, but a glyph with an
    // advance of zero is a legitimate combining mark.
    let (mut has_width, mut has_height) = (false, false);
    let (mut has_left, mut has_top, mut has_advance) = (false, false, false);

    while let Some(field) = reader.next_field() {
        let (number, wire) = field?;
        match number {
            1 => id = Some(reader.varint()? as u32),
            2 => bitmap = reader.delimited()?,
            3 => {
                metrics.width = reader.varint()? as u32;
                has_width = true;
            }
            4 => {
                metrics.height = reader.varint()? as u32;
                has_height = true;
            }
            5 => {
                metrics.left = zigzag64(reader.varint()?);
                has_left = true;
            }
            6 => {
                metrics.top = zigzag64(reader.varint()?);
                has_top = true;
            }
            7 => {
                metrics.advance = reader.varint()? as u32;
                has_advance = true;
            }
            _ => reader.skip(wire)?,
        }
    }

    let Some(id) = id else { return Ok(None) };
    if !has_width || !has_height || !has_left || !has_top || !has_advance {
        return Ok(None);
    }
    if !metrics_are_sane(&metrics) || !range.contains(id) {
        return Ok(None);
    }

    // A glyph with pixels must carry exactly the bitmap its metrics describe. Anything else is
    // a read past the end waiting to happen, so the glyph goes rather than the bitmap.
    let bitmap = if metrics.width > 0 && metrics.height > 0 {
        let expected = ((metrics.width + 2 * BORDER) * (metrics.height + 2 * BORDER)) as usize;
        if bitmap.len() != expected {
            return Ok(None);
        }
        bitmap.to_vec()
    } else {
        // No pixels, so no bitmap is kept even if one was sent — a space has an advance and
        // nothing to draw, and its metrics are what the shaper needs.
        Vec::new()
    };

    Ok(Some(Glyph {
        id,
        metrics,
        bitmap,
    }))
}

/// Zigzag-decodes a 64-bit varint into the signed value it stands for.
///
/// The metrics are `sint64` on the wire. `protobuf::zigzag` takes a `u32`, which would truncate
/// a negative before decoding it and turn a bearing of -1 into a large positive.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
const fn zigzag64(value: u64) -> i32 {
    (((value >> 1) as i64) ^ -((value & 1) as i64)) as i32
}

/// Parses a glyph range file.
///
/// Glyphs outside `range`, and glyphs that are malformed by any of the conditions above, are
/// dropped rather than reported: a range file with one bad glyph is still the source of the
/// other 255, and failing the parse would cost a whole block of text for one letter.
///
/// # Errors
///
/// [`GlyphError`] when the protobuf itself does not decode, which is a different thing from a
/// glyph inside it being unusable.
pub fn parse(range: Range, bytes: &[u8]) -> Result<Vec<Glyph>, GlyphError> {
    let mut out = Vec::new();
    let mut reader = Reader::new(bytes);

    // stacks
    while let Some(field) = reader.next_field() {
        let (number, wire) = field?;
        if number != 1 {
            reader.skip(wire)?;
            continue;
        }
        let mut stack = Reader::new(reader.delimited()?);
        while let Some(field) = stack.next_field() {
            let (number, wire) = field?;
            if number != 3 {
                stack.skip(wire)?;
                continue;
            }
            if let Some(parsed) = glyph(stack.delimited()?, range)? {
                out.push(parsed);
            }
        }
    }

    Ok(out)
}
