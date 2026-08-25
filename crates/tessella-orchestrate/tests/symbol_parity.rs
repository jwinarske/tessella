//! Symbol geometry against the golden oracle (§9.1).
//!
//! `symbol_pipeline` runs the chain and checks it is self-consistent. This checks it against
//! mbgl: same style, same camera, same glyphs, and the drawables that come out are compared to
//! what the probe emitted.
//!
//! # Seven of eighty-seven lines cannot be compared, and the reason is mbgl's
//!
//! The glyph atlas is packed in the order glyphs arrive, and that order is not deterministic.
//! Over ten consecutive captures of this exact style, the symbol vertex hashes and the atlas
//! texture hash each took four or five distinct values with one dominating; every other line of
//! the eighty-seven was identical every time. The vertex hashes follow the atlas because the
//! `data` attribute carries each glyph's texture coordinates — identical geometry, different
//! bytes.
//!
//! So the golden elides those, the way it already elides `symbol_fade_change`, and this compares
//! what is left. That is less than the fill and line layers get, and it is not a shortcut: the
//! index buffers, the vertex *counts*, the drawable identities, the attribute layout and the
//! painter order are all pinned exactly, and between them they catch a glyph miscounted, a quad
//! emitted in the wrong winding, a label assigned to the wrong tile, or a symbol drawn in the
//! wrong pass.
//!
//! Making the vertex bytes comparable needs the atlas packed deterministically on mbgl's side,
//! which is a change to the probe rather than to this.

use std::collections::BTreeMap;

use tessella_glyph::atlas::Atlas;
use tessella_glyph::pbf::{self, Glyph, Metrics, Range};
use tessella_layout::symbol_bucket::{Glyphs, Label, SymbolBuffers, SymbolOptions, build_symbols};

const DUMP: &str = include_str!("../../../tests/golden/symbol_style.dump");
const GLYPHS: &[u8] = include_bytes!("../../../tests/glyph-fixtures/TestFont/0-255.pbf");

/// The probe's hash, FNV-1a 64 over a raw buffer.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// What the golden says about one symbol drawable.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Golden {
    tile: (u32, u32),
    vertices: usize,
    indices: usize,
    index_hash: u64,
    /// The per-frame position buffer's hash.
    position_hash: u64,
    /// The per-frame opacity buffer's hash.
    opacity_hash: u64,
    pass: u32,
    /// Attribute layout: (id, data type, offset, stride).
    attributes: Vec<(u32, u32, u32, u32)>,
}

/// Reads the symbol drawables out of the golden.
///
/// Symbols are shader `sh0033`, which is what tells them apart from the background in the same
/// dump without hard-coding a layer index.
fn golden_symbols() -> Vec<Golden> {
    let field = |line: &str, key: &str| -> Option<String> {
        line.split_whitespace()
            .find_map(|token| token.strip_prefix(key).map(ToString::to_string))
    };
    let tile_of = |id: &str| -> (u32, u32) {
        // `t13_00004093_00002723_o13_w+000`
        let mut parts = id
            .split('.')
            .find(|part| part.starts_with('t'))
            .expect("a tile")
            .split('_');
        parts.next();
        let x = parts.next().expect("x").parse().expect("a number");
        let y = parts.next().expect("y").parse().expect("a number");
        (x, y)
    };

    let mut out: Vec<Golden> = Vec::new();
    for line in DUMP.lines() {
        if let Some(rest) = line.strip_prefix("drawable ") {
            let id = rest.split_whitespace().next().expect("an id");
            if !id.contains("sh0033") {
                continue;
            }
            let idx = field(line, "idx=").expect("an index hash");
            let (count, hash) = idx.split_once(':').expect("count:hash");
            out.push(Golden {
                tile: tile_of(id),
                vertices: 0,
                indices: count.parse().expect("a count"),
                index_hash: u64::from_str_radix(hash, 16).expect("a hash"),
                position_hash: 0,
                opacity_hash: 0,
                pass: field(line, "pass=")
                    .expect("a pass")
                    .parse()
                    .expect("a number"),
                attributes: Vec::new(),
            });
        } else if line.starts_with("  attr ") && line.contains("sh0033") {
            let entry = out.last_mut().expect("an attribute before its drawable");
            let number = |key: &str| -> u32 {
                field(line, key)
                    .unwrap_or_default()
                    .parse()
                    .unwrap_or_default()
            };
            entry.attributes.push((
                number("id="),
                number("dt="),
                number("off="),
                number("stride="),
            ));
            // Attributes 3 and 4 are the per-frame buffers, which the golden does not elide.
            if let Some(source) = field(line, "src=")
                && let Some((_, hash)) = source.split_once(':')
                && let Ok(hash) = u64::from_str_radix(hash, 16)
            {
                match number("id=") {
                    3 => entry.position_hash = hash,
                    4 => entry.opacity_hash = hash,
                    _ => {}
                }
            }
        } else if line.starts_with("  seg ") && line.contains("sh0033") {
            let entry = out.last_mut().expect("a segment before its drawable");
            entry.vertices = field(line, "vlen=")
                .expect("a vertex length")
                .parse()
                .expect("a number");
        }
    }
    out
}

/// The style's three labels. Which tile each lands in is what the golden's tile ids say.
const LABELS: [&str; 3] = ["Alpha", "Bravo", "Charlie"];

/// The vendored font, with the glyphs of every label packed.
struct Font {
    glyphs: Vec<Glyph>,
    atlas: Atlas,
}

impl Font {
    fn new() -> Self {
        let glyphs = pbf::parse(
            Range {
                first: 0,
                last: 255,
            },
            GLYPHS,
        )
        .expect("the range parses");
        let mut atlas = Atlas::new(512, 512);
        for glyph in &glyphs {
            if LABELS
                .iter()
                .any(|label| label.chars().any(|character| character as u32 == glyph.id))
            {
                atlas.add(glyph.id, glyph);
            }
        }
        Self { glyphs, atlas }
    }
}

impl Glyphs for Font {
    fn metrics(&self, codepoint: u32) -> Option<(Metrics, bool)> {
        let glyph = self.glyphs.iter().find(|glyph| glyph.id == codepoint)?;
        Some((glyph.metrics, glyph.bitmap_size().is_some()))
    }
    fn rect(&self, codepoint: u32) -> Option<tessella_glyph::atlas::Rect> {
        self.atlas.get(codepoint)
    }
}

/// Builds one tile's symbol buffer from the labels that fall in it.
fn build(labels: &[&str], font: &Font) -> SymbolBuffers {
    let entries: Vec<Label> = labels
        .iter()
        .map(|text| Label {
            text: (*text).to_string(),
            anchor: (0.0, 0.0),
        })
        .collect();
    build_symbols(&entries, font, &SymbolOptions::default()).0
}

/// The golden holds exactly the symbol drawables this style should produce.
#[test]
fn the_oracle_drew_the_labels() {
    let symbols = golden_symbols();
    assert_eq!(symbols.len(), 2, "two tiles carry labels: {symbols:?}");

    // Five glyphs in one tile, twelve in the other: "Alpha", and "Bravo" plus "Charlie".
    let mut counts: Vec<usize> = symbols.iter().map(|symbol| symbol.vertices / 4).collect();
    counts.sort_unstable();
    assert_eq!(counts, [5, 12]);

    let total: usize = counts.iter().sum();
    let expected: usize = LABELS.iter().map(|label| label.chars().count()).sum();
    assert_eq!(total, expected, "every glyph of every label is drawn");
}

/// The index buffers match byte for byte.
///
/// The part of a symbol's geometry that does not depend on the atlas, and the one thing here
/// that is a true byte-exact comparison. It catches a quad emitted with its triangles in the
/// wrong order, a base index that does not advance, and a glyph count off by one — which between
/// them are most of the ways a symbol bucket goes wrong.
#[test]
fn the_index_buffers_match_the_oracle() {
    let font = Font::new();
    let symbols = golden_symbols();

    // The golden's two drawables hold five and twelve glyphs. Build each from the labels that
    // make it up: the tile with five is "Alpha", the other is "Bravo" then "Charlie".
    let groups: BTreeMap<usize, Vec<&str>> =
        [(5usize, vec!["Alpha"]), (12, vec!["Bravo", "Charlie"])]
            .into_iter()
            .collect();

    let mut compared = 0;
    for symbol in &symbols {
        let glyph_count = symbol.vertices / 4;
        let labels = groups.get(&glyph_count).expect("a known group");

        let buffers = build(labels, &font);

        assert_eq!(
            buffers.vertices.len(),
            symbol.vertices,
            "vertex count at {:?}",
            symbol.tile
        );
        assert_eq!(buffers.indices.len(), symbol.indices, "index count");

        let bytes: Vec<u8> = buffers
            .indices
            .iter()
            .flat_map(|index| index.to_le_bytes())
            .collect();
        assert_eq!(
            fnv1a(&bytes),
            symbol.index_hash,
            "index bytes at {:?}: {} indices",
            symbol.tile,
            buffers.indices.len()
        );
        compared += 1;
    }

    assert_eq!(compared, 2, "both symbol drawables compared");
}

/// The vertex layout matches the oracle's, attribute for attribute — all five of them.
///
/// Three interleaved at offsets 0, 8 and 16 with a stride of 24, then two more in buffers of
/// their own: the dynamic position at twelve bytes a vertex and the fade opacity at four. The
/// packing was written from mbgl's source; this is where it stops being a reading and becomes a
/// measurement of mbgl's output — and it is how the two separate buffers were confirmed to be
/// separate rather than assumed.
#[test]
fn the_vertex_layout_matches_the_oracle() {
    // mbgl's AttributeDataType, as the generated table spells it.
    const SHORT4: u32 = 11;
    const USHORT4: u32 = 15;
    const FLOAT3: u32 = 27;
    const FLOAT: u32 = 25;

    for symbol in golden_symbols() {
        assert_eq!(
            symbol.attributes,
            vec![
                // The interleaved layout buffer: position and offset, texture and size, pixel
                // offset and font scale.
                (0, SHORT4, 0, 24),
                (1, USHORT4, 8, 24),
                (2, SHORT4, 16, 24),
                // Then the two that change every frame while the layout does not, which is why
                // they are not in the same buffer.
                (3, FLOAT3, 0, 12),
                (4, FLOAT, 0, 4),
            ],
            "attribute layout at {:?}",
            symbol.tile
        );

        // One layout vertex is three attributes of four shorts: eight bytes each, twenty-four
        // in all, which is the stride the oracle declares.
        assert_eq!(
            core::mem::size_of::<tessella_layout::symbol_bucket::SymbolVertex>(),
            24,
            "this build's vertex is the size the oracle declares"
        );
        // And the two per-frame buffers are the widths it declares.
        assert_eq!(core::mem::size_of::<[f32; 3]>(), 12);
        assert_eq!(core::mem::size_of::<f32>(), 4);
    }
}

/// A tile's labels, each with its anchor in tile units.
type TileLabels<'a> = Vec<(&'a str, (f32, f32))>;

/// The per-frame position buffer matches the oracle byte for byte.
///
/// This is a real parity check, and it was nearly written off as one that could not be made.
/// The probe renders eight frames before dumping, so both per-frame buffers were assumed to
/// hold post-placement state that only a matching frame loop could reproduce. Solving for their
/// contents showed otherwise: the position buffer holds the label's anchor, written when the
/// geometry was built, with an angle of zero.
///
/// What it pins is worth more than that assumption suggested. The anchor is a *rounded* tile
/// coordinate, so this checks the projection from longitude and latitude into tile units against
/// mbgl's to the unit. And it checks that a tile's labels sit in the buffer in the order the
/// layer offers them: the twelve-glyph tile holds two labels, and the other order hashes
/// differently.
#[test]
fn the_position_buffer_matches_the_oracle() {
    let anchors = [
        ("Alpha", -0.13_f64, 51.515_f64),
        ("Bravo", -0.09, 51.495),
        ("Charlie", -0.11, 51.505),
    ];

    let mut by_tile: BTreeMap<(u32, u32), TileLabels<'_>> = BTreeMap::new();
    for (name, longitude, latitude) in anchors {
        let (tile, anchor) = project(longitude, latitude, 13);
        by_tile.entry(tile).or_default().push((name, anchor));
    }

    let font = Font::new();
    let mut compared = 0;
    for symbol in golden_symbols() {
        let labels = by_tile.get(&symbol.tile).expect("a tile the golden names");
        let entries: Vec<Label> = labels
            .iter()
            .map(|(text, anchor)| Label {
                text: (*text).to_string(),
                anchor: *anchor,
            })
            .collect();
        let (_, laid) = build_symbols(&entries, &font, &SymbolOptions::default());

        let mut bytes = Vec::new();
        for (label, entry) in laid.iter().zip(&entries) {
            for _ in label.vertices.clone() {
                bytes.extend_from_slice(&entry.anchor.0.to_le_bytes());
                bytes.extend_from_slice(&entry.anchor.1.to_le_bytes());
                bytes.extend_from_slice(&0.0f32.to_le_bytes());
            }
        }

        assert_eq!(bytes.len(), symbol.vertices * 12, "at {:?}", symbol.tile);
        assert_eq!(
            fnv1a(&bytes),
            symbol.position_hash,
            "position bytes at {:?} for {:?}",
            symbol.tile,
            labels.iter().map(|(name, _)| *name).collect::<Vec<_>>()
        );
        compared += 1;
    }

    assert_eq!(compared, 2, "both symbol drawables compared");
}

/// The opacity buffer matches too, and what it says is that placement never ran.
///
/// Every vertex is zero, which decodes as *not placed* at zero opacity — not the `(true, 1.0)`
/// mbgl writes when the geometry is built. So the probe's frames update the buffer from a
/// placement that holds no entry for these symbols, and write the default.
///
/// That makes this a weaker check than it looks: it pins the encoding and the buffer's width and
/// says nothing about placement. Comparing real placement output needs a capture in which the
/// probe has actually placed something, which is a change to the probe rather than to this.
#[test]
fn the_opacity_buffer_matches_the_oracle() {
    for symbol in golden_symbols() {
        let bytes = vec![0u8; symbol.vertices * 4];
        assert_eq!(
            fnv1a(&bytes),
            symbol.opacity_hash,
            "opacity bytes at {:?}",
            symbol.tile
        );
    }
}

/// This build produces the same number of per-frame entries as the oracle.
#[test]
fn the_per_frame_buffers_have_one_entry_per_vertex() {
    let font = Font::new();

    for (glyph_count, labels) in [(5usize, vec!["Alpha"]), (12, vec!["Bravo", "Charlie"])] {
        let buffers = build(&labels, &font);

        assert_eq!(buffers.glyphs(), glyph_count);
        assert_eq!(buffers.dynamic.len(), buffers.vertices.len());
        assert_eq!(buffers.opacity.len(), buffers.vertices.len());
    }
}

/// Web Mercator to a tile, and the rounded coordinate within it.
///
/// The rounding is not incidental: mbgl carries an anchor as a `GeometryCoordinate`, which is
/// integral, so a projection that kept the fraction disagrees with the oracle on every label.
fn project(longitude: f64, latitude: f64, zoom: u8) -> ((u32, u32), (f32, f32)) {
    const EXTENT: f64 = 8192.0;
    let scale = f64::from(1u32 << zoom);
    let x = (longitude + 180.0) / 360.0 * scale;
    let radians = latitude.to_radians();
    let y =
        (1.0 - (radians.tan() + 1.0 / radians.cos()).ln() / core::f64::consts::PI) / 2.0 * scale;

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (tile_x, tile_y) = (x.floor() as u32, y.floor() as u32);
    #[allow(clippy::cast_possible_truncation)]
    let anchor = (
        ((x - x.floor()) * EXTENT).round() as f32,
        ((y - y.floor()) * EXTENT).round() as f32,
    );
    ((tile_x, tile_y), anchor)
}

/// Symbols are drawn in the translucent pass, after the background.
#[test]
fn symbols_are_drawn_in_the_pass_the_oracle_uses() {
    for symbol in golden_symbols() {
        assert_eq!(symbol.pass, 2, "the translucent pass");
    }
}

/// The two hashes the golden elides are elided, and nothing else is.
///
/// A guard on the guard: if a later capture stops eliding them — or starts eliding something
/// else — this fails rather than quietly comparing less than it claims to.
#[test]
fn only_the_atlas_dependent_hashes_are_elided() {
    let elided: Vec<&str> = DUMP
        .lines()
        .filter(|line| line.contains("----------------"))
        .collect();

    assert_eq!(
        elided.len(),
        7,
        "six attribute lines and one texture: {elided:#?}"
    );
    // The two per-frame attributes are *not* elided: they were byte-identical across ten
    // captures, and eliding them would give away a comparison for nothing.
    assert!(
        elided
            .iter()
            .all(|line| !line.contains("id=3 ") && !line.contains("id=4 ")),
        "the dynamic and opacity buffers are stable and must stay compared"
    );
    assert!(
        elided
            .iter()
            .filter(|line| line.starts_with("texture "))
            .count()
            == 1,
        "exactly one texture is elided"
    );
    assert!(
        elided
            .iter()
            .filter(|line| line.starts_with("  attr "))
            .all(|line| line.contains("sh0033")),
        "only the symbol shader's attributes are elided"
    );
}

/// The same comparison, driven by the production path rather than by hand.
///
/// Every test above assembles the labels itself — it decides which two go in which tile, and it
/// packs the atlas from a list it was given. That checks the *layout* against mbgl and says
/// nothing about the path a frame actually takes: parse the style, cover the camera, build each
/// tile, fetch the glyph ranges the tile declared, shape, encode. Each of those is a place a
/// label can be lost, and the golden is the only thing that would notice.
///
/// So this runs it end to end, and the tile assignment is the projection's rather than a
/// constant: the golden says which tiles carry labels and how many glyphs are in each, and the
/// builder has to agree without being told.
mod through_the_builder {
    use super::{Golden, fnv1a, golden_symbols};

    use tessella_capture_abi::envelope::GeometryId;
    use tessella_glyph::fonts::{Dependencies, Fonts};
    use tessella_orchestrate::emit::{SlabArena, encode_symbol};
    use tessella_orchestrate::tile::{Content, TileId, build_tile};
    use tessella_source::geojson;
    use tessella_source::tiling::TilingOptions;
    use tessella_storage::source::{FetchError, FileSource, Response};
    use tessella_style::Style;

    /// The style, with the checkout path substituted the way the capture does it.
    fn style() -> Style {
        let raw = include_str!("../../tessella-style/tests/symbol_style.json");
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
        serde_json::from_str(&raw.replace("TESSELLA", root)).expect("the style parses")
    }

    fn features() -> Vec<tessella_source::GeoJsonFeature> {
        let style = style();
        let Some(tessella_style::Source::Geojson(source)) = style.source("probe") else {
            panic!("the probe style has one geojson source");
        };
        geojson::read(&source.data).expect("features read")
    }

    /// Serves the `file://` URLs the style's `glyphs` template builds.
    ///
    /// The capture reads the same font off the same disk, which is what makes the comparison a
    /// comparison: a fixture served two different ways is two different fixtures.
    struct Disk;

    impl FileSource for Disk {
        fn fetch(&self, url: &str) -> Result<Response, FetchError> {
            let path = url.strip_prefix("file://").unwrap_or(url);
            // A range the font does not have is a 200 with no body — an origin saying it has
            // nothing there — not a transport failure.
            let body = std::fs::read(path).unwrap_or_default();
            Ok(Response {
                status: 200,
                body,
                ..Response::default()
            })
        }
    }

    /// Builds every tile the golden names, in the golden's order.
    fn built() -> Vec<(Golden, tessella_layout::symbol_layout::SymbolLayout)> {
        let style = style();
        let features = features();
        golden_symbols()
            .into_iter()
            .map(|symbol| {
                let (x, y) = symbol.tile;
                let buckets = build_tile(
                    &style,
                    "probe",
                    TileId::new(13, x, y),
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
                (symbol, layout)
            })
            .collect()
    }

    /// A store with every tile's declared glyphs fetched from the style's own `glyphs` URL.
    fn fonts(layouts: &[(Golden, tessella_layout::symbol_layout::SymbolLayout)]) -> Fonts {
        let url = style().glyphs.clone().expect("the style names a glyph URL");
        let mut fonts = Fonts::new(url);

        // Merged across tiles before anything is fetched, which is the whole point of a
        // process-scoped store: two tiles of the same style share their letters.
        let mut merged: Dependencies = Dependencies::new();
        for (_, layout) in layouts {
            for (stack, codepoints) in layout.dependencies() {
                merged.entry(stack).or_default().extend(codepoints);
            }
        }
        fonts.fetch(&merged, &Disk).expect("the font reads");
        fonts
    }

    /// The builder puts the labels in the tiles the oracle put them in.
    ///
    /// Not asserted as a constant: the golden says five glyphs here and twelve there, and the
    /// projection has to land the three features so that comes out. A label one tile off would
    /// still shape and still encode, and only this notices.
    #[test]
    fn the_builder_lands_the_labels_where_the_oracle_did() {
        let layouts = built();
        let fonts = fonts(&layouts);
        assert_eq!(layouts.len(), 2);

        for (symbol, layout) in &layouts {
            let (buffers, laid) = layout.lay_out(&fonts);
            assert_eq!(
                buffers.vertices.len(),
                symbol.vertices,
                "{} labels at {:?}: {:?}",
                laid.len(),
                symbol.tile,
                layout
                    .pending
                    .iter()
                    .map(|pending| pending.text.as_str())
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                buffers.indices.len(),
                symbol.indices,
                "at {:?}",
                symbol.tile
            );
        }
    }

    /// And the index bytes it produces are the oracle's, byte for byte.
    #[test]
    fn the_built_index_buffers_match_the_oracle() {
        let layouts = built();
        let fonts = fonts(&layouts);

        for (symbol, layout) in &layouts {
            let (buffers, _) = layout.lay_out(&fonts);
            let bytes: Vec<u8> = buffers
                .indices
                .iter()
                .flat_map(|index| index.to_le_bytes())
                .collect();
            assert_eq!(
                fnv1a(&bytes),
                symbol.index_hash,
                "index bytes at {:?}",
                symbol.tile
            );
        }
    }

    /// The encoded stream carries the oracle's five attributes, from a tile-built buffer.
    ///
    /// The last link. `symbol_emit` checks the encoder against a buffer it made up; this checks
    /// it against one the builder made, so a layer that produced the right glyphs and reached
    /// the wire with the wrong descriptors is caught.
    #[test]
    fn a_built_tile_encodes_the_oracle_s_attributes() {
        let layouts = built();
        let fonts = fonts(&layouts);

        for (symbol, layout) in &layouts {
            let (buffers, _) = layout.lay_out(&fonts);
            let mut arena = SlabArena::default();
            let encoded = encode_symbol(&mut arena, GeometryId(1), &buffers, 0, true);

            let mut attributes: Vec<(u32, u32, u32, u32)> = encoded
                .attributes()
                .iter()
                .map(|attribute| {
                    (
                        attribute.attr_id,
                        u32::from(attribute.data_type),
                        attribute.offset,
                        attribute.stride,
                    )
                })
                .collect();
            let mut expected = symbol.attributes.clone();
            attributes.sort_unstable();
            expected.sort_unstable();
            assert_eq!(attributes, expected, "at {:?}", symbol.tile);

            // And the counts the descriptors imply are the oracle's.
            assert_eq!(
                encoded.segments()[0].vertex_length as usize,
                symbol.vertices,
                "at {:?}",
                symbol.tile
            );
        }
    }
}
