//! A label whose sections are set at different sizes (§R2's per-section scaling).
//!
//! # Why this has its own capture
//!
//! `symbol_style.dump` is the R2 symbol reference and the golden README's rule is that a new
//! question gets a new capture rather than an amended one. Adding a layer to it moved every
//! count nine tests assert, none of which is about scaling.
//!
//! # Why it can be checked at all
//!
//! It could not be, until the probe emitted a per-attribute hash. Three attributes share the
//! glyph vertex buffer and only one carries texture coordinates, but the whole buffer had to be
//! elided because the glyph atlas is packed in arrival order — so a capture of this label and a
//! capture of the same label at scale one were byte-identical in everything compared. Measured,
//! not assumed: that is why `fld=` exists.
//!
//! # What a scaled section changes
//!
//! Three things, and mbgl reads the scale at each of them. The glyph's *advance*, so the pen
//! moves further — `metrics.advance * scale + spacing`, the spacing unscaled, so a double-size
//! word is not also a loosely-set one. The *line's height*, which takes the largest scale on it,
//! with every glyph offset by `(lineMaxScale - scale) * ONE_EM` so the small text keeps its feet
//! on the same baseline instead of floating. And the *quad*, since a bigger glyph covers more of
//! the atlas rectangle it samples.
//!
//! Getting one of the three wrong draws text of the right size in the wrong place, or the wrong
//! size in the right one — which is exactly the failure a capture that could not see any of it
//! would have let through.

use tessella_glyph::shaping::{Char, Options as ShapeOptions};
use tessella_glyph::text::ONE_EM;

/// FNV-1a, as the probe hashes with.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

const DUMP: &str = include_str!("../../../tests/golden/scaled_style.dump");

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

/// The oracle shaped both sections.
#[test]
fn both_sections_are_laid_out() {
    let (vertices, _) = golden();
    assert_eq!(
        vertices, 32,
        "eight glyphs of four vertices: `Big` at twice the size and `small` at half, both drawn"
    );
}

/// A line's height is its largest section's, and the smaller text sits on the same baseline.
///
/// The arithmetic mbgl does, checked here rather than through the capture because the capture
/// hashes the result and this is the rule that produces it. `Big` is at two and `small` at a
/// half, so the line is two ems tall and every `small` glyph drops by one and a half of one.
#[test]
fn the_line_takes_its_largest_scale() {
    let chars: Vec<Char> = "Big"
        .chars()
        .map(|c| Char::new(c as u32, 10.0).at_scale(2.0))
        .chain(
            "small"
                .chars()
                .map(|c| Char::new(c as u32, 10.0).at_scale(0.5)),
        )
        .collect();

    let shaping = tessella_glyph::shaping::shape(
        &chars,
        &ShapeOptions {
            max_width: 0.0,
            line_height: ONE_EM,
            ..ShapeOptions::default()
        },
    );

    let line = shaping.lines.first().expect("one line");
    assert_eq!(line.len(), 8);

    // The big section defines the baseline, so it is not offset at all.
    for glyph in &line[..3] {
        assert!(
            (glyph.y - line[0].y).abs() < f32::EPSILON,
            "the largest section sets the baseline"
        );
    }
    // And the small section drops by the difference, rather than floating at the line's top.
    let drop = line[3].y - line[0].y;
    assert!(
        (drop - (2.0 - 0.5) * ONE_EM).abs() < 0.01,
        "small text should drop by `(lineMaxScale - scale) * ONE_EM`, dropped by {drop}"
    );

    // Two ems tall, not one: a line with a double-size word is a double-height line.
    assert!(
        (shaping.bottom - shaping.top - 2.0 * ONE_EM).abs() < 0.01,
        "height {} should be two ems",
        shaping.bottom - shaping.top
    );
}

/// An unscaled label is unchanged, which is every label a style has ever written.
#[test]
fn one_section_at_one_is_what_it_always_was() {
    let plain: Vec<Char> = "Big".chars().map(|c| Char::new(c as u32, 10.0)).collect();
    let marked: Vec<Char> = plain.iter().map(|c| c.at_scale(1.0)).collect();

    let options = ShapeOptions {
        max_width: 0.0,
        line_height: ONE_EM,
        ..ShapeOptions::default()
    };
    assert_eq!(
        tessella_glyph::shaping::shape(&plain, &options),
        tessella_glyph::shaping::shape(&marked, &options),
        "a scale of one is the identity, so nothing that never writes `format` moves"
    );
}

/// The whole thing, through the builder, against the capture.
///
/// The rules above are checked from the arithmetic; this checks the result. It runs the same
/// style the probe ran through the production path — parse, build the tile, collect the glyph
/// dependencies, fetch the same font off the same disk, lay out — and hashes the glyph positions
/// the way the probe hashes them.
///
/// # What it caught
///
/// It failed first, and what it reported was worth more than a green tick: the probe was taught
/// to decode this attribute so the difference could be read rather than hashed, and it named two
/// defects that had nothing to do with per-section scaling.
///
/// ```text
/// mbgl:  (342,6318,-1688,-666) (342,6318,-408,-666) (342,6318,-1688,934) …
/// then:  (342,6317,-1688,-512) (342,6317,-408,-512) (342,6317,-1688,1088) …
/// ```
///
/// Every x offset matched, exactly — all thirty-two, across both sections. So the advances,
/// their per-section scaling, the letter spacing that is *not* scaled with them, and the
/// horizontal alignment were already right.
///
/// The anchor was a unit low, and in the *unscaled* case too — the capture answers 6318 for both
/// styles. A fill or a line reaches the tile through `to_tile_ring`, which rounds because that is
/// what geojson-vt does before mbgl sees a coordinate; the symbol path took the raw float, and
/// the symbol vertex packs its anchor by truncating, so 6317.68 was drawn at 6317. Nothing saw
/// it because the passing layout test projects with its own helper instead of building a tile.
///
/// The y offsets were out by a constant 154 — 4.8125 pixels, which is exactly `1.2 em - 1 em`.
/// The shaper was handed `ONE_EM` as its line height, ignoring `text-line-height`. A single line
/// hides it: the alignment branch a one-line label takes does not use the block height at all,
/// and both heights give a zero shift. Grow one line and the other branch runs, and every line
/// of every multi-line label was 4.8 pixels too close to the one above it.
#[test]
fn the_shaped_label_matches_the_capture() {
    use std::collections::BTreeMap;

    use tessella_glyph::fonts::{Dependencies, Fonts};
    use tessella_orchestrate::tile::{Content, TileId, build_tile};
    use tessella_source::geojson;
    use tessella_source::tiling::TilingOptions;
    use tessella_storage::source::{FetchError, FileSource, Response};
    use tessella_style::Style;

    /// Serves the `file://` URLs the style's `glyphs` template builds, off the disk the capture
    /// read the same font from.
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

    let raw = include_str!("../../tessella-style/tests/scaled_style.json");
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let style: Style =
        serde_json::from_str(&raw.replace("TESSELLA", root)).expect("the style parses");
    let Some(tessella_style::Source::Geojson(source)) = style.source("probe") else {
        panic!("one geojson source")
    };
    let features = geojson::read(&source.data).expect("features read");

    // The tile the capture put the label in.
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

    // The attribute's own bytes, as the probe gathers them: `pos_offset` is the first eight of
    // every twenty-four.
    let mut field = Vec::with_capacity(buffers.vertices.len() * 8);
    for vertex in &buffers.vertices {
        for value in vertex.pos_offset {
            field.extend_from_slice(&value.to_le_bytes());
        }
    }
    assert_eq!(
        fnv1a(&field),
        hash,
        "the scaled label's glyph positions differ from the capture's"
    );
}
