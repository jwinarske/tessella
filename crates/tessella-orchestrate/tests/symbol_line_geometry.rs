// SPDX-License-Identifier: BSD-2-Clause
//! Symbols placed along a line, and symbols with icons, counted against the oracle.
//!
//! # What the oracle gives
//!
//! `tests/golden/symbol_lines_style.dump`: three bent roads and two points under six layers —
//! a label written along its road and repeated by `symbol-spacing`, an icon-only layer along the
//! same roads, a point symbol carrying both halves, a layer with `text-translate` and
//! `icon-translate` set to different offsets and different anchors, and a label whose `text-size`
//! varies with zoom.
//!
//! Per layer the drawable names total to:
//!
//! | layer | vertices | indices |
//! |---|---|---|
//! | `line-label` | 224 | 336 |
//! | `line-icon` | 96 | 144 |
//! | `point-both` | 56 | 84 |
//! | `translated` | 56 | 84 |
//! | `zoom-sized` | 112 | 168 |
//!
//! # Why this capture exists
//!
//! The five symbol goldens that came before it are all point-placed text. None sets
//! `symbol-placement`, none sets `icon-image`, and none carries a translate — so line placement,
//! the icon half, and the anchor walk had no byte-level test at all.
//!
//! The three roads are deliberately a hair apart. An icon on one is inside the repeat distance of
//! an icon on the next, which is the case mbgl's `!feature.formattedText` guard exists for: run
//! the duplicate-name check on symbols that have no name and every icon-only symbol keys on the
//! same empty string, so one arrow suppresses the next street's. Under that bug this layer reads
//! 32 vertices against the oracle's 96. Spread the roads out and the test goes quiet, which is
//! how the first draft of this fixture was written and why it is worth saying.
//!
//! # What counts can and cannot see
//!
//! They see how much geometry a layer produces: whether an icon resolved, how many anchors a road
//! carried, how many glyphs a label shaped. They do **not** see where any of it was put — the
//! per-frame dynamic buffer a line-placed label is walked into is written by the frame, not the
//! bucket, so a walk stepped by the wrong size moves no number here. That defect needs pixels or
//! a frame-level capture; this file is not where it would be caught.
//!
//! # The icon's size is load-bearing
//!
//! `get_anchors` is handed the icon's extent along with the label's, so how many anchors fit on a
//! road depends on how wide the sprite is. Writing this test with a made-up 18-pixel icon in
//! place of the fixture's 21-pixel `oneway_road` put ten icons on one tile where the oracle puts
//! nine — which is why the positions below are read out of the sheet's own index rather than
//! invented.

use std::collections::BTreeMap;

use tessella_glyph::fonts::Fonts;
use tessella_glyph::sprite::{IconPosition, Positions};
use tessella_orchestrate::Content;
use tessella_orchestrate::tile::{TileId, build_tile};
use tessella_source::tiling::TilingOptions;
use tessella_storage::source::{FetchError, FileSource, Response};
use tessella_style::Style;

const DUMP: &str = include_str!("../../../tests/golden/symbol_lines_style.dump");
const STYLE: &str = include_str!("../../tessella-style/tests/symbol_lines_style.json");
const GLYPHS: &[u8] = include_bytes!("../../../tests/glyph-fixtures/TestFont/0-255.pbf");
const SHEET: &str = include_str!("../../../tests/sprite-fixtures/emerald.json");

/// The tiles the capture covers, which is the z13 cover of its camera.
const TILES: [(u32, u32); 6] = [
    (4092, 2723),
    (4092, 2724),
    (4093, 2723),
    (4093, 2724),
    (4094, 2723),
    (4094, 2724),
];

/// An origin serving the vendored font and nothing else.
struct Origin;
impl FileSource for Origin {
    fn fetch(&self, url: &str) -> Result<Response, FetchError> {
        let body = if url.contains("TestFont") && url.contains("0-255") {
            GLYPHS.to_vec()
        } else {
            Vec::new()
        };
        Ok(Response {
            status: 200,
            body,
            ..Response::default()
        })
    }
}
// `FileSource` is `Send + Sync`; this one holds nothing at all.
unsafe impl Sync for Origin {}
unsafe impl Send for Origin {}

/// Where the sheet's icons sit once packed, from the sheet's own index.
///
/// The rectangle is padded by one on every side, which is what the packer does and what the
/// quad's border samples. Only the icons this style names are packed; the position within the
/// atlas is arbitrary and the *size* is not.
fn sprites() -> Positions {
    let index: serde_json::Value = serde_json::from_str(SHEET).expect("the sheet index parses");
    ["oneway_road", "default_marker", "dot"]
        .iter()
        .enumerate()
        .map(|(slot, name)| {
            let entry = &index[*name];
            let read =
                |key: &str| u32::try_from(entry[key].as_u64().expect("a dimension")).expect("fits");
            #[allow(clippy::cast_possible_truncation)]
            let position = IconPosition {
                padded_rect: tessella_glyph::atlas::Rect {
                    x: slot as u32 * 128 + 1,
                    y: 1,
                    width: read("width") + 2,
                    height: read("height") + 2,
                },
                pixel_ratio: 1.0,
                sdf: false,
                content: None,
                text_fit_width: None,
                text_fit_height: None,
            };
            ((*name).to_string(), position)
        })
        .collect()
}

/// Vertex and index totals per layer index, read out of the oracle's drawable names.
fn oracle_totals() -> BTreeMap<usize, (usize, usize)> {
    let mut out: BTreeMap<usize, (usize, usize)> = BTreeMap::new();
    for line in DUMP.lines() {
        let Some(rest) = line.strip_prefix("drawable L") else {
            continue;
        };
        let layer: usize = rest[..5].parse().expect("a five-digit layer index");
        let verts: usize = rest
            .split(".v")
            .nth(1)
            .and_then(|t| t.split('#').next())
            .expect("a vertex count")
            .parse()
            .expect("digits");
        let indices: usize = line
            .split("idx=")
            .nth(1)
            .and_then(|t| t.split(':').next())
            .expect("an index count")
            .parse()
            .expect("digits");
        let slot = out.entry(layer).or_default();
        slot.0 += verts;
        slot.1 += indices;
    }
    out
}

/// The same totals from this build's own buckets, both halves of a symbol summed.
///
/// The oracle splits a symbol's text and icons into two drawables and this sums them, because
/// what is being compared is how much geometry the layer produces, not how it is parcelled out.
fn our_totals() -> (BTreeMap<usize, (usize, usize)>, Vec<String>) {
    let style = Style::parse(STYLE).expect("the style parses");
    let tessella_style::Source::Geojson(source) = style.source("probe").expect("a source") else {
        panic!("the fixture has one geojson source")
    };
    let features = tessella_source::geojson::read(&source.data).expect("features read");
    let positions = sprites();

    let mut out: BTreeMap<usize, (usize, usize)> = BTreeMap::new();
    for (x, y) in TILES {
        let buckets = build_tile(
            &style,
            "probe",
            TileId::new(13, x, y),
            &features,
            TilingOptions::default(),
        )
        .expect("the tile builds");
        for bucket in &buckets {
            let Content::Symbol(layout) = &bucket.content else {
                continue;
            };
            let mut fonts = Fonts::new(style.glyphs.clone().expect("a glyph URL"));
            fonts
                .fetch(&layout.dependencies(), &Origin)
                .expect("the origin answers");
            let (text, laid) = layout.lay_out(&fonts, Some(&positions));
            let (icons, _) = layout.lay_out_icons(&positions, &laid);
            let verts = text.vertices.len() + icons.vertices.len();
            if verts == 0 {
                continue;
            }
            let slot = out.entry(bucket.layer_index).or_default();
            slot.0 += verts;
            slot.1 += text.indices.len() + icons.indices.len();
        }
    }
    (out, style.layers.iter().map(|l| l.id.clone()).collect())
}

/// Every symbol layer produces exactly the geometry mbgl produces for it.
#[test]
fn line_placed_symbols_match_the_oracles_geometry() {
    let oracle = oracle_totals();
    let (ours, names) = our_totals();

    assert!(!ours.is_empty(), "the fixture built no symbol buckets");
    for (&layer, &(verts, indices)) in &ours {
        let name = names.get(layer).map_or("?", String::as_str);
        let &(want_v, want_i) = oracle
            .get(&layer)
            .unwrap_or_else(|| panic!("the oracle drew nothing for layer {layer} ({name})"));
        assert_eq!(
            (verts, indices),
            (want_v, want_i),
            "layer {layer} ({name}): {verts} vertices and {indices} indices against the oracle's \
             {want_v} and {want_i}"
        );
    }
}

/// The icon-only layer really does repeat along the roads.
///
/// A guard against the fixture degenerating: if the icon stopped resolving, or the repeat guard
/// thinned the layer back to one symbol a road, the test above would still pass against a
/// re-captured oracle that had done the same. Twenty-six icons over three roads is the shape.
#[test]
fn the_icon_only_layer_repeats_along_its_line() {
    let (ours, names) = our_totals();
    let icons = names
        .iter()
        .position(|n| n == "line-icon")
        .and_then(|i| ours.get(&i).copied())
        .expect("the icon layer built");
    assert_eq!(icons.0 % 4, 0, "an icon is one quad: {icons:?}");
    assert!(
        icons.0 / 4 > 6,
        "three roads should carry more than two icons each: {} icons",
        icons.0 / 4
    );
}
