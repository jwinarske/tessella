//! Decoding a raster tile or a sprite sheet, checked against maplibre-native's own expectations.
//!
//! The numbers are `test/util/image.test.cpp`'s, and the files are the ones it reads. That
//! matters more here than in most ports: an image decoder is easy to write so that it produces
//! *an* image for every input, and a wrong one is a picture rather than an error.

#![cfg(feature = "image")]

use tessella_source::image::{Format, ImageError, MAX_IMAGE_BYTES, decode, sniff};

const NO_PROFILE: &[u8] = include_bytes!("../../../tests/image-fixtures/no_profile.png");
const NO_PROFILE_ALPHA: &[u8] =
    include_bytes!("../../../tests/image-fixtures/no_profile_alpha.png");
const PROFILE: &[u8] = include_bytes!("../../../tests/image-fixtures/profile.png");
const PROFILE_ALPHA: &[u8] = include_bytes!("../../../tests/image-fixtures/profile_alpha.png");
const TILE_PNG: &[u8] = include_bytes!("../../../tests/image-fixtures/tile.png");
const TILE_JPEG: &[u8] = include_bytes!("../../../tests/image-fixtures/tile.jpeg");
const TILE_WEBP: &[u8] = include_bytes!("../../../tests/image-fixtures/tile.webp");

/// mbgl `Image.PNGReadNoProfile` and `Image.PNGReadProfile`.
///
/// Both files hold the same pixel and one of them carries an ICC profile. mbgl expects the same
/// bytes from each, which is the assertion worth having: a decoder that honoured the profile
/// would colour-manage one tile of a basemap and not its neighbours, and the seam between them
/// is a bug report about the *tile server*.
#[test]
fn a_colour_profile_does_not_change_the_pixel() {
    for (name, body) in [("no_profile", NO_PROFILE), ("profile", PROFILE)] {
        let image = decode(body).expect("the fixture decodes");
        assert_eq!(image.size(), (1, 1), "{name}");
        assert_eq!(image.pixels, vec![128, 0, 0, 255], "{name}");
    }
}

/// mbgl `Image.PNGReadNoProfileAlpha` and `Image.PNGReadProfileAlpha`, and what says the decode
/// premultiplies.
///
/// The files hold half-red at half alpha. mbgl expects `64, 0, 0, 128` — the colour multiplied
/// by its alpha — because everything downstream blends premultiplied. A decoder returning the
/// straight `128` draws a bright fringe wherever artwork fades out, which is invisible on the
/// opaque sprites that are most of a sheet.
#[test]
fn a_translucent_pixel_comes_back_premultiplied() {
    for (name, body) in [
        ("no_profile_alpha", NO_PROFILE_ALPHA),
        ("profile_alpha", PROFILE_ALPHA),
    ] {
        let image = decode(body).expect("the fixture decodes");
        assert_eq!(image.size(), (1, 1), "{name}");
        assert_eq!(image.pixels, vec![64, 0, 0, 128], "{name}");
    }
}

/// mbgl's rounding, which is round-to-nearest and not a truncation.
///
/// `(c * a + 127) / 255`, not `c * a / 255`. The two differ by one over most of the range, and
/// one level of red is not something anyone sees — but the sprite atlas is compared byte for
/// byte against the oracle, so a truncating premultiply is a diff on nearly every translucent
/// pixel of every sheet.
#[test]
fn the_premultiply_rounds_the_way_the_oracle_does() {
    // A 1x1 RGBA PNG holding (3, 1, 253) at alpha 128 — three channels chosen so that all three
    // land where the two roundings disagree. Truncating gives 1, 0, 126.
    let png = one_pixel_rgba(3, 1, 253, 128);
    let image = decode(&png).expect("the hand-built PNG decodes");
    assert_eq!(image.pixels, vec![2, 1, 127, 128]);
}

/// An opaque pixel is left exactly as it was.
///
/// The branch premultiplication is easy to get wrong in the direction nobody notices: an
/// alpha-255 pixel must come through untouched, and `(c * 255 + 127) / 255` is `c` only because
/// of the rounding term.
#[test]
fn an_opaque_pixel_is_untouched() {
    let png = one_pixel_rgba(1, 127, 254, 255);
    let image = decode(&png).expect("the hand-built PNG decodes");
    assert_eq!(image.pixels, vec![1, 127, 254, 255]);
}

/// A fully transparent pixel loses its colour, which is what premultiplied means.
#[test]
fn a_transparent_pixel_keeps_no_colour() {
    let png = one_pixel_rgba(255, 255, 255, 0);
    let image = decode(&png).expect("the hand-built PNG decodes");
    assert_eq!(image.pixels, vec![0, 0, 0, 0]);
}

/// mbgl `Image.PNGTile` and `Image.JPEGTile`: one tile, two encodings, the same size.
///
/// The JPEG is the case that matters for a basemap. Satellite imagery is photographic and every
/// commercial source serves it as JPEG, so a build that reads only PNG draws no satellite at
/// all — and the failure is silent, because the tile fetches successfully and then does not
/// decode.
#[test]
fn a_raster_tile_decodes_from_either_encoding() {
    for (name, body) in [("tile.png", TILE_PNG), ("tile.jpeg", TILE_JPEG)] {
        let image = decode(body).expect("the fixture decodes");
        assert_eq!(image.size(), (256, 256), "{name}");
        assert_eq!(image.pixels.len(), 256 * 256 * 4, "{name}");
    }
}

/// A JPEG has no alpha, and comes back opaque rather than blank.
///
/// The output colourspace is requested as RGBA, so the alpha channel is one the decoder invents.
/// Inventing it as zero would make every satellite tile fully transparent — a black map with a
/// working fetch, which is the worst kind of silence.
#[test]
fn a_jpeg_is_opaque_everywhere() {
    let image = decode(TILE_JPEG).expect("the fixture decodes");
    let pixels = image.pixels.as_chunks::<4>().0;
    assert!(
        pixels.iter().all(|pixel| pixel[3] == 255),
        "a jpeg decoded with a transparent pixel in it"
    );
    // And it is a photograph rather than one flat colour, which is what says the decode did
    // something: a decoder returning a zeroed buffer of the right size passes every assertion
    // above it.
    let first = pixels[0];
    assert!(
        pixels.iter().any(|pixel| *pixel != first),
        "the whole tile decoded to one colour"
    );
}

/// The format comes from the bytes, not from the name.
///
/// A tile URL template ends in `.png` for plenty of sources that serve JPEG behind it, and a
/// `Content-Type` can be absent, wrong, or `application/octet-stream`. The first eight bytes
/// cannot be any of those things.
#[test]
fn the_format_is_sniffed_from_the_signature() {
    assert_eq!(sniff(TILE_PNG), Some(Format::Png));
    assert_eq!(sniff(TILE_JPEG), Some(Format::Jpeg));
    assert_eq!(sniff(TILE_WEBP), Some(Format::Webp));
    assert_eq!(sniff(b""), None);
    assert_eq!(
        sniff(b"<!DOCTYPE html>"),
        None,
        "an error page is not a tile"
    );

    // PNG's signature carries a CRLF pair and a lone LF precisely so a transfer that mangles
    // line endings breaks the signature instead of the image. A decoder matching only the first
    // four bytes would accept the mangled file and then fail somewhere less legible.
    let mut mangled = TILE_PNG.to_vec();
    mangled[4] = b'\n';
    assert_eq!(sniff(&mangled), None);

    // `RIFF` alone is a container tag shared with WAV, AVI and a dozen other formats. Matching
    // on it would hand a sound file to the image decoder and report a decode failure where "not
    // an image" is the truthful answer.
    let mut wav = b"RIFF\x24\x08\x00\x00WAVEfmt ".to_vec();
    wav.extend_from_slice(&[0; 16]);
    assert_eq!(sniff(&wav), None, "a wav is not a webp");

    // And a RIFF header that stops before the form type is not enough to claim either.
    assert_eq!(sniff(b"RIFF\x24\x08\x00\x00WEB"), None);
}

/// mbgl `Image.WebPTile`: the third encoding of the same tile, at the same size.
///
/// The one a URL is least likely to admit to. A `.png` template served as WebP behind a
/// content-negotiating CDN is an ordinary arrangement, which is why the format is sniffed rather
/// than trusted — and why this fixture matters more than its rarity suggests.
#[cfg(feature = "webp")]
#[test]
fn a_webp_tile_decodes() {
    let image = decode(TILE_WEBP).expect("the fixture decodes");
    assert_eq!(image.size(), (256, 256));
    assert_eq!(image.pixels.len(), 256 * 256 * 4);

    // The fixture has no alpha chunk, so the decoder answers three channels and the widening
    // supplies the fourth. Zero-filling it would make every WebP tile fully transparent — a
    // blank map with a working fetch.
    assert!(
        image.pixels.as_chunks::<4>().0.iter().all(|p| p[3] == 255),
        "a webp decoded with a transparent pixel in it"
    );

    // And it decoded a picture rather than a zeroed buffer of the right size.
    let pixels = image.pixels.as_chunks::<4>().0;
    let first = pixels[0];
    assert!(
        pixels.iter().any(|pixel| *pixel != first),
        "the whole tile decoded to one colour"
    );
}

/// The PNG and the WebP are the same picture, which is what checks the VP8 path.
///
/// mbgl's `tile.webp` is a `VP8 ` chunk — *lossy*, in an extended container with an EXIF chunk
/// beside it — and it still agrees with `tile.png` to within a tenth of a level on every channel
/// mean. That is a much stronger statement than the size check `Image.WebPTile` makes: a decoder
/// that swapped the chroma planes, or upsampled them wrongly, or read the extended header's
/// dimensions instead of the frame's, produces something of exactly the right size and visibly
/// the wrong colour. The mean is what notices.
///
/// `tile.jpeg` is deliberately not in this comparison. It is a different photograph — its red
/// channel means 117.6 against the PNG's 63.9 — and mbgl's own tests never claim otherwise; they
/// assert the size of each file and nothing more. Reading the three as one picture in three
/// encodings is the mistake this paragraph exists to stop.
#[cfg(feature = "webp")]
#[test]
fn the_png_and_the_webp_are_the_same_picture() {
    let reference = decode(TILE_PNG).expect("png");
    let webp = decode(TILE_WEBP).expect("webp");
    assert_eq!(webp.size(), reference.size());

    let mean = |image: &tessella_source::image::Image, channel: usize| -> f64 {
        let pixels = image.pixels.as_chunks::<4>().0;
        pixels
            .iter()
            .map(|pixel| f64::from(pixel[channel]))
            .sum::<f64>()
            / pixels.len() as f64
    };

    for channel in 0..3 {
        let (want, got) = (mean(&reference, channel), mean(&webp, channel));
        assert!(
            (want - got).abs() < 0.5,
            "channel {channel}: the webp means {got} against the png's {want}"
        );
    }
}

/// A build without the WebP decoder says so, rather than calling the bytes corrupt.
///
/// The distinction is the point, and it is why `Unsupported` exists beside `Decode`: one says
/// this binary was not built to read the format, which is a build answer, and the other says the
/// body is broken, which is a retry. Reporting the second for the first sends the next person to
/// look at the tile server.
#[cfg(not(feature = "webp"))]
#[test]
fn a_webp_tile_is_refused_by_name() {
    use tessella_source::image::Format;

    assert_eq!(sniff(TILE_WEBP), Some(Format::Webp), "it is still a webp");
    assert_eq!(
        decode(TILE_WEBP),
        Err(ImageError::Unsupported(Format::Webp))
    );
}

/// A truncated body fails rather than producing an image with a garbage tail.
#[test]
fn a_truncated_image_does_not_decode() {
    let half = &TILE_PNG[..TILE_PNG.len() / 2];
    assert!(decode(half).is_err(), "half a png decoded");

    let half = &TILE_JPEG[..TILE_JPEG.len() / 2];
    match decode(half) {
        Err(_) => {}
        // A JPEG is a stream of independently decodable scans, so a truncated one can legally
        // produce a partial image. What must not happen is a buffer shorter than the dimensions
        // it claims, which is what would read past the end downstream.
        Ok(image) => assert_eq!(
            image.pixels.len(),
            (image.width as usize) * (image.height as usize) * 4,
            "a truncated jpeg produced a short buffer"
        ),
    }
}

/// A header claiming more than the ceiling is refused before anything is allocated.
///
/// The bound is against the *header*, so the cost of a bomb is a parse of a few dozen bytes
/// rather than the gigabytes it asked for. 8192 square is chosen deliberately: `zune-png` has a
/// 16384-square limit of its own, and a test using a larger figure would be caught by that
/// instead and would pass while this bound did nothing.
#[test]
fn a_decompression_bomb_is_refused_from_its_header() {
    let bomb = png_header(8192, 8192);
    match decode(&bomb) {
        Err(ImageError::TooLarge { wanted }) => {
            assert_eq!(wanted, 8192 * 8192 * 4);
            assert!(wanted > MAX_IMAGE_BYTES);
        }
        other => panic!("a bomb was not refused from its header: {other:?}"),
    }
}

/// A zero-dimension image is refused rather than returned as an empty one.
///
/// Everything downstream divides by a dimension somewhere — an atlas rectangle, a texture
/// coordinate — so a zero is a division several stages away from the file that caused it.
#[test]
fn an_empty_image_is_unusable() {
    // PNG's own spec forbids a zero dimension, so the decoder refuses it first; the assertion
    // is that it is refused, not which of the two layers refused it.
    assert!(decode(&png_header(0, 16)).is_err());
    assert!(decode(&png_header(16, 0)).is_err());
}

/// Bytes that are not an image at all.
#[test]
fn arbitrary_bytes_are_not_an_image() {
    assert_eq!(decode(b"not an image"), Err(ImageError::Unrecognized));
    assert_eq!(decode(&[]), Err(ImageError::Unrecognized));

    // A valid signature over nothing is a *decode* failure, not an unrecognized format.
    let mut stub = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    stub.extend_from_slice(&[0; 8]);
    assert!(matches!(decode(&stub), Err(ImageError::Decode(_))));
}

/// A PNG with the given dimensions and an empty image behind them.
///
/// Enough for the header bound to read, which is the point: the ceiling is checked before any
/// pixel is decoded, so a fixture that never carries pixels still exercises it.
fn png_header(width: u32, height: u32) -> Vec<u8> {
    png_with(width, height, &[])
}

/// A PNG with the given dimensions and the given zlib stream as its one `IDAT`.
fn png_with(width: u32, height: u32, idat: &[u8]) -> Vec<u8> {
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    // Eight-bit RGBA, no interlace.
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);

    let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    push_chunk(&mut png, b"IHDR", &ihdr);
    push_chunk(&mut png, b"IDAT", idat);
    push_chunk(&mut png, b"IEND", &[]);
    png
}

/// A one-pixel eight-bit RGBA PNG holding exactly these channels.
///
/// Hand-built rather than vendored so the premultiply can be checked at chosen values: the
/// rounding differs from a truncation by one only over part of the range, and the oracle's
/// fixtures happen to sit where the two agree.
fn one_pixel_rgba(r: u8, g: u8, b: u8, a: u8) -> Vec<u8> {
    // One scanline: a filter byte then the pixel, in a single stored (uncompressed) deflate
    // block. Stored blocks are what make a fixture like this hand-writable at all — the
    // alternative is pulling in an encoder in order to test a decoder.
    let raw = [0u8, r, g, b, a];
    let mut zlib = vec![0x78, 0x01];
    #[allow(clippy::cast_possible_truncation)]
    let len = raw.len() as u16;
    zlib.push(0x01);
    zlib.extend_from_slice(&len.to_le_bytes());
    zlib.extend_from_slice(&(!len).to_le_bytes());
    zlib.extend_from_slice(&raw);
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());
    png_with(1, 1, &zlib)
}

fn push_chunk(png: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    #[allow(clippy::cast_possible_truncation)]
    png.extend_from_slice(&(body.len() as u32).to_be_bytes());
    let mut chunk = kind.to_vec();
    chunk.extend_from_slice(body);
    png.extend_from_slice(&chunk);
    png.extend_from_slice(&crc32(&chunk).to_be_bytes());
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for byte in bytes {
        a = (a + u32::from(*byte)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}
