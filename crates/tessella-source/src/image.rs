//! Decoding a raster tile or a sprite sheet into RGBA.
//!
//! # Two formats, because a basemap is both
//!
//! Sprite sheets are PNG — they have alpha, and an icon without alpha is a rectangle of
//! background around a picture. Satellite imagery is JPEG, and overwhelmingly so: a photographic
//! tile stored losslessly is several times the bytes for a difference nobody looking at a map
//! can see, so every commercial imagery source serves JPEG and a build that reads only PNG can
//! draw no satellite basemap at all. Terrain shading and label-free overlays go back to PNG for
//! the alpha. So the two are not alternatives to choose between; a real style uses both, often
//! from the same source, and which one a tile is cannot be known before it arrives.
//!
//! The format is therefore sniffed from the bytes rather than taken from the URL or a
//! `Content-Type`. A tile URL template ends in `.png` for many sources that serve JPEG behind
//! it, and a header can be absent, wrong, or `application/octet-stream`; the first eight bytes
//! cannot be any of those things.
//!
//! # Everything widens to RGBA
//!
//! A PNG may be greyscale, palletted, RGB or RGBA and a JPEG greyscale or YCbCr, and what is
//! downstream — an atlas rectangle, a texture upload, a shader sampling a quad — counts in
//! pixels. A decoder returning the file's own channel count would make every offset past this
//! point depend on how the file happened to be encoded, which is a defect that appears only for
//! the one source that ships greyscale.
//!
//! # The bound is checked against the header
//!
//! Every byte here came off a network from a party that is not trusted. The module is
//! `forbid(unsafe_code)` and so are both decoders, so the risk is not memory corruption; it is
//! *allocation*. An image states its dimensions in its header and the decoder allocates from
//! them, so a few hundred bytes can ask for gigabytes — the classic decompression bomb, and on a
//! device-class target an out-of-memory rather than a slow frame. The dimensions are read and
//! refused before any pixel is decoded, so a bomb costs a header parse rather than the
//! allocation it asked for.

use alloc::string::String;
use alloc::vec::Vec;

/// A decoded image, always eight-bit RGBA.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height * 4` bytes.
    pub pixels: Vec<u8>,
}

impl Image {
    /// Its size, in the shape a bounds check takes.
    #[must_use]
    pub const fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

/// The largest a decoded image may be, in bytes of RGBA.
///
/// Sixty-four mebibytes is four thousand pixels square. Generous against what these actually
/// are — a raster tile is 256 or 512 a side, a sprite sheet a few hundred to a couple of
/// thousand — and small enough that refusing costs less than the allocation would.
///
/// Stated here rather than left to the decoders' own defaults: `zune-png` refuses beyond 16384
/// square, which is a gibibyte of RGBA and far past anything this reads, and a version that
/// raised it would remove the bound with nothing saying so.
pub const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;

/// Which decoder read the bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// PNG.
    Png,
    /// JPEG.
    Jpeg,
}

/// Why an image could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ImageError {
    /// The bytes begin with neither a PNG signature nor a JPEG one.
    ///
    /// Reported separately from a decode failure because the two want different fixes: an
    /// unrecognized format is usually a source serving something else entirely — an error page,
    /// a WebP tile — while a decode failure is a corrupt or truncated body.
    #[error("the bytes are neither a PNG nor a JPEG")]
    Unrecognized,
    /// The decoder refused the bytes.
    #[error("the image did not decode: {0}")]
    Decode(String),
    /// The image decoded to a size or colour type nothing can index.
    #[error("the image is {width}x{height}, which is not usable")]
    Unusable {
        /// Decoded width.
        width: u32,
        /// Decoded height.
        height: u32,
    },
    /// The build has no decoder for this format.
    ///
    /// Both decoders sit behind an off-by-default feature (DR-12), so a build that never draws a
    /// raster tile and never loads a sprite does not carry them. Reported rather than gated at
    /// the call site: a style asking for something this binary was not built to read is a
    /// configuration answer, and a caller wrapped in `cfg` would have to duplicate its own
    /// planning to say the same thing.
    #[error("this build was compiled without a {0:?} decoder")]
    Unsupported(Format),
    /// The header asks for more memory than [`MAX_IMAGE_BYTES`] allows.
    ///
    /// Refused before decoding rather than after: the point is not to hold the allocation.
    #[error("the image header asks for {wanted} bytes, past the {MAX_IMAGE_BYTES} ceiling")]
    TooLarge {
        /// What the header's dimensions would have cost, in bytes of RGBA.
        wanted: usize,
    },
}

/// The format the bytes claim to be, from their signature alone.
///
/// PNG's is eight bytes and includes a `\r\n` pair and a lone `\n` precisely so that a transfer
/// that mangles line endings corrupts the signature rather than silently corrupting the image.
/// JPEG's is the two-byte start-of-image marker.
#[must_use]
pub fn sniff(body: &[u8]) -> Option<Format> {
    const PNG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    if body.starts_with(&PNG) {
        return Some(Format::Png);
    }
    if body.starts_with(&[0xff, 0xd8]) {
        return Some(Format::Jpeg);
    }
    None
}

/// Decodes a PNG or a JPEG to RGBA.
///
/// # Errors
///
/// [`ImageError`] when the bytes are neither format, when the decoder refuses them, when the
/// header's dimensions exceed [`MAX_IMAGE_BYTES`], or when the image has no area.
pub fn decode(body: &[u8]) -> Result<Image, ImageError> {
    match sniff(body) {
        Some(Format::Png) => decode_png(body),
        Some(Format::Jpeg) => decode_jpeg(body),
        None => Err(ImageError::Unrecognized),
    }
}

#[cfg(not(feature = "image"))]
fn decode_png(_body: &[u8]) -> Result<Image, ImageError> {
    Err(ImageError::Unsupported(Format::Png))
}

#[cfg(not(feature = "image"))]
fn decode_jpeg(_body: &[u8]) -> Result<Image, ImageError> {
    Err(ImageError::Unsupported(Format::Jpeg))
}

/// Refuses dimensions that would allocate past the ceiling.
#[cfg(feature = "image")]
///
/// `saturating_mul` rather than checked: an overflow and a value past the ceiling both end in
/// the same refusal, and saturating says so in one expression instead of two.
fn afford(width: usize, height: usize) -> Result<(), ImageError> {
    let wanted = width.saturating_mul(height).saturating_mul(4);
    if wanted > MAX_IMAGE_BYTES {
        return Err(ImageError::TooLarge { wanted });
    }
    Ok(())
}

#[cfg(feature = "image")]
fn decode_png(body: &[u8]) -> Result<Image, ImageError> {
    use zune_png::PngDecoder;
    use zune_png::zune_core::bytestream::ZCursor;
    use zune_png::zune_core::options::DecoderOptions;

    // Eight-bit RGBA outright rather than a conversion afterwards. A sixteen-bit image decoded
    // at its own depth is twice the bytes for the same pixels, and the mismatch shows up as
    // everything sampling half of its neighbour.
    let options = DecoderOptions::default()
        .png_set_strip_to_8bit(true)
        .png_set_add_alpha_channel(true);
    let mut decoder = PngDecoder::new_with_options(ZCursor::new(body), options);

    // The header first, and *only* the header.
    decoder
        .decode_headers()
        .map_err(|error| ImageError::Decode(alloc::format!("{error:?}")))?;
    let (width, height) = decoder
        .dimensions()
        .ok_or_else(|| ImageError::Decode("the header carries no dimensions".into()))?;
    afford(width, height)?;

    let pixels = decoder
        .decode_raw()
        .map_err(|error| ImageError::Decode(alloc::format!("{error:?}")))?;
    let colorspace = decoder
        .colorspace()
        .ok_or_else(|| ImageError::Decode("the header carries no colorspace".into()))?;

    #[allow(clippy::cast_possible_truncation)]
    let (width, height) = (width as u32, height as u32);
    finish(pixels, channels(colorspace)?, width, height)
}

#[cfg(feature = "image")]
fn decode_jpeg(body: &[u8]) -> Result<Image, ImageError> {
    use zune_jpeg::JpegDecoder;
    use zune_jpeg::zune_core::bytestream::ZCursor;
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;

    // RGBA out of the decoder rather than out of a widening pass. A JPEG has no alpha, so the
    // channel it gains is a constant, and asking for it here lets the decoder's own SIMD write
    // it instead of a scalar loop copying every pixel a second time (§12.2).
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA);
    let mut decoder = JpegDecoder::new_with_options(ZCursor::new(body), options);

    decoder
        .decode_headers()
        .map_err(|error| ImageError::Decode(alloc::format!("{error:?}")))?;
    let (width, height) = decoder
        .dimensions()
        .ok_or_else(|| ImageError::Decode("the header carries no dimensions".into()))?;
    afford(width, height)?;

    let pixels = decoder
        .decode()
        .map_err(|error| ImageError::Decode(alloc::format!("{error:?}")))?;

    // A greyscale JPEG comes back as one channel however the output colourspace was set —
    // `jpeg_set_out_colorspace` is honoured for three-component images and the decoder keeps
    // Luma otherwise — so the widening below is not dead code for the RGBA request above.
    let out = decoder
        .output_colorspace()
        .ok_or_else(|| ImageError::Decode("the decoder reports no colourspace".into()))?;
    // Bounded by `afford` above, so the cast cannot lose anything: a dimension whose product
    // fits 64 MiB of RGBA fits a u32 several times over.
    #[allow(clippy::cast_possible_truncation)]
    let (width, height) = (width as u32, height as u32);
    finish(pixels, channels(out)?, width, height)
}

/// How many bytes a pixel occupies in a colour space, or a refusal.
#[cfg(feature = "image")]
fn channels(space: zune_png::zune_core::colorspace::ColorSpace) -> Result<usize, ImageError> {
    use zune_png::zune_core::colorspace::ColorSpace;
    match space {
        ColorSpace::RGBA => Ok(4),
        ColorSpace::RGB => Ok(3),
        ColorSpace::LumaA => Ok(2),
        ColorSpace::Luma => Ok(1),
        other => Err(ImageError::Decode(alloc::format!(
            "{other:?} is not a colour type a map image uses"
        ))),
    }
}

/// Widens a decoded buffer to RGBA, premultiplies it, and checks it against its own dimensions.
#[cfg(feature = "image")]
///
/// The length check is the one that matters and it is the reason this is not written as a
/// wrapping `chunks_exact`: a buffer shorter than its own header describes is a truncated
/// download, and a widening that took what was there would produce an image whose bottom rows
/// are whatever the allocator last held.
fn finish(pixels: Vec<u8>, channels: usize, width: u32, height: u32) -> Result<Image, ImageError> {
    if width == 0 || height == 0 {
        return Err(ImageError::Unusable { width, height });
    }
    let count = (width as usize) * (height as usize);
    if pixels.len() < count * channels {
        return Err(ImageError::Unusable { width, height });
    }

    // Already RGBA: take the buffer rather than copying it. A raster tile is a quarter of a
    // megabyte and this runs per tile per frame's worth of cover, so the copy is not free
    // (§11.5).
    let mut pixels = if channels == 4 {
        let mut pixels = pixels;
        pixels.truncate(count * 4);
        pixels
    } else {
        let mut out = Vec::with_capacity(count * 4);
        for source in pixels.chunks_exact(channels).take(count) {
            match channels {
                3 => out.extend_from_slice(&[source[0], source[1], source[2], 255]),
                2 => out.extend_from_slice(&[source[0], source[0], source[0], source[1]]),
                _ => out.extend_from_slice(&[source[0], source[0], source[0], 255]),
            }
        }
        out
    };

    // Only a source that carried alpha can have anything to premultiply — a widened RGB or
    // greyscale image is opaque everywhere, so the pass would read and write every byte to leave
    // it as it was.
    if channels == 4 || channels == 2 {
        premultiply(&mut pixels);
    }

    Ok(Image {
        width,
        height,
        pixels,
    })
}

/// Multiplies each colour channel by its alpha, in place.
#[cfg(feature = "image")]
///
/// mbgl's `util::premultiply`, and its rounding: `(c * a + 127) / 255` rather than `c * a / 255`,
/// which is a round-to-nearest instead of a truncation and differs by one over most of the range.
///
/// # Why the decode is where this belongs
///
/// Everything downstream blends premultiplied — style colours are stored that way, and the
/// shaders are mbgl's — so an image that is not is the odd one out. Left straight, a translucent
/// icon's anti-aliased edge blends its own colour at full strength against the background and
/// draws a bright fringe around every marker on the map: a defect that is invisible on opaque
/// sprites, which is most of them, and appears only where the artwork fades out.
///
/// It cannot be deferred to the upload either. The atlas cuts icons out of the sheet and packs
/// them, and the icon quad's border samples the padding beside them; premultiplying after that
/// would still be correct, but premultiplying *before* any of it means every consumer of these
/// pixels — atlas, texture, raster quad — is looking at the same space.
fn premultiply(pixels: &mut [u8]) {
    for pixel in pixels.as_chunks_mut::<4>().0 {
        let alpha = u32::from(pixel[3]);
        if alpha == 255 {
            continue;
        }
        for channel in &mut pixel[..3] {
            #[allow(clippy::cast_possible_truncation)]
            {
                *channel = ((u32::from(*channel) * alpha + 127) / 255) as u8;
            }
        }
    }
}
