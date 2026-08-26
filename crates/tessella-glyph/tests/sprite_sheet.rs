//! Decoding a sprite sheet, and the widening every colour type goes through.
//!
//! The rectangles the index hands out are in *pixels*, so a decoder that returned the file's own
//! channel count would make every offset downstream depend on how the sheet happened to be
//! encoded — a greyscale sheet and an RGBA one with identical rectangles would sample different
//! things. Everything is widened to RGBA for that reason, and each colour type is checked.

#![cfg(feature = "png")]

use tessella_glyph::sprite::{self, SheetError};

/// Encodes a PNG with stored (uncompressed) zlib blocks.
///
/// Hand-written rather than pulled from a crate: the decoder under test is the dependency being
/// evaluated, and encoding with a second image crate to test the first would make the test pass
/// or fail on either of them. zlib permits stored blocks, so a valid PNG needs only a CRC and an
/// Adler sum.
fn encode(width: u32, height: u32, color_type: u8, samples: &[u8]) -> Vec<u8> {
    fn crc32(bytes: &[u8]) -> u32 {
        let mut table = [0u32; 256];
        for (index, entry) in table.iter_mut().enumerate() {
            let mut value = index as u32;
            for _ in 0..8 {
                value = if value & 1 == 1 {
                    0xedb8_8320 ^ (value >> 1)
                } else {
                    value >> 1
                };
            }
            *entry = value;
        }
        let mut crc = 0xffff_ffffu32;
        for byte in bytes {
            crc = table[((crc ^ u32::from(*byte)) & 0xff) as usize] ^ (crc >> 8);
        }
        crc ^ 0xffff_ffff
    }

    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        let mut framed = kind.to_vec();
        framed.extend_from_slice(body);
        out.extend_from_slice(&framed);
        out.extend_from_slice(&crc32(&framed).to_be_bytes());
    }

    let channels = match color_type {
        0 => 1, // greyscale
        2 => 3, // RGB
        4 => 2, // greyscale + alpha
        6 => 4, // RGBA
        other => panic!("colour type {other} is not one this encoder writes"),
    };
    let stride = width as usize * channels;

    // Each scanline is prefixed with its filter type, which is zero: none.
    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    for row in 0..height as usize {
        raw.push(0);
        raw.extend_from_slice(&samples[row * stride..(row + 1) * stride]);
    }

    let mut zlib = vec![0x78, 0x01];
    for (index, block) in raw.chunks(65_535).enumerate() {
        zlib.push(u8::from((index + 1) * 65_535 >= raw.len()));
        let len = block.len() as u16;
        zlib.extend_from_slice(&len.to_le_bytes());
        zlib.extend_from_slice(&(!len).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for byte in &raw {
        a = (a + u32::from(*byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());

    let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut header = Vec::new();
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, color_type, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &header);
    chunk(&mut png, b"IDAT", &zlib);
    chunk(&mut png, b"IEND", &[]);
    png
}

/// An RGBA sheet decodes to its own bytes.
#[test]
fn an_rgba_sheet_decodes_unchanged() {
    let samples: Vec<u8> = (0..2 * 2 * 4).map(|index| index as u8).collect();
    let sheet = sprite::decode_sheet(&encode(2, 2, 6, &samples)).expect("decodes");

    assert_eq!(sheet.size(), (2, 2));
    assert_eq!(sheet.pixels.len(), 2 * 2 * 4);
    assert_eq!(sheet.pixels, samples);
}

/// An RGB sheet gains an opaque alpha rather than a transparent one.
///
/// Zero-filling the channel would make every icon of an RGB sheet invisible, which reads as a
/// missing sheet rather than as a decode bug.
#[test]
fn an_rgb_sheet_gains_opaque_alpha() {
    let samples = [10, 20, 30, 40, 50, 60];
    let sheet = sprite::decode_sheet(&encode(2, 1, 2, &samples)).expect("decodes");
    assert_eq!(sheet.pixels, vec![10, 20, 30, 255, 40, 50, 60, 255]);
}

/// A greyscale sheet is broadcast across the colour channels.
///
/// Not left in the red channel: the rectangle is the same either way, so an icon would decode to
/// the right place in the right size and draw red.
#[test]
fn a_greyscale_sheet_broadcasts_across_the_channels() {
    let sheet = sprite::decode_sheet(&encode(2, 1, 0, &[64, 200])).expect("decodes");
    assert_eq!(sheet.pixels, vec![64, 64, 64, 255, 200, 200, 200, 255]);
}

/// Greyscale with alpha keeps the alpha and broadcasts the rest.
#[test]
fn greyscale_with_alpha_keeps_its_alpha() {
    let sheet = sprite::decode_sheet(&encode(2, 1, 4, &[64, 128, 200, 0])).expect("decodes");
    assert_eq!(sheet.pixels, vec![64, 64, 64, 128, 200, 200, 200, 0]);
}

/// The decoded size is what the index's bounds check runs against.
///
/// The two halves of the sprite resource meeting: the index refuses a rectangle that runs off
/// the sheet, and this is where the sheet's size comes from. A decoder reporting the wrong size
/// would either refuse valid icons or admit ones that sample past the end.
#[test]
fn the_decoded_size_bounds_the_index() {
    let samples = vec![0u8; 32 * 32 * 4];
    let sheet = sprite::decode_sheet(&encode(32, 32, 6, &samples)).expect("decodes");

    let index = sprite::parse(
        br#"{"inside": {"x": 0,  "y": 0, "width": 16, "height": 16},
             "past":   {"x": 24, "y": 0, "width": 16, "height": 16}}"#,
        Some(sheet.size()),
    )
    .expect("the index parses");

    assert!(index.contains_key("inside"));
    assert!(!index.contains_key("past"), "24 + 16 runs past 32");
}

/// A body that is not a PNG is an error rather than an empty sheet.
#[test]
fn a_body_that_is_not_a_png_is_an_error() {
    assert!(matches!(
        sprite::decode_sheet(b"<html>404</html>"),
        Err(SheetError::Decode(_))
    ));
    assert!(matches!(
        sprite::decode_sheet(&[]),
        Err(SheetError::Decode(_))
    ));

    // A valid header with a truncated body is the case that would read past the end if the
    // decoder were trusted rather than checked.
    let mut truncated = encode(4, 4, 6, &[0u8; 4 * 4 * 4]);
    truncated.truncate(40);
    assert!(sprite::decode_sheet(&truncated).is_err());
}

/// A sheet large enough to hold a real style's icons decodes.
///
/// Small fixtures pass on a decoder that mishandles multi-block zlib streams or wide scanlines,
/// which is most of what a real sheet is.
#[test]
fn a_full_size_sheet_decodes() {
    let samples: Vec<u8> = (0..512usize * 512 * 4).map(|index| index as u8).collect();
    let sheet = sprite::decode_sheet(&encode(512, 512, 6, &samples)).expect("decodes");
    assert_eq!(sheet.size(), (512, 512));
    assert_eq!(sheet.pixels.len(), 512 * 512 * 4);
    assert_eq!(sheet.pixels, samples);
}
