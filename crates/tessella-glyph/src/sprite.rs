//! The sprite index: where each icon sits in a style's sprite sheet.
//!
//! A transcription of mbgl's `SpriteParser`. A style names one sprite URL and the origin serves
//! two resources for it — a JSON index and an image — and the index is what says which rectangle
//! of that image an `icon-image` refers to.
//!
//! # Almost all of it is refusal
//!
//! The index is hand-written or tool-generated JSON with no schema enforcing anything, and every
//! field can be wrong in a way that is not a parse error. A negative width, a rectangle that runs
//! off the sheet, a pixel ratio of zero: each of those reads fine and then either draws garbage
//! or divides by zero somewhere far away. mbgl checks the lot in `createStyleImage` and drops the
//! entry rather than the sheet, which is what a style with one bad icon needs — the other three
//! hundred still draw.
//!
//! The bounds are mbgl's own numbers and are transcribed rather than chosen: a dimension over
//! 1024, or a pixel ratio outside `0 < ratio <= 10`, is refused.
//!
//! # Stretches are what make a shield fit its text
//!
//! A route shield is drawn around a label whose width is not known when the sprite was made, so
//! the icon says which of its *columns* and *rows* may be stretched and which must not. `content`
//! is the box the text goes in. All three are optional and all three are in the sprite's own
//! pixels, before the pixel ratio is applied.

use std::collections::BTreeMap;

use serde_json::Value;

/// The largest dimension an icon may have, as mbgl bounds it.
pub const MAX_DIMENSION: i64 = 1024;

/// The largest pixel ratio, as mbgl bounds it.
pub const MAX_PIXEL_RATIO: f64 = 10.0;

/// A stretchable range of an icon, in its own pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stretch {
    /// Where it starts.
    pub from: f64,
    /// Where it ends.
    pub to: f64,
}

/// The box an icon's text is laid into.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Content {
    /// Left edge.
    pub left: f64,
    /// Top edge.
    pub top: f64,
    /// Right edge.
    pub right: f64,
    /// Bottom edge.
    pub bottom: f64,
}

/// One icon's entry in the index.
#[derive(Debug, Clone, PartialEq)]
pub struct Sprite {
    /// Left edge in the sheet, in sheet pixels.
    pub x: u32,
    /// Top edge in the sheet.
    pub y: u32,
    /// Width in the sheet.
    pub width: u32,
    /// Height in the sheet.
    pub height: u32,
    /// How many sheet pixels make one logical pixel.
    ///
    /// A `@2x` sheet has ratio 2, so a 32-pixel icon occupies 64 pixels of sheet. Everything
    /// downstream measures in logical pixels, which is why this is carried rather than folded
    /// in: folding it into the rectangle would lose the sheet coordinates the upload needs.
    pub pixel_ratio: f64,
    /// Whether the icon is a distance field, drawn through the SDF shader and recolourable.
    pub sdf: bool,
    /// Columns that may stretch.
    pub stretch_x: Vec<Stretch>,
    /// Rows that may stretch.
    pub stretch_y: Vec<Stretch>,
    /// Where text goes inside it.
    pub content: Option<Content>,
}

impl Sprite {
    /// Its size in logical pixels, which is what layout measures in.
    #[must_use]
    pub fn logical_size(&self) -> (f64, f64) {
        (
            f64::from(self.width) / self.pixel_ratio,
            f64::from(self.height) / self.pixel_ratio,
        )
    }
}

/// A style's sprite index.
pub type Index = BTreeMap<String, Sprite>;

/// Why an index could not be read at all.
///
/// A *malformed entry* is not one of these — it is dropped and the rest of the sheet is kept,
/// which is mbgl's behaviour and the one a style with one bad icon needs.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpriteError {
    /// The body is not JSON.
    #[error("the sprite index is not JSON: {0}")]
    Json(String),
    /// The body is JSON but not an object of entries.
    #[error("the sprite index is not an object")]
    NotAnObject,
}

fn number(entry: &serde_json::Map<String, Value>, key: &str) -> Option<f64> {
    entry.get(key)?.as_f64()
}

fn stretches(entry: &serde_json::Map<String, Value>, key: &str) -> Vec<Stretch> {
    let Some(Value::Array(ranges)) = entry.get(key) else {
        return Vec::new();
    };
    ranges
        .iter()
        .filter_map(|range| {
            let pair = range.as_array()?;
            // A range that is not exactly two numbers is not a range. Taking the first two of a
            // longer one would silently accept `[0, 4, 9]` as `[0, 4]`.
            if pair.len() != 2 {
                return None;
            }
            Some(Stretch {
                from: pair[0].as_f64()?,
                to: pair[1].as_f64()?,
            })
        })
        .collect()
}

fn content(entry: &serde_json::Map<String, Value>) -> Option<Content> {
    let box_ = entry.get("content")?.as_array()?;
    if box_.len() != 4 {
        return None;
    }
    Some(Content {
        left: box_[0].as_f64()?,
        top: box_[1].as_f64()?,
        right: box_[2].as_f64()?,
        bottom: box_[3].as_f64()?,
    })
}

/// Reads one entry, or `None` when it is unusable.
///
/// `sheet` is the image's size, and is what the rectangle is checked against — an entry naming a
/// rectangle outside the sheet would sample whatever is past the end of the buffer. `None` for
/// the sheet skips that check, which is what a caller reading the index before the image has
/// arrived has to do.
fn read(value: &Value, sheet: Option<(u32, u32)>) -> Option<Sprite> {
    let entry = value.as_object()?;

    let width = number(entry, "width")?;
    let height = number(entry, "height")?;
    let x = number(entry, "x")?;
    let y = number(entry, "y")?;
    // The spec's default, and the one a sheet without `@2x` uses.
    let pixel_ratio = number(entry, "pixelRatio").unwrap_or(1.0);

    // mbgl's `createStyleImage` bounds, transcribed. Each of these reads as valid JSON and then
    // goes wrong somewhere else: a zero ratio divides by zero in `logical_size`, a negative
    // dimension wraps when it reaches an unsigned rectangle, and an oversized one is a sheet
    // nothing can upload.
    //
    // Written as a negated `>` rather than as `<=` on purpose, and not for style: `<= 0`
    // *accepts* a NaN, since every comparison against one is false, and would then hand an
    // unsigned cast a value with no defined result. Defensive rather than load-bearing today —
    // JSON has no NaN literal and a number out of range fails the whole document before it
    // reaches here — and the defensive form costs nothing while tidying it does not.
    #[allow(clippy::neg_cmp_op_on_partial_ord)]
    if !(width > 0.0)
        || !(height > 0.0)
        || width > MAX_DIMENSION as f64
        || height > MAX_DIMENSION as f64
    {
        return None;
    }
    #[allow(clippy::neg_cmp_op_on_partial_ord)]
    if !(pixel_ratio > 0.0) || pixel_ratio > MAX_PIXEL_RATIO {
        return None;
    }
    if x < 0.0 || y < 0.0 {
        return None;
    }
    // Whole pixels. A fractional rectangle has no meaning against a texture and would round
    // differently in the two places that read it.
    if x.fract() != 0.0 || y.fract() != 0.0 || width.fract() != 0.0 || height.fract() != 0.0 {
        return None;
    }

    if let Some((sheet_width, sheet_height)) = sheet {
        let (sheet_width, sheet_height) = (f64::from(sheet_width), f64::from(sheet_height));
        if x >= sheet_width || y >= sheet_height {
            return None;
        }
        if x + width > sheet_width || y + height > sheet_height {
            return None;
        }
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some(Sprite {
        x: x as u32,
        y: y as u32,
        width: width as u32,
        height: height as u32,
        pixel_ratio,
        sdf: entry.get("sdf").and_then(Value::as_bool).unwrap_or(false),
        stretch_x: stretches(entry, "stretchX"),
        stretch_y: stretches(entry, "stretchY"),
        content: content(entry),
    })
}

/// Parses a sprite index.
///
/// `sheet` is the image's size when it is known. An entry that is unusable is dropped and the
/// rest are kept: a style with one bad icon still draws the other three hundred.
///
/// # Errors
///
/// [`SpriteError`] when the body is not JSON or is not an object. Those are failures of the
/// whole resource rather than of one entry, and answering with an empty index would be a style
/// that silently has no icons.
pub fn parse(body: &[u8], sheet: Option<(u32, u32)>) -> Result<Index, SpriteError> {
    let value: Value =
        serde_json::from_slice(body).map_err(|error| SpriteError::Json(error.to_string()))?;
    let Value::Object(entries) = value else {
        return Err(SpriteError::NotAnObject);
    };

    let mut out = Index::new();
    for (name, entry) in entries {
        if let Some(sprite) = read(&entry, sheet) {
            out.insert(name, sprite);
        }
    }
    Ok(out)
}

/// The two URLs a sprite source resolves to, for a given pixel ratio.
///
/// mbgl's `SpriteLoader`: the style's `sprite` is a *base*, and the suffix goes before the
/// extension rather than after the whole URL. A query string is kept and the suffix goes in
/// front of it, which is what makes a signed sprite URL work.
#[must_use]
pub fn urls(base: &str, pixel_ratio: f64) -> (String, String) {
    // mbgl uses `@2x` for anything above 1, and nothing at or below it.
    let suffix = if pixel_ratio > 1.0 { "@2x" } else { "" };

    let (path, query) = match base.find('?') {
        Some(at) => (&base[..at], &base[at..]),
        None => (base, ""),
    };

    (
        format!("{path}{suffix}.json{query}"),
        format!("{path}{suffix}.png{query}"),
    )
}

/// A decoded sprite sheet: the pixels the index's rectangles point into.
///
/// RGBA, eight bits a channel, which is what the style spec's sprites are and what the wire's
/// `RGBA` pixel type means. Not premultiplied — mbgl premultiplies on upload and the capture's
/// texture hash is over the decoded image, so premultiplying here would put different bytes on
/// the wire than the oracle has.
#[cfg(feature = "png")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sheet {
    /// Width in sheet pixels.
    pub width: u32,
    /// Height in sheet pixels.
    pub height: u32,
    /// `width * height * 4` bytes.
    pub pixels: Vec<u8>,
}

#[cfg(feature = "png")]
impl Sheet {
    /// Its size, in the shape the index's bounds check takes.
    #[must_use]
    pub const fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

/// Why a sheet could not be decoded.
#[cfg(feature = "png")]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SheetError {
    /// The bytes are not a PNG this decoder reads.
    #[error("the sprite sheet did not decode: {0}")]
    Decode(String),
    /// The image decoded to a size or depth nothing can index.
    #[error("the sprite sheet is {width}x{height}, which is not a usable sheet")]
    Unusable {
        /// Decoded width.
        width: u32,
        /// Decoded height.
        height: u32,
    },
}

/// Decodes a sprite sheet.
///
/// Every colour type is widened to RGBA: a sheet may be greyscale, palletted or RGB, and the
/// rectangle the index hands out is in pixels rather than bytes — so a decoder that returned the
/// source's own channel count would make every offset downstream depend on the file's encoding.
///
/// # Errors
///
/// [`SheetError`] when the bytes are not a PNG, or when the image has no area.
#[cfg(feature = "png")]
pub fn decode_sheet(body: &[u8]) -> Result<Sheet, SheetError> {
    use zune_png::PngDecoder;
    use zune_png::zune_core::bytestream::ZCursor;
    use zune_png::zune_core::options::DecoderOptions;

    // Ask for eight-bit RGBA outright rather than converting afterwards. A sixteen-bit sheet
    // decoded at its own depth would be twice the bytes with the same rectangles, and the
    // mismatch would show as every icon sampling half of its neighbour.
    let options = DecoderOptions::default()
        .png_set_strip_to_8bit(true)
        .png_set_add_alpha_channel(true);
    let mut decoder = PngDecoder::new_with_options(ZCursor::new(body), options);

    let pixels = decoder
        .decode_raw()
        .map_err(|error| SheetError::Decode(alloc_string(&error)))?;
    let (width, height) = decoder
        .dimensions()
        .ok_or_else(|| SheetError::Decode("the header carries no dimensions".to_string()))?;

    #[allow(clippy::cast_possible_truncation)]
    let (width, height) = (width as u32, height as u32);
    if width == 0 || height == 0 {
        return Err(SheetError::Unusable { width, height });
    }

    let colorspace = decoder
        .colorspace()
        .ok_or_else(|| SheetError::Decode("the header carries no colorspace".to_string()))?;
    let pixels = widen(&pixels, colorspace, width, height)?;

    Ok(Sheet {
        width,
        height,
        pixels,
    })
}

#[cfg(feature = "png")]
fn alloc_string(error: &impl core::fmt::Debug) -> String {
    format!("{error:?}")
}

/// Widens whatever the decoder produced into RGBA.
#[cfg(feature = "png")]
fn widen(
    pixels: &[u8],
    colorspace: zune_png::zune_core::colorspace::ColorSpace,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, SheetError> {
    use zune_png::zune_core::colorspace::ColorSpace;

    let count = (width as usize) * (height as usize);
    let expand = |channels: usize, place: fn(&[u8], &mut [u8; 4])| {
        if pixels.len() < count * channels {
            return None;
        }
        let mut out = Vec::with_capacity(count * 4);
        for source in pixels.chunks_exact(channels).take(count) {
            let mut rgba = [0u8; 4];
            place(source, &mut rgba);
            out.extend_from_slice(&rgba);
        }
        Some(out)
    };

    let widened = match colorspace {
        ColorSpace::RGBA => (pixels.len() >= count * 4).then(|| pixels[..count * 4].to_vec()),
        ColorSpace::RGB => expand(3, |source, rgba| {
            *rgba = [source[0], source[1], source[2], 255];
        }),
        ColorSpace::LumaA => expand(2, |source, rgba| {
            *rgba = [source[0], source[0], source[0], source[1]];
        }),
        ColorSpace::Luma => expand(1, |source, rgba| {
            *rgba = [source[0], source[0], source[0], 255];
        }),
        other => {
            return Err(SheetError::Decode(format!(
                "a sprite sheet in {other:?} is not a colour type a sheet uses"
            )));
        }
    };

    widened.ok_or(SheetError::Unusable { width, height })
}

/// A style's sprite resource: the index and the sheet it points into.
///
/// The icon counterpart of [`Fonts`], and simpler in the way that matters — there is nothing to
/// pack. A glyph atlas is built by this process out of ranges that arrive separately; a sprite
/// sheet arrives already laid out, and the index is its map. So the store fetches two resources
/// once and holds them.
///
/// [`Fonts`]: crate::fonts::Fonts
#[cfg(feature = "png")]
#[derive(Debug)]
pub struct Sprites {
    base: String,
    pixel_ratio: f64,
    index: Index,
    sheet: Option<Sheet>,
    /// Whether the sheet has been uploaded since it last changed — §6.4's damage, for a
    /// resource that changes exactly once.
    dirty: bool,
}

#[cfg(feature = "png")]
impl Sprites {
    /// An empty store for a style's `sprite` base at a device pixel ratio.
    #[must_use]
    pub fn new(base: impl Into<String>, pixel_ratio: f64) -> Self {
        Self {
            base: base.into(),
            pixel_ratio,
            index: Index::new(),
            sheet: None,
            dirty: false,
        }
    }

    /// Fetches the index and the sheet, in that order.
    ///
    /// The sheet is fetched first in wall-clock terms by any real transport, but the *index* is
    /// parsed against the sheet's size — a rectangle running off the image is refused, and that
    /// check needs the image. So the image is decoded before the index is read, and a style whose
    /// sheet fails to decode gets no icons rather than icons pointing at nothing.
    ///
    /// Idempotent: a second call with the resource already held fetches nothing. A style has one
    /// sprite and every tile of it asks the same question.
    ///
    /// # Errors
    ///
    /// [`SpriteError`] when the index is not readable. A sheet that fails to decode is reported
    /// as [`SpriteError::Json`]'s sibling — see [`SheetError`] — through [`Self::load`], which is
    /// what a caller with its own transport uses.
    pub fn fetch(&mut self, files: &dyn tessella_storage::FileSource) -> Result<bool, LoadError> {
        if self.sheet.is_some() {
            return Ok(false);
        }
        let (index_url, image_url) = urls(&self.base, self.pixel_ratio);

        let image = files
            .fetch(&image_url)
            .map_err(|source| LoadError::Fetch(source.to_string()))?;
        let json = files
            .fetch(&index_url)
            .map_err(|source| LoadError::Fetch(source.to_string()))?;

        self.load(&json.body, &image.body)?;
        Ok(true)
    }

    /// Reads an index and a sheet that a caller already has.
    ///
    /// # Errors
    ///
    /// [`LoadError`] when either half is unreadable. Both are required: an index without a sheet
    /// names rectangles in an image that does not exist, and a sheet without an index has no
    /// names to look them up by.
    pub fn load(&mut self, index: &[u8], image: &[u8]) -> Result<(), LoadError> {
        let sheet = decode_sheet(image).map_err(LoadError::Sheet)?;
        let parsed = parse(index, Some(sheet.size())).map_err(LoadError::Index)?;

        self.index = parsed;
        self.sheet = Some(sheet);
        self.dirty = true;
        Ok(())
    }

    /// The index, for laying icons out.
    #[must_use]
    pub const fn index(&self) -> &Index {
        &self.index
    }

    /// One icon, by the name a layer asked for.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Sprite> {
        self.index.get(name)
    }

    /// The sheet, once it has arrived.
    #[must_use]
    pub const fn sheet(&self) -> Option<&Sheet> {
        self.sheet.as_ref()
    }

    /// Whether the sheet needs uploading, clearing the flag.
    ///
    /// A sprite sheet changes once in a style's life, so this answers true once. §6.5's still
    /// frame is a frame with no envelopes in it, and re-uploading a megabyte of unchanged icons
    /// every frame would make a settled map the most expensive one.
    pub fn take_dirty(&mut self) -> bool {
        core::mem::replace(&mut self.dirty, false)
    }
}

/// Why a sprite resource could not be loaded.
#[cfg(feature = "png")]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LoadError {
    /// The transport failed. The resource stays unheld, so a later call tries again.
    #[error("the sprite could not be fetched: {0}")]
    Fetch(String),
    /// The sheet did not decode.
    #[error(transparent)]
    Sheet(#[from] SheetError),
    /// The index was not readable.
    #[error(transparent)]
    Index(#[from] SpriteError),
}
