//! An `["image", …]` section inside a label, against the capture.
//!
//! # What an inline image is
//!
//! A sprite drawn in the run of the text, sharing the label's line and its buffer. Every
//! measurement of it comes from somewhere else than a glyph's, which is the whole of the
//! feature: its size is the sprite's own in pixels rather than a font's in ems, so it is
//! rescaled by `ONE_EM / text-size` to sit on a line of text; its baseline offset aligns its
//! *bottom* to the line rather than a glyph's feet; and if it is taller than an em it makes the
//! line taller instead of overlapping the one above.
//!
//! # Why the whole layer changes shader
//!
//! The glyphs are in the glyph atlas and the sprite is in the icon atlas, and one drawable
//! samples one texture — unless it is `SymbolTextAndIconShader`, which declares two. So one
//! image anywhere in a layer binds all of it to that shader, and the capture shows it: `sh0034`
//! where every other symbol drawable is `sh0033`, with `slot=0` and `slot=1` both bound.
//!
//! That is also why the SDF flag is per *quad* here rather than per drawable. A glyph is always
//! a distance field and a sprite usually is not, and now they share a buffer, so which of the
//! two a vertex is has to travel with the vertex.
//!
//! # What it needed on the way
//!
//! The `["image", …]` operator, which was not implemented. The `format` evaluator already knew
//! to recognise an image section by an object with a `name` in it — nothing produced one, so
//! the expression failed to parse and the layer drew no labels at all rather than labels
//! without pictures.
//!
//! Needs the `image` feature, because the sprite sheet is a PNG and decoding one is behind it.
//! The parity is not optional — the feature is only how the fixture is read.

#![cfg(feature = "image")]

use std::collections::BTreeMap;

use tessella_glyph::fonts::{Dependencies, Fonts};
use tessella_orchestrate::tile::{Content, TileId, build_tile};
use tessella_source::geojson;
use tessella_source::tiling::TilingOptions;
use tessella_storage::source::{FetchError, FileSource, Response};
use tessella_style::Style;

const DUMP: &str = include_str!("../../../tests/golden/image_text_style.dump");

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
        if !line.contains("sh0034") {
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
fn the_inline_image_matches_the_capture() {
    let raw = include_str!("../../tessella-style/tests/image_text_style.json");
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

    const SHEET: &[u8] = include_bytes!("../../../tests/sprite-fixtures/emerald.png");
    const INDEX: &[u8] = include_bytes!("../../../tests/sprite-fixtures/emerald.json");
    let mut sprites = tessella_glyph::sprite::Sprites::new("file://emerald", 1.0);
    sprites.load(INDEX, SHEET).expect("the sprite loads");

    let (buffers, _) = layout.lay_out(&fonts, Some(sprites.positions()));
    assert!(
        buffers.icons_in_text,
        "a label with an image section makes the whole bucket a two-texture one"
    );
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
        "the inline image's glyph positions differ from the capture's"
    );
}
