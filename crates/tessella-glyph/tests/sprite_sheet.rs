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

/// The store fetches both halves and holds them.
mod store {
    use super::encode;

    use std::cell::RefCell;

    use tessella_glyph::sprite::{LoadError, Sprites};
    use tessella_storage::source::{FetchError, FileSource, Response};

    /// An origin serving one sprite resource, counting what was asked of it.
    struct Origin {
        asked: RefCell<Vec<String>>,
        sheet: Vec<u8>,
        index: Vec<u8>,
    }

    impl Origin {
        fn new(index: &str) -> Self {
            Self {
                asked: RefCell::new(Vec::new()),
                sheet: encode(32, 32, 6, &[0u8; 32 * 32 * 4]),
                index: index.as_bytes().to_vec(),
            }
        }

        fn asked(&self) -> Vec<String> {
            self.asked.borrow().clone()
        }
    }

    impl FileSource for Origin {
        fn fetch(&self, url: &str) -> Result<Response, FetchError> {
            self.asked.borrow_mut().push(url.to_string());
            let body = if url.ends_with(".png") {
                self.sheet.clone()
            } else {
                self.index.clone()
            };
            Ok(Response {
                status: 200,
                body,
                ..Response::default()
            })
        }
    }

    // `FileSource` is `Send + Sync`; the `RefCell` here is single-threaded test bookkeeping.
    unsafe impl Sync for Origin {}
    unsafe impl Send for Origin {}

    const INDEX: &str = r#"{"airport": {"x": 0, "y": 0, "width": 16, "height": 16},
                            "past":    {"x": 24, "y": 24, "width": 16, "height": 16}}"#;

    /// Both URLs are asked for, and the index is read against the sheet's size.
    ///
    /// The ordering that matters: an entry running off the image is refused, and that check needs
    /// the image — so a store that parsed the index first would admit rectangles that sample past
    /// the end of the texture.
    #[test]
    fn the_index_is_read_against_the_sheet() {
        let origin = Origin::new(INDEX);
        let mut sprites = Sprites::new("https://example.com/sprite", 1.0);
        assert!(sprites.fetch(&origin).expect("the origin answers"));

        let asked = origin.asked();
        assert_eq!(asked.len(), 2, "{asked:?}");
        assert!(asked.iter().any(|url| url.ends_with("sprite.json")));
        assert!(asked.iter().any(|url| url.ends_with("sprite.png")));

        assert!(sprites.get("airport").is_some());
        assert!(
            sprites.get("past").is_none(),
            "24 + 16 runs past a 32-pixel sheet"
        );
        assert_eq!(sprites.sheet().expect("a sheet").size(), (32, 32));
    }

    /// A second call fetches nothing.
    ///
    /// A style has one sprite and every tile of it asks the same question, so the store is what
    /// stops a map spending two round trips per tile on an answer it already has.
    #[test]
    fn a_second_call_asks_for_nothing() {
        let origin = Origin::new(INDEX);
        let mut sprites = Sprites::new("https://example.com/sprite", 1.0);
        assert!(sprites.fetch(&origin).expect("answers"));
        assert!(
            !sprites.fetch(&origin).expect("answers"),
            "it fetched again"
        );
        assert_eq!(origin.asked().len(), 2);
    }

    /// The retina base is asked for at a ratio above one.
    #[test]
    fn a_retina_ratio_asks_for_the_retina_sheet() {
        let origin = Origin::new(INDEX);
        let mut sprites = Sprites::new("https://example.com/sprite", 2.0);
        sprites.fetch(&origin).expect("answers");
        assert!(
            origin.asked().iter().all(|url| url.contains("@2x")),
            "{:?}",
            origin.asked()
        );
    }

    /// The sheet is owed once and then never again.
    ///
    /// A sprite sheet changes once in a style's life. A settled frame is a frame with no
    /// envelopes in it, and re-uploading a megabyte of unchanged icons every frame would make a
    /// still map the most expensive one.
    #[test]
    fn the_sheet_is_uploaded_once() {
        let origin = Origin::new(INDEX);
        let mut sprites = Sprites::new("https://example.com/sprite", 1.0);
        sprites.fetch(&origin).expect("answers");

        assert!(sprites.take_dirty(), "an arrived sheet owes an upload");
        assert!(!sprites.take_dirty(), "it owed a second one");
    }

    /// A sheet that does not decode gives no icons rather than icons pointing at nothing.
    #[test]
    fn an_undecodable_sheet_gives_no_icons() {
        let mut sprites = Sprites::new("https://example.com/sprite", 1.0);
        let error = sprites
            .load(INDEX.as_bytes(), b"<html>404</html>")
            .expect_err("a 404 page is not a sheet");
        assert!(matches!(error, LoadError::Sheet(_)));

        assert!(sprites.index().is_empty(), "an index survived a dead sheet");
        assert!(sprites.sheet().is_none());
        assert!(!sprites.take_dirty(), "nothing arrived, so nothing is owed");
    }

    /// An index that is not an index is an error too, and leaves the store empty.
    #[test]
    fn an_unreadable_index_leaves_the_store_empty() {
        let mut sprites = Sprites::new("https://example.com/sprite", 1.0);
        let sheet = encode(8, 8, 6, &[0u8; 8 * 8 * 4]);
        let error = sprites
            .load(b"[1,2,3]", &sheet)
            .expect_err("an array is not an index");
        assert!(matches!(error, LoadError::Index(_)));
        assert!(sprites.sheet().is_none(), "the sheet outlived its index");
    }
}

/// Icons are cut out of the sheet and repacked, which is what mbgl does and this did not.
///
/// The sheet is a *transport*, not a texture. `parseSprite` copies each icon into an image of
/// its own and `DynamicTextureAtlas` packs those with a pixel of padding around each; the icon
/// quad's one-pixel border then samples that padding. Drawing straight from the sheet — where
/// icons sit flush against each other — makes the border sample the neighbouring picture, which
/// is a hairline of the wrong icon around every marker on the map.
mod atlas {
    use super::encode;

    use tessella_glyph::sprite::{IconAtlas, Sprites, decode_sheet};

    /// Two 4x4 icons side by side, flush: red then blue.
    ///
    /// Flush on purpose. A sheet with a gutter would hide the bug this checks for.
    fn flush_sheet() -> Vec<u8> {
        let mut samples = vec![0u8; 8 * 4 * 4];
        for row in 0..4usize {
            for column in 0..8usize {
                let at = (row * 8 + column) * 4;
                let colour: [u8; 4] = if column < 4 {
                    [255, 0, 0, 255]
                } else {
                    [0, 0, 255, 255]
                };
                samples[at..at + 4].copy_from_slice(&colour);
            }
        }
        encode(8, 4, 6, &samples)
    }

    const INDEX: &str = r#"{"red":  {"x": 0, "y": 0, "width": 4, "height": 4},
                            "blue": {"x": 4, "y": 0, "width": 4, "height": 4}}"#;

    /// The packed rectangle is a pixel larger than the icon on every side.
    #[test]
    fn the_packed_rectangle_carries_a_pixel_of_padding() {
        let sheet = decode_sheet(&flush_sheet()).expect("decodes");
        let index = tessella_glyph::sprite::parse(INDEX.as_bytes(), Some(sheet.size()))
            .expect("the index parses");

        let mut atlas = IconAtlas::new(64, 64);
        let placed = atlas.add(&sheet, &index["red"]).expect("it fits");

        assert_eq!(
            (placed.padded_rect.width, placed.padded_rect.height),
            (6, 6),
            "a four-pixel icon occupies six with its padding"
        );
        assert_eq!(
            placed.display_size(),
            (4.0, 4.0),
            "the padding was not taken back out for layout"
        );
    }

    /// The pixel around a packed icon is transparent, not its neighbour.
    ///
    /// The assertion the whole rework exists for. In the sheet the two icons are flush, so the
    /// pixel to the right of the red one is blue; in the atlas it must be nothing.
    #[test]
    fn the_border_pixel_is_padding_and_not_the_neighbour() {
        let sheet = decode_sheet(&flush_sheet()).expect("decodes");
        let index =
            tessella_glyph::sprite::parse(INDEX.as_bytes(), Some(sheet.size())).expect("parses");

        // In the sheet, the pixel after the red icon is the blue one's first.
        // Row zero, column four: the first pixel of the blue icon.
        let after_red_in_sheet = 4 * 4;
        assert_eq!(
            &sheet.pixels[after_red_in_sheet..after_red_in_sheet + 4],
            &[0, 0, 255, 255],
            "the fixture is not flush, so this proves nothing"
        );

        let mut atlas = IconAtlas::new(64, 64);
        let red = atlas.add(&sheet, &index["red"]).expect("it fits");
        atlas.add(&sheet, &index["blue"]).expect("it fits");

        // In the atlas, the border pixel inside the padded rectangle is transparent.
        let (width, _) = atlas.size();
        let border_x = red.padded_rect.x + red.padded_rect.width - 1;
        let border_y = red.padded_rect.y;
        let at = ((border_y * width + border_x) as usize) * 4;
        assert_eq!(
            &atlas.pixels()[at..at + 4],
            &[0, 0, 0, 0],
            "the quad's border samples the neighbouring icon"
        );

        // And the icon's own pixels are there, unshifted.
        let inside = (((red.padded_rect.y + 1) * width + red.padded_rect.x + 1) as usize) * 4;
        assert_eq!(&atlas.pixels()[inside..inside + 4], &[255, 0, 0, 255]);
    }

    /// A retina icon keeps its ratio through the packing.
    #[test]
    fn the_pixel_ratio_survives_packing() {
        let sheet = decode_sheet(&encode(8, 8, 6, &[255u8; 8 * 8 * 4])).expect("decodes");
        let index = tessella_glyph::sprite::parse(
            br#"{"icon": {"x": 0, "y": 0, "width": 8, "height": 8, "pixelRatio": 2}}"#,
            Some(sheet.size()),
        )
        .expect("parses");

        let mut atlas = IconAtlas::new(64, 64);
        let placed = atlas.add(&sheet, &index["icon"]).expect("it fits");
        assert_eq!(placed.padded_rect.width, 10, "eight plus its padding");
        assert_eq!(
            placed.display_size(),
            (4.0, 4.0),
            "a 2x icon of eight sheet pixels is four logical ones"
        );
    }

    /// The store packs on load, so every icon in the index has a position.
    #[test]
    fn loading_packs_every_icon() {
        let mut sprites = Sprites::new("https://example.com/sprite", 1.0);
        sprites
            .load(INDEX.as_bytes(), &flush_sheet())
            .expect("both halves load");

        assert_eq!(sprites.positions().len(), 2);
        assert!(sprites.positions().contains_key("red"));
        assert_eq!(sprites.atlas().size(), (1024, 1024));
        assert!(
            !sprites.take_dirty_rects().is_empty(),
            "packing two icons dirtied nothing"
        );
    }
}

/// A sheet whose header asks for gigabytes is refused before it is decoded.
///
/// The classic decompression bomb: a PNG states its dimensions in twenty-five bytes and the
/// decoder allocates from them, so a few hundred bytes can ask for more memory than the device
/// has. `zune-png`'s own default stops at 16384 square, which is still a gibibyte of RGBA — far
/// past anything a sprite sheet is, and past what an RK3566 has to spare.
///
/// The header is parsed and the *rest is not*, which is the whole point: refusing costs a parse
/// rather than the allocation being asked for.
#[test]
fn a_sheet_that_asks_for_too_much_is_refused() {
    use tessella_glyph::sprite::MAX_SHEET_BYTES;

    // Eight thousand square: 256 mebibytes of RGBA, from a file of sixty-odd bytes. Chosen to
    // sit *inside* `zune-png`'s own 16384 limit and outside this one — a larger header is
    // refused by the decoder instead, which would make this test pass without exercising the
    // bound it is about.
    let bomb = header_only(8192, 8192);
    assert!(bomb.len() < 100, "the bomb is {} bytes", bomb.len());

    match sprite::decode_sheet(&bomb) {
        Err(SheetError::TooLarge { wanted }) => {
            assert!(wanted > MAX_SHEET_BYTES, "{wanted} is not over the ceiling");
        }
        other => panic!("a 256 MiB header was not refused: {other:?}"),
    }

    // The decoder's own cap is still there behind it, which is why a yet larger header fails
    // differently rather than reaching the check above.
    assert!(matches!(
        sprite::decode_sheet(&header_only(20_000, 20_000)),
        Err(SheetError::Decode(_))
    ));

    // And a sheet inside the ceiling still decodes, so the bound is not simply refusing
    // everything.
    let real = encode(64, 64, 6, &[0u8; 64 * 64 * 4]);
    assert!(sprite::decode_sheet(&real).is_ok());
}

/// A PNG that states enormous dimensions and carries almost no data.
///
/// Enough structure for the header to parse — a real bomb has that much — and a token IDAT that
/// could not fill a pixel of what the header claims. The whole file is under a hundred bytes.
fn header_only(width: u32, height: u32) -> Vec<u8> {
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

    let mut header = Vec::new();
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, 0]);

    let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let chunk = |kind: &[u8; 4], body: &[u8], png: &mut Vec<u8>| {
        png.extend_from_slice(&(body.len() as u32).to_be_bytes());
        let mut framed = kind.to_vec();
        framed.extend_from_slice(body);
        png.extend_from_slice(&framed);
        png.extend_from_slice(&crc32(&framed).to_be_bytes());
    };
    chunk(b"IHDR", &header, &mut png);
    // A zlib stream holding one empty stored block: valid, and nowhere near enough to fill what
    // the header asks for.
    chunk(
        b"IDAT",
        &[0x78, 0x01, 0x01, 0x00, 0x00, 0xff, 0xff],
        &mut png,
    );
    chunk(b"IEND", &[], &mut png);
    png
}
