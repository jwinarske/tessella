//! `text-letter-spacing`, against the capture.
//!
//! # Why this has its own capture
//!
//! Because nothing here had one, and three separate things were wrong with a single property.
//! The shaping unit tests all pass a spacing straight to the shaper and construct their own
//! characters, so they check the shaper's arithmetic and nothing about how a style reaches it;
//! every symbol capture until this one used the default spacing of zero, where all three
//! defects are the identity.
//!
//! # What was wrong
//!
//! The spec's unit is the **em** and everything downstream is in pixels, so mbgl multiplies by
//! `ONE_EM` where it reads the property. This did not, making every spaced label twenty-four
//! times too tight.
//!
//! Then it was applied **twice**. mbgl keeps two different advances for one glyph:
//! `getGlyphAdvance`, which line breaking measures with, is the glyph's advance *plus* the
//! spacing; `shapeLines`, which sets the line, uses the advance alone and adds the spacing
//! itself. One `Char::advance` carried the spaced version and the shaper added it again.
//!
//! And it was applied to text that must not have it. `allowsLetterSpacing` is false for a label
//! with any Arabic in it, because those letters are drawn joined and tracking them apart breaks
//! the joins rather than loosening the word.
//!
//! The first two are visible in this capture: at `text-letter-spacing: 0.1` mbgl advances 621
//! tile units between the first two glyphs of "Alpha" and this build advanced 550 — 2.2 pixels
//! short, which is exactly `0.1 * 24 - 0.1 - 0.1`. Both numbers were needed to tell which of
//! the two mistakes was in play; either alone would have been fitted by the wrong fix.

use std::collections::BTreeMap;

use tessella_glyph::fonts::{Dependencies, Fonts};
use tessella_orchestrate::tile::{Content, TileId, build_tile};
use tessella_source::geojson;
use tessella_source::tiling::TilingOptions;
use tessella_storage::source::{FetchError, FileSource, Response};
use tessella_style::Style;

const DUMP: &str = include_str!("../../../tests/golden/spaced_style.dump");

/// FNV-1a, as the probe hashes with.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The symbol drawable's vertex count and its layout attribute's field hash.
fn golden() -> (usize, u64) {
    let mut found = None;
    for line in DUMP.lines() {
        if !line.contains("sh0033") {
            continue;
        }
        if let Some(rest) = line.trim().strip_prefix("attr ")
            && rest.contains(" id=0 ")
            && let Some(hash) = rest
                .split_whitespace()
                .find_map(|token| token.strip_prefix("fld="))
            && let Ok(hash) = u64::from_str_radix(hash, 16)
        {
            let vertices = rest
                .split('.')
                .find_map(|part| part.strip_prefix('v'))
                .and_then(|part| part.split('#').next())
                .and_then(|part| part.parse().ok())
                .expect("a vertex count");
            found = Some((vertices, hash));
        }
    }
    found.expect("the capture holds a symbol drawable")
}

/// Serves the `file://` URLs the style's `glyphs` template builds.
struct Disk;
impl FileSource for Disk {
    fn fetch(&self, url: &str) -> Result<Response, FetchError> {
        let path = url.strip_prefix("file://").unwrap_or(url);
        Ok(Response {
            status: 200,
            body: std::fs::read(path).unwrap_or_default(),
            ..Response::default()
        })
    }
}

/// The spaced label's glyph positions are the capture's, through the production path.
#[test]
fn the_spaced_label_matches_the_capture() {
    let raw = include_str!("../../tessella-style/tests/spaced_style.json");
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let style: Style =
        serde_json::from_str(&raw.replace("TESSELLA", root)).expect("the style parses");
    let Some(tessella_style::Source::Geojson(source)) = style.source("probe") else {
        panic!("one geojson source")
    };
    let features = geojson::read(&source.data).expect("features read");

    let buckets = build_tile(
        &style,
        "probe",
        TileId::new(13, 4093, 2723),
        &features,
        TilingOptions::default(),
    )
    .expect("the tile builds");
    let layout = buckets
        .iter()
        .find_map(|bucket| match &bucket.content {
            Content::Symbol(layout) => Some(layout.clone()),
            _ => None,
        })
        .expect("a symbol layer");

    let mut fonts = Fonts::new(style.glyphs.clone().expect("a glyph URL"));
    let mut merged: Dependencies = BTreeMap::new();
    for (stack, codepoints) in layout.dependencies() {
        merged.entry(stack).or_default().extend(codepoints);
    }
    fonts.fetch(&merged, &Disk).expect("the font reads");

    let (buffers, _) = layout.lay_out(&fonts, None);
    let (vertices, hash) = golden();
    assert_eq!(buffers.vertices.len(), vertices, "glyph count");

    let mut field = Vec::with_capacity(buffers.vertices.len() * 8);
    for vertex in &buffers.vertices {
        for value in vertex.pos_offset {
            field.extend_from_slice(&value.to_le_bytes());
        }
    }
    assert_eq!(
        fnv1a(&field),
        hash,
        "the spaced label's glyph positions differ from the capture's"
    );
}

/// A label mbgl would refuse to track is not tracked here either.
///
/// The gate is on the *label*, not the character: one Arabic letter anywhere in it drops the
/// spacing for all of it, which is what `allowsLetterSpacing`'s `all_of` says.
#[test]
fn arabic_refuses_letter_spacing() {
    use tessella_glyph::text::allows_letter_spacing;

    assert!(allows_letter_spacing(&['A' as u32, 'l' as u32]));
    assert!(!allows_letter_spacing(&[0x0627]), "an alef alone");
    assert!(
        !allows_letter_spacing(&['A' as u32, 0x0627, 'l' as u32]),
        "one Arabic letter is enough to drop the spacing for the whole label"
    );
    assert!(
        !allows_letter_spacing(&[0xFE8D]),
        "the presentation forms count too, which is what the label holds after shaping"
    );
}
