//! Reading a glyph range, checked against mbgl's own expectations.
//!
//! `fake_glyphs-0-255.pbf` is mbgl's fixture and is built for exactly this: a file of glyphs
//! that are wrong in a different way each, plus one that is right. A parser that accepted them
//! all would pass a test written against a real font, because a real font contains no bad
//! glyphs — which is why the fixture exists and why it is the one worth vendoring.

use tessella_glyph::pbf::{self, BORDER, Range};

const FAKE: &[u8] = include_bytes!("../../../tests/glyph-fixtures/fake_glyphs-0-255.pbf");
const REAL: &[u8] = include_bytes!("../../../tests/glyph-fixtures/TestFont/0-255.pbf");

/// mbgl `GlyphPBF.Parsing`: of everything in the fixture, exactly one glyph survives.
#[test]
fn only_the_valid_glyph_survives() {
    let glyphs = pbf::parse(
        Range {
            first: 0,
            last: 255,
        },
        FAKE,
    )
    .expect("the file decodes");

    assert_eq!(glyphs.len(), 1, "the rest of the fixture is malformed");
    let glyph = &glyphs[0];
    assert_eq!(glyph.id, 69);
    assert_eq!(glyph.metrics.width, 1);
    assert_eq!(glyph.metrics.height, 1);
    assert_eq!(glyph.metrics.left, 20);
    assert_eq!(glyph.metrics.top, 2);
    assert_eq!(glyph.metrics.advance, 8);

    // One pixel plus a three-pixel border on each side is seven by seven, and mbgl's fixture
    // fills it with 'x'.
    assert_eq!(glyph.bitmap_size(), Some((7, 7)));
    assert_eq!(glyph.bitmap.len(), 49);
    assert!(glyph.bitmap.iter().all(|byte| *byte == b'x'));
}

/// A real range parses, and every glyph in it is self-consistent.
///
/// The fixture above proves the rejections fire. This proves they do not fire on a real font,
/// which is the failure the rejections invite: a bound one off in the wrong direction drops
/// legitimate letters and the text merely comes out wrong.
#[test]
fn a_real_range_parses_whole() {
    let range = Range {
        first: 0,
        last: 255,
    };
    let glyphs = pbf::parse(range, REAL).expect("the file decodes");

    assert!(glyphs.len() > 100, "only {} glyphs", glyphs.len());
    for glyph in &glyphs {
        assert!(
            range.contains(glyph.id),
            "{} is outside the range",
            glyph.id
        );
        match glyph.bitmap_size() {
            Some((width, height)) => assert_eq!(
                glyph.bitmap.len(),
                (width * height) as usize,
                "glyph {} carries a bitmap its metrics do not describe",
                glyph.id
            ),
            None => assert!(glyph.bitmap.is_empty()),
        }
    }

    // ASCII letters and digits are what a range 0-255 is mostly for; if the parse quietly kept
    // only the punctuation this would still have passed the count above.
    for codepoint in [b'A', b'a', b'0', b'z'] {
        assert!(
            glyphs.iter().any(|glyph| glyph.id == u32::from(codepoint)),
            "{} is missing",
            codepoint as char
        );
    }
}

/// A space has an advance and no pixels, and must keep the advance.
///
/// The one case where "no bitmap" is correct rather than malformed. A parser that treated a
/// zero-area glyph as invalid would drop every space, and the shaper would set the words run
/// together — which looks like a shaping bug several layers from the cause.
#[test]
fn a_space_keeps_its_advance() {
    let glyphs = pbf::parse(
        Range {
            first: 0,
            last: 255,
        },
        REAL,
    )
    .expect("the file decodes");
    let space = glyphs
        .iter()
        .find(|glyph| glyph.id == u32::from(b' '))
        .expect("a range covering ASCII has a space");

    assert!(space.bitmap.is_empty(), "a space has nothing to draw");
    assert!(space.metrics.advance > 0, "and still moves the pen");
    assert_eq!(space.bitmap_size(), None);
}

/// Glyphs outside the range asked for are dropped.
///
/// The id check is what stops a mislabelled or mis-served file from filling the wrong block of
/// the atlas: the range is part of the URL, so a server answering `0-255` with `256-511` would
/// otherwise be believed.
#[test]
fn glyphs_outside_the_range_are_dropped() {
    let empty = pbf::parse(
        Range {
            first: 256,
            last: 511,
        },
        REAL,
    )
    .expect("the file decodes");
    assert!(
        empty.is_empty(),
        "an ASCII range answered {} glyphs for 256-511",
        empty.len()
    );
}

/// Ranges are 256-codepoint blocks, and the scheme stops at the BMP.
#[test]
fn ranges_are_blocks_of_two_hundred_and_fifty_six() {
    assert_eq!(
        Range::of(0),
        Some(Range {
            first: 0,
            last: 255
        })
    );
    assert_eq!(
        Range::of(255),
        Some(Range {
            first: 0,
            last: 255
        })
    );
    assert_eq!(
        Range::of(256),
        Some(Range {
            first: 256,
            last: 511
        })
    );
    assert_eq!(
        Range::of(65535),
        Some(Range {
            first: 65280,
            last: 65535
        })
    );

    // Above the BMP there is no range file; those glyphs are rasterized locally. Answering
    // with a range would build a URL no origin serves.
    assert_eq!(Range::of(65536), None);

    assert_eq!(Range::of(97).expect("ascii").to_string(), "0-255");
    assert_eq!(Range::of(12288).expect("cjk").to_string(), "12288-12543");
}

/// Garbage is refused rather than parsed into plausible glyphs.
#[test]
fn a_non_glyph_file_yields_nothing() {
    let range = Range {
        first: 0,
        last: 255,
    };
    // Well-formed protobuf, wrong contents: fields nothing here reads.
    assert!(
        pbf::parse(range, &[0x28, 0x01])
            .expect("decodes")
            .is_empty()
    );
    // Truncated in the middle of a length-delimited field.
    assert!(pbf::parse(range, &[0x0a, 0x7f]).is_err());
    assert!(pbf::parse(range, &[]).expect("decodes").is_empty());
}

/// The border is three, and the ecosystem depends on it.
#[test]
fn the_border_is_what_the_encoders_used() {
    assert_eq!(BORDER, 3);
}

/// Encodes a minimal glyph range file, so a specific malformation can be tested.
///
/// mbgl's fixture covers the malformations *it* was built for. The required-field check
/// survived being deleted against it, which means the fixture happens not to contain a glyph
/// that is complete except for one field — so this builds one.
fn encode(fields: &[(u32, u64)], bitmap: Option<&[u8]>) -> Vec<u8> {
    fn varint(out: &mut Vec<u8>, mut value: u64) {
        while value >= 0x80 {
            out.push((value as u8) | 0x80);
            value >>= 7;
        }
        out.push(value as u8);
    }
    fn tagged(out: &mut Vec<u8>, number: u32, wire: u32) {
        varint(out, u64::from(number << 3 | wire));
    }

    let mut glyph = Vec::new();
    for (number, value) in fields {
        tagged(&mut glyph, *number, 0);
        varint(&mut glyph, *value);
    }
    if let Some(bitmap) = bitmap {
        tagged(&mut glyph, 2, 2);
        varint(&mut glyph, bitmap.len() as u64);
        glyph.extend_from_slice(bitmap);
    }

    let mut stack = Vec::new();
    tagged(&mut stack, 3, 2);
    varint(&mut stack, glyph.len() as u64);
    stack.extend_from_slice(&glyph);

    let mut out = Vec::new();
    tagged(&mut out, 1, 2);
    varint(&mut out, stack.len() as u64);
    out.extend_from_slice(&stack);
    out
}

/// Zigzag, as the encoder writes a signed metric.
fn zz(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

/// Every one of the six fields is required, and any one missing drops the glyph.
///
/// Proto2 makes them all optional on the wire, so a glyph missing `advance` parses perfectly
/// and then lays out on top of its neighbour. Nothing errors; the text is simply wrong.
#[test]
fn a_glyph_missing_any_required_field_is_dropped() {
    let range = Range {
        first: 0,
        last: 255,
    };
    let pixels = vec![b'x'; 49];
    let complete: [(u32, u64); 6] = [
        (1, 69),     // id
        (3, 1),      // width
        (4, 1),      // height
        (5, zz(20)), // left
        (6, zz(2)),  // top
        (7, 8),      // advance
    ];

    // The control: with all six, it parses.
    let whole = pbf::parse(range, &encode(&complete, Some(&pixels))).expect("decodes");
    assert_eq!(whole.len(), 1, "the complete glyph must survive");

    // And with any one removed, it does not.
    for missing in 0..complete.len() {
        let kept: Vec<(u32, u64)> = complete
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != missing)
            .map(|(_, field)| *field)
            .collect();
        let parsed = pbf::parse(range, &encode(&kept, Some(&pixels))).expect("decodes");
        assert!(
            parsed.is_empty(),
            "a glyph without field {} was accepted",
            complete[missing].0
        );
    }
}

/// A negative bearing survives the round trip.
///
/// `left` and `top` are `sint64`, and a glyph whose bitmap hangs left of the pen — which is
/// most italics and many accents — has a negative `left`. Decoding the zigzag wrongly turns
/// that into a large positive, and the letter lands off the end of the line.
#[test]
fn a_negative_bearing_decodes_as_negative() {
    let range = Range {
        first: 0,
        last: 255,
    };
    let pixels = vec![b'x'; 49];
    let glyph = encode(
        &[(1, 65), (3, 1), (4, 1), (5, zz(-4)), (6, zz(-1)), (7, 8)],
        Some(&pixels),
    );

    let parsed = pbf::parse(range, &glyph).expect("decodes");
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].metrics.left, -4);
    assert_eq!(parsed[0].metrics.top, -1);
}
