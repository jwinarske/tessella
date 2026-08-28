//! Vertical writing, against the capture.
//!
//! # What a vertical label is, and what it is not
//!
//! Not a label whose pen moves downwards. mbgl lays a vertical shaping out along the *same* axis
//! as a horizontal one and turns each quad a quarter turn, so the glyphs run down the screen
//! because the quads were rotated, not because the shaper moved. Which is why the two shapings
//! can share a buffer and a size and differ only in their corners.
//!
//! And not one label but two. mbgl shapes a label that permits vertical writing **both ways**
//! and keeps both, because which one is drawn is a placement decision — a label that will not
//! fit across may still fit down — and placement runs per view, long after the tile is built.
//! This capture holds twenty-four vertices for three characters: three glyphs, four corners,
//! two orientations, horizontal first.
//!
//! # Which characters turn
//!
//! Not all of them, and which ones depends on why the line is vertical. A label following a
//! line that runs downwards keeps upright only the characters that have an upright form of
//! their own, and turns the rest with the line. A point label the style asked to set vertically
//! keeps everything upright *except* whitespace and the scripts whose letters join, which
//! turning would break. The predicates behind that are not transcribed: they are a hundred and
//! twenty lines of nested block tests with single characters carved out of the middle, so the
//! probe answers them for every code unit in the plane and the table is generated from that.
//!
//! # The fixture
//!
//! `TestFont/0-255.pbf` is a real font subset and covers Latin, which is enough for every
//! capture until this one — mbgl only shapes vertically when a character has an upright
//! orientation, and every such character is CJK. Vendoring a CJK font for three ideographs
//! would put a third party's outlines here for a test that never looks at them, so
//! `tools/glyph-fixtures/synthetic.py` writes the range instead: real metrics for a full-width
//! ideograph, and a distance field that is a gradient rather than a letter. What the capture
//! compares is positions, and a position comes from the metrics.

use std::collections::BTreeMap;

use tessella_glyph::fonts::{Dependencies, Fonts};
use tessella_orchestrate::tile::{Content, TileId, build_tile};
use tessella_source::geojson;
use tessella_source::tiling::TilingOptions;
use tessella_storage::source::{FetchError, FileSource, Response};
use tessella_style::Style;

const DUMP: &str = include_str!("../../../tests/golden/vertical_style.dump");

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
fn the_vertical_label_matches_the_capture() {
    let raw = include_str!("../../tessella-style/tests/vertical_style.json");
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
        "the vertical label's glyph positions differ from the capture's"
    );
}
