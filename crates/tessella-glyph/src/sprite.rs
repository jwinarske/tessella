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

/// The width and height of the icon atlas.
///
/// mbgl starts its dynamic texture at 512 square and grows it. This does not grow, for the
/// reason the glyph atlas does not: a rectangle handed out for a texture the consumer has
/// already uploaded cannot move. A thousand and twenty-four square holds a street style's whole
/// icon set with room over, since each icon is tens of pixels rather than hundreds.
#[cfg(feature = "image")]
pub const ATLAS_SIZE: u32 = 1024;

/// The largest dimension an icon may have, as mbgl bounds it.
pub const MAX_DIMENSION: u64 = 1024;

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
    /// How the icon stretches horizontally to fit its text.
    pub text_fit_width: Option<TextFit>,
    /// And vertically.
    pub text_fit_height: Option<TextFit>,
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

/// Where one icon sits in the *icon atlas*, once it has been cut from the sheet and packed.
///
/// mbgl's `ImagePosition`. Not the sprite's rectangle in the sheet, and the difference is the
/// whole point: `parseSprite` copies each icon out of the sheet into an image of its own, and
/// `DynamicTextureAtlas` packs those into a texture with a pixel of padding around each. The
/// sheet is a *transport* for the icons, not the texture they are drawn from.
///
/// The rectangle here includes that one pixel on every side — mbgl's `paddedRect` — which is
/// what the icon quad's one-pixel border samples. Handing out the sheet rectangle instead makes
/// that border sample the neighbouring icon, which draws a hairline of the wrong picture around
/// every marker on the map.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IconPosition {
    /// Its rectangle in the atlas, including a pixel of padding on every side.
    pub padded_rect: crate::atlas::Rect,
    /// How many atlas pixels make one logical pixel.
    pub pixel_ratio: f64,
    /// Whether it is a distance field.
    pub sdf: bool,
    /// The box text is laid into, in the sprite's own pixels.
    pub content: Option<Content>,
    /// How the icon may stretch horizontally around that text.
    pub text_fit_width: Option<TextFit>,
    /// And vertically.
    pub text_fit_height: Option<TextFit>,
}

impl IconPosition {
    /// The margins between the content box and the icon's own edges, in logical pixels.
    ///
    /// Zero when the sprite names no content box. Ordered top, bottom, left, right.
    #[must_use]
    pub fn content_margins(&self) -> (f32, f32, f32, f32) {
        let Some(content) = self.content else {
            return (0.0, 0.0, 0.0, 0.0);
        };
        let (width, height) = self.display_size();
        #[allow(clippy::cast_possible_truncation)]
        crate::quads::content_padding(
            (width as f32, height as f32),
            (
                content.left as f32,
                content.top as f32,
                content.right as f32,
                content.bottom as f32,
            ),
            self.pixel_ratio as f32,
        )
    }

    /// The icon's size in logical pixels: the padding removed and the ratio divided out.
    ///
    /// mbgl's `ImagePosition::displaySize`. Both steps matter — leaving the padding in draws
    /// every icon two pixels too large, and leaving the ratio in draws every retina icon at
    /// twice its size.
    #[must_use]
    pub fn display_size(&self) -> (f64, f64) {
        (
            f64::from(self.padded_rect.width - 2) / self.pixel_ratio,
            f64::from(self.padded_rect.height - 2) / self.pixel_ratio,
        )
    }
}

/// Where every icon of a style sits, after packing.
pub type Positions = BTreeMap<String, IconPosition>;

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

/// The largest value the rectangle fields accept, as mbgl's `getUInt16` bounds it.
pub const MAX_COORDINATE: u64 = 65_535;

/// One rectangle field, with mbgl's `getUInt16` semantics.
///
/// Not a refusal. mbgl reads `x`, `y`, `width` and `height` as unsigned sixteen-bit integers
/// with a **default of zero**, so a field that is absent, fractional, negative, or above 65535
/// is logged and becomes zero — and the entry carries on to the bounds check with that zero in
/// it. That matters in two opposite directions: a missing `width` becomes zero and is refused by
/// `width <= 0`, while a missing `x` becomes zero and is perfectly valid. `{"width": 32,
/// "height": 32}` with no origin is an icon at the sheet's top left, and mbgl's own
/// `SpriteParsingSimpleWidthHeight` says so.
fn coordinate(entry: &serde_json::Map<String, Value>, key: &str) -> u32 {
    let Some(value) = entry.get(key) else {
        return 0;
    };
    // `IsUint()` in rapidjson: a non-negative integer. A fractional or negative number is not
    // one, and falls to the default rather than being rounded towards anything.
    match value.as_u64() {
        Some(number) if number <= MAX_COORDINATE => {
            #[allow(clippy::cast_possible_truncation)]
            {
                number as u32
            }
        }
        _ => 0,
    }
}

/// One number-typed field, with mbgl's `getDouble` semantics: any number, or the default.
fn number(entry: &serde_json::Map<String, Value>, key: &str, default: f64) -> f64 {
    entry.get(key).and_then(Value::as_f64).unwrap_or(default)
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

/// How an icon's own pixels are stretched to fit the text laid into it.
///
/// mbgl's `style::TextFit`. An unrecognized string is `None` — the same as absent — rather than
/// a default, because the three named behaviours are genuinely different and guessing between
/// them resizes a shield the wrong way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextFit {
    /// Scale in both directions as needed.
    StretchOrShrink,
    /// Grow only.
    StretchOnly,
    /// Keep the aspect ratio.
    Proportional,
}

fn text_fit(entry: &serde_json::Map<String, Value>, key: &str) -> Option<TextFit> {
    match entry.get(key)?.as_str()? {
        "stretchOrShrink" => Some(TextFit::StretchOrShrink),
        "stretchOnly" => Some(TextFit::StretchOnly),
        "proportional" => Some(TextFit::Proportional),
        _ => None,
    }
}

/// Reads one entry, or `None` when it is unusable.
///
/// `sheet` is the image's size, and is what the rectangle is checked against — an entry naming a
/// rectangle outside the sheet would sample whatever is past the end of the buffer. `None` for
/// the sheet skips that check, which is what a caller reading the index before the image has
/// arrived has to do.
fn read(value: &Value, sheet: Option<(u32, u32)>) -> Option<Sprite> {
    let entry = value.as_object()?;

    let width = coordinate(entry, "width");
    let height = coordinate(entry, "height");
    let x = coordinate(entry, "x");
    let y = coordinate(entry, "y");
    let pixel_ratio = number(entry, "pixelRatio", 1.0);

    // mbgl's `createStyleImage` bounds, transcribed. The rectangle fields cannot be negative or
    // fractional by the time they arrive — `coordinate` has already turned those into zero — so
    // what is left to refuse is a zero or oversized dimension, a ratio outside its range, and a
    // rectangle that does not fit the sheet.
    if width == 0 || height == 0 {
        return None;
    }
    if u64::from(width) > MAX_DIMENSION || u64::from(height) > MAX_DIMENSION {
        return None;
    }
    // Written as a negated `>` rather than as `<=` on purpose, and not for style: `<= 0`
    // *accepts* a NaN, since every comparison against one is false, and would then hand an
    // unsigned cast a value with no defined result. Defensive rather than load-bearing today —
    // JSON has no NaN literal and a number out of range fails the whole document before it
    // reaches here — and the defensive form costs nothing while tidying it does not.
    #[allow(clippy::neg_cmp_op_on_partial_ord)]
    if !(pixel_ratio > 0.0) || pixel_ratio > MAX_PIXEL_RATIO {
        return None;
    }

    if let Some((sheet_width, sheet_height)) = sheet {
        if x >= sheet_width || y >= sheet_height {
            return None;
        }
        if x + width > sheet_width || y + height > sheet_height {
            return None;
        }
    }

    Some(Sprite {
        x,
        y,
        width,
        height,
        pixel_ratio,
        sdf: entry.get("sdf").and_then(Value::as_bool).unwrap_or(false),
        stretch_x: stretches(entry, "stretchX"),
        stretch_y: stretches(entry, "stretchY"),
        content: content(entry),
        text_fit_width: text_fit(entry, "textFitWidth"),
        text_fit_height: text_fit(entry, "textFitHeight"),
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
#[cfg(feature = "image")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sheet {
    /// Width in sheet pixels.
    pub width: u32,
    /// Height in sheet pixels.
    pub height: u32,
    /// `width * height * 4` bytes.
    pub pixels: Vec<u8>,
}

#[cfg(feature = "image")]
impl Sheet {
    /// Its size, in the shape the index's bounds check takes.
    #[must_use]
    pub const fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

/// The largest a decoded sheet may be, in bytes of RGBA.
///
/// Re-exported from the shared image decoder rather than restated: the bound is one number and
/// two copies of it drift. See [`tessella_source::image::MAX_IMAGE_BYTES`] for why it is checked
/// against the header rather than after the allocation.
#[cfg(feature = "image")]
pub use tessella_source::image::MAX_IMAGE_BYTES as MAX_SHEET_BYTES;

/// Why a sheet could not be decoded.
#[cfg(feature = "image")]
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
    /// The header asks for more memory than [`MAX_SHEET_BYTES`] allows.
    ///
    /// Refused before decoding rather than after: the point is not to hold the allocation.
    #[error("the sprite sheet header asks for {wanted} bytes, past the {MAX_SHEET_BYTES} ceiling")]
    TooLarge {
        /// What the header's dimensions would have cost, in bytes of RGBA.
        wanted: usize,
    },
}

/// Decodes a sprite sheet.
///
/// A thin adapter over [`tessella_source::image::decode`], which a raster tile goes through as
/// well: the bound against the header, the widening to RGBA and the format sniff are the same
/// questions for a sheet and for a tile, and answering them twice is how the two drift apart.
/// What stays here is the error type, because a caller loading a style wants to be told its
/// *sprite* did not decode.
///
/// A sheet that is a JPEG is accepted, and is a style bug rather than a decode one: JPEG has no
/// alpha, so every icon in it draws its background as an opaque rectangle. Refusing it here
/// would be this layer inventing a rule the spec does not state, and the picture says plainly
/// what happened.
///
/// # Errors
///
/// [`SheetError`] when the bytes are not an image this build reads, or when the image has no
/// area.
#[cfg(feature = "image")]
pub fn decode_sheet(body: &[u8]) -> Result<Sheet, SheetError> {
    use tessella_source::image::{Image, ImageError};

    match tessella_source::image::decode(body) {
        Ok(Image {
            width,
            height,
            pixels,
        }) => Ok(Sheet {
            width,
            height,
            pixels,
        }),
        Err(ImageError::TooLarge { wanted }) => Err(SheetError::TooLarge { wanted }),
        Err(ImageError::Unusable { width, height }) => Err(SheetError::Unusable { width, height }),
        Err(other) => Err(SheetError::Decode(other.to_string())),
    }
}

/// A style's sprite resource: the index and the sheet it points into.
///
/// The icon counterpart of [`Fonts`], and simpler in the way that matters — there is nothing to
/// pack. A glyph atlas is built by this process out of ranges that arrive separately; a sprite
/// sheet arrives already laid out, and the index is its map. So the store fetches two resources
/// once and holds them.
///
/// [`Fonts`]: crate::fonts::Fonts
#[cfg(feature = "image")]
#[derive(Debug)]
pub struct Sprites {
    base: String,
    pixel_ratio: f64,
    index: Index,
    sheet: Option<Sheet>,
    atlas: IconAtlas,
    positions: Positions,
    /// Whether the sheet has been uploaded since it last changed — §6.4's damage, for a
    /// resource that changes exactly once.
    dirty: bool,
}

#[cfg(feature = "image")]
impl Sprites {
    /// An empty store for a style's `sprite` base at a device pixel ratio.
    #[must_use]
    pub fn new(base: impl Into<String>, pixel_ratio: f64) -> Self {
        Self {
            base: base.into(),
            pixel_ratio,
            index: Index::new(),
            sheet: None,
            atlas: IconAtlas::new(ATLAS_SIZE, ATLAS_SIZE),
            positions: Positions::new(),
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

        // Cut every icon out of the sheet and pack it. mbgl does this in two places — the
        // parser copies each icon to an image of its own, and the atlas packs those with padding
        // — and the padding is what the icon quad's one-pixel border samples.
        let mut atlas = IconAtlas::new(ATLAS_SIZE, ATLAS_SIZE);
        let mut positions = Positions::new();
        for (name, sprite) in &parsed {
            if let Some(position) = atlas.add(&sheet, sprite) {
                positions.insert(name.clone(), position);
            }
        }

        self.index = parsed;
        self.sheet = Some(sheet);
        self.atlas = atlas;
        self.positions = positions;
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

    /// Where every icon sits in the atlas, which is what layout draws from.
    #[must_use]
    pub const fn positions(&self) -> &Positions {
        &self.positions
    }

    /// The atlas itself, for the texture upload.
    #[must_use]
    pub const fn atlas(&self) -> &IconAtlas {
        &self.atlas
    }

    /// The rectangles the atlas has changed since the last call.
    pub fn take_dirty_rects(&mut self) -> Vec<crate::atlas::Rect> {
        self.atlas.take_dirty()
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
#[cfg(feature = "image")]
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

/// The texture a style's icons are drawn from.
///
/// Four channels, unlike the glyph atlas: an icon is a picture and three of its channels carry
/// something. The packing is the same — `ShelfPack` with two pixels reserved on every side and
/// one of them reported, which is mbgl's `extraPadding` plus `ImagePosition::padding`.
///
/// Why this exists at all, given the sheet is already a laid-out image: mbgl does not upload the
/// sheet. `parseSprite` copies each icon out of it, and the atlas packs those copies with
/// padding between them. A sheet has no padding — icons in it are usually flush — so drawing
/// straight from it makes every icon quad's one-pixel border sample its neighbour.
#[cfg(feature = "image")]
#[derive(Debug)]
pub struct IconAtlas {
    pack: crate::atlas::ShelfPack,
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    next_key: u32,
    dirty: Vec<crate::atlas::Rect>,
}

#[cfg(feature = "image")]
impl IconAtlas {
    /// An empty atlas.
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            pack: crate::atlas::ShelfPack::new(width, height),
            pixels: vec![0; (width as usize) * (height as usize) * 4],
            width,
            height,
            next_key: 0,
            dirty: Vec::new(),
        }
    }

    /// Its dimensions, which is what `texsize_icon` carries.
    #[must_use]
    pub const fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The RGBA pixels.
    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// The rectangles changed since the last call — §6.4's damage.
    pub fn take_dirty(&mut self) -> Vec<crate::atlas::Rect> {
        core::mem::take(&mut self.dirty)
    }

    /// Cuts one icon out of `sheet` and packs it, returning where it landed.
    ///
    /// `None` when the atlas is full. The icon is skipped rather than the sheet refused, the way
    /// a glyph that does not fit is.
    pub fn add(&mut self, sheet: &Sheet, sprite: &Sprite) -> Option<IconPosition> {
        use crate::atlas::PADDING;

        let key = self.next_key;
        let slot = self
            .pack
            .pack(key, sprite.width + 2 * PADDING, sprite.height + 2 * PADDING)?;
        self.next_key += 1;

        // Copy the icon's pixels out of the sheet into the middle of its slot. Row by row,
        // because the two images have different widths and a single copy would shear it.
        for row in 0..sprite.height {
            let from = (((sprite.y + row) * sheet.width + sprite.x) as usize) * 4;
            let to = (((slot.y + PADDING + row) * self.width + slot.x + PADDING) as usize) * 4;
            let run = (sprite.width as usize) * 4;
            if from + run > sheet.pixels.len() || to + run > self.pixels.len() {
                // The index's bounds check should have refused this already; not trusting it
                // here is what keeps a bad sheet from being a read past the end.
                return None;
            }
            self.pixels[to..to + run].copy_from_slice(&sheet.pixels[from..from + run]);
        }

        self.dirty.push(slot);
        Some(IconPosition {
            // One pixel of the two is reported, which is the pixel the quad's border samples.
            padded_rect: crate::atlas::Rect {
                x: slot.x + 1,
                y: slot.y + 1,
                width: slot.width - 2,
                height: slot.height - 2,
            },
            pixel_ratio: sprite.pixel_ratio,
            sdf: sprite.sdf,
            content: sprite.content,
            text_fit_width: sprite.text_fit_width,
            text_fit_height: sprite.text_fit_height,
        })
    }
}
