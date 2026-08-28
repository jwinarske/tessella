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
    /// Attribute 0's own bytes, hashed — the glyph's tile position and its label anchor.
    ///
    /// Newly comparable. Three attributes share the glyph vertex buffer and only attribute 1
    /// carries texture coordinates, but the shared hash had to be elided because the atlas
    /// packing is not deterministic — which took this one with it. The probe emits a per-field
    /// hash now, so what the buffer says about geometry can be checked where what it says about
    /// the atlas still cannot.
    layout_hash: u64,
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
                layout_hash: 0,
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
            // Attribute 0's own bytes: the glyph position and the label anchor, which do not
            // depend on where the atlas put anything.
            if number("id=") == 0
                && let Some(hash) = field(line, "fld=")
                && let Ok(hash) = u64::from_str_radix(&hash, 16)
            {
                entry.layout_hash = hash;
            }
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
        pending: 0,
        sections: vec![tessella_layout::symbol::Section { text: (*text).to_string(), scale: 1.0 }],
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
        pending: 0,
        sections: vec![tessella_layout::symbol::Section { text: (*text).to_string(), scale: 1.0 }],
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

    use tessella_capture_abi::envelope::{GeometryId, TextureId};

    /// The glyph atlas a symbol drawable samples. Any id will do: what is under test is the
    /// slot, which comes from the shader's generated table rather than from this.
    const ATLAS: TextureId = TextureId(3);
    use tessella_glyph::fonts::{Dependencies, Fonts};
    use tessella_orchestrate::emit::{SlabArena, encode_symbol};
    use tessella_orchestrate::tile::{Content, TileId, build_tile};
    use tessella_source::geojson;
    use tessella_source::tiling::TilingOptions;
    use tessella_storage::source::{FetchError, FileSource, Response};
    use tessella_style::Style;

    /// The style, with the checkout path substituted the way the capture does it.
    pub(super) fn style() -> Style {
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
    pub(super) fn built() -> Vec<(Golden, tessella_layout::symbol_layout::SymbolLayout)> {
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
    pub(super) fn fonts(
        layouts: &[(Golden, tessella_layout::symbol_layout::SymbolLayout)],
    ) -> Fonts {
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
            let (buffers, laid) = layout.lay_out(&fonts, None);
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
            let (buffers, _) = layout.lay_out(&fonts, None);
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
            let (buffers, _) = layout.lay_out(&fonts, None);
            let mut arena = SlabArena::default();
            let encoded = encode_symbol(&mut arena, GeometryId(1), &buffers, 0, true, ATLAS);

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

/// The oracle binds one texture per symbol drawable, at slot zero, and none to a fill.
///
/// Both halves matter and only together. `sh0033` is the symbol shader and every one of its
/// drawables carries exactly one `tex ... slot=0` line; the fill and background drawables in the
/// same dump carry none. A producer that bound nothing would agree with the second and fail the
/// first, and one that bound a texture to everything would do the reverse.
///
/// This is read out of the dump rather than out of the generated table, so it is an independent
/// statement of the same fact — `tessella-capture-abi`'s own tests check the table against these
/// numbers from the other side.
#[test]
fn the_oracle_binds_one_texture_to_a_symbol_and_none_to_a_fill() {
    let mut symbol_bindings = 0;
    let mut other_bindings = 0;
    for line in DUMP.lines() {
        let Some(rest) = line.trim().strip_prefix("tex ") else {
            continue;
        };
        let id = rest.split_whitespace().next().expect("a drawable id");
        let slot = rest
            .split_whitespace()
            .find_map(|token| token.strip_prefix("slot="))
            .expect("a slot");
        assert_eq!(slot, "0", "{line}");
        if id.contains("sh0033") {
            symbol_bindings += 1;
        } else {
            other_bindings += 1;
        }
    }

    assert_eq!(
        symbol_bindings,
        golden_symbols().len(),
        "one texture per symbol drawable"
    );
    assert_eq!(other_bindings, 0, "a non-symbol drawable bound a texture");
}

/// An encoded symbol drawable carries that binding, with the slot from the shader's table.
///
/// The slot is not passed in — the caller supplies an atlas and the generated table says where
/// it lands. Which is the point: a producer that remembered a number would agree with the oracle
/// today and bind the wrong sampler the moment a style used a shader with two.
#[test]
fn an_encoded_symbol_binds_its_atlas_at_the_oracle_s_slot() {
    use tessella_capture_abi::envelope::{GeometryId, TextureId, TextureRef, WireRecord};
    use tessella_orchestrate::emit::{SlabArena, encode_symbol};

    let atlas = TextureId(41);
    let buffers = tessella_layout::symbol_bucket::SymbolBuffers::default();
    let mut arena = SlabArena::default();

    for is_sdf in [true, false] {
        let encoded = encode_symbol(&mut arena, GeometryId(1), &buffers, 0, is_sdf, atlas);
        assert_eq!(encoded.record.texture_refs.count, 1, "sdf={is_sdf}");

        let size = core::mem::size_of::<TextureRef>();
        let start = encoded.record.texture_refs.offset as usize;
        let bound = TextureRef::from_bytes(&encoded.payload[start..start + size]).expect("a ref");
        assert_eq!(bound.slot, 0, "sdf={is_sdf}");
        assert_eq!(bound.texture, atlas, "sdf={is_sdf}");
    }
}

/// A shader's samplers are supplied in full or the producer is at fault.
///
/// Not a shorter list: what a shader reads from an unbound sampler is the backend's business
/// rather than a defined black, so a drawable missing one is a drawable that cannot draw. The
/// raster shader is the case that makes it concrete — it declares two and mbgl fills both with
/// the same picture.
#[test]
#[should_panic(expected = "declares 2 samplers and 1")]
fn supplying_too_few_textures_is_refused() {
    use tessella_capture_abi::BuiltIn;
    use tessella_capture_abi::envelope::TextureId;

    let _ = tessella_orchestrate::emit::texture_refs(BuiltIn::RasterShader, &[TextureId(1)]);
}

/// The glyph atlas reaches the stream as the texture the oracle describes.
///
/// `symbol_style.dump` lists three textures where the hermetic style lists two: mbgl's `0x0`
/// pattern placeholder, its `1x1` transparent image, and the glyph atlas at `512x512 fmt=1`.
/// The atlas hash is elided because its packing order is not deterministic — the dimensions and
/// the format are not, and they are on the wire.
mod atlas_texture {
    use super::through_the_builder::{built, fonts};

    use tessella_capture_abi::TexturePixelType;
    use tessella_capture_abi::envelope::TextureId;
    use tessella_glyph::fonts::ATLAS_SIZE;
    use tessella_orchestrate::texture;

    const DUMP: &str = include_str!("../../../tests/golden/symbol_style.dump");

    /// The `WxH fmt=N` the golden lists for the atlas.
    fn golden_atlas() -> (u32, u32, u8) {
        let line = DUMP
            .lines()
            .filter(|line| line.starts_with("texture "))
            // The two placeholders are 0x0 and 1x1; the atlas is the one with area.
            .find(|line| !line.contains(" 0x0 ") && !line.contains(" 1x1 "))
            .expect("the symbol capture has a glyph atlas");

        let mut parts = line.split_whitespace().skip(1);
        let size = parts.next().expect("dimensions");
        let (width, height) = size.split_once('x').expect("WxH");
        let format = line
            .split_whitespace()
            .find_map(|token| token.strip_prefix("fmt="))
            .expect("a format")
            .parse()
            .expect("a number");
        (
            width.parse().expect("a number"),
            height.parse().expect("a number"),
            format,
        )
    }

    /// The oracle lists three textures, and only the symbol capture does.
    #[test]
    fn the_symbol_capture_adds_a_texture() {
        let count: usize = DUMP
            .lines()
            .find_map(|line| line.strip_prefix("textures "))
            .expect("a texture count")
            .parse()
            .expect("a number");
        assert_eq!(count, 3, "two placeholders and the glyph atlas");
    }

    /// This build's atlas is the size and format the oracle's is.
    ///
    /// The size is not a free choice: it is on the wire, and a consumer sizing its allocation
    /// from the first upload gets a different texture from the one the oracle describes. It was
    /// picked as 2048 on a hunch before this test existed, and the oracle says 512.
    #[test]
    fn the_atlas_matches_the_oracle_s_texture() {
        let (width, height, format) = golden_atlas();
        assert_eq!((ATLAS_SIZE, ATLAS_SIZE), (width, height));
        assert_eq!(
            texture::GLYPH_ATLAS_FORMAT,
            TexturePixelType::from_repr(format).expect("a known format")
        );
        assert_eq!(
            texture::GLYPH_ATLAS_FORMAT,
            TexturePixelType::Alpha,
            "the largest texture the process keeps is single-channel (12.4)"
        );
    }

    /// A tile's glyphs produce an upload of that texture, and only once.
    ///
    /// The second half is §6.5: a settled view emits nothing. Re-uploading a quarter of a
    /// megabyte of unchanged glyphs every frame would make a still map the most expensive one.
    #[test]
    fn packing_uploads_and_settling_does_not() {
        let layouts = built();
        let mut fonts = fonts(&layouts);

        let stack = vec!["TestFont".to_string()];
        let dirty = fonts.take_dirty(&stack);
        assert!(!dirty.is_empty(), "packing the labels dirtied nothing");

        let atlas = fonts.atlas(&stack).expect("an atlas");
        let upload = texture::glyph_atlas(TextureId(1), atlas, &dirty).expect("an upload");
        assert_eq!(upload.record.size.width, ATLAS_SIZE);
        assert_eq!(upload.record.size.height, ATLAS_SIZE);
        assert_eq!(upload.record.format, texture::GLYPH_ATLAS_FORMAT as u8);

        // Every dirty rectangle is inside the texture it belongs to.
        for rect in &upload.record.rects[..upload.record.rect_count as usize] {
            assert!(
                u32::from(rect.x) + u32::from(rect.w) <= ATLAS_SIZE,
                "{rect:?} runs off the atlas"
            );
            assert!(
                u32::from(rect.y) + u32::from(rect.h) <= ATLAS_SIZE,
                "{rect:?}"
            );
        }

        // Nothing moved since, so nothing is owed.
        let settled = fonts.take_dirty(&stack);
        let atlas = fonts.atlas(&stack).expect("an atlas");
        assert!(
            texture::glyph_atlas(TextureId(1), atlas, &settled).is_none(),
            "a settled atlas still emitted an upload"
        );
    }
}

/// Painter order for a style with a symbol layer in it.
///
/// `draw_order` pins the hermetic style's forty-three entries. This is the same comparison over
/// the one style that has symbols, and it is the only place the symbol layer's *pass* and
/// *sublayer* are checked against the oracle rather than chosen. They were chosen: the layer
/// emits at sublayer 0 in the translucent pass because that is what the dump shows, and symbols
/// overhanging tile edges would make leaving the stencil off the defensible guess. The oracle
/// settles it, and nothing but this notices if it stops agreeing.
mod painter_order {
    use super::through_the_builder::style;

    use std::collections::BTreeMap;

    use tessella_capture_abi::envelope::ViewId;
    use tessella_orchestrate::order::{self, DrawOrder};
    use tessella_orchestrate::tile::{TileId as BuildTile, build_sourceless, build_tile};
    use tessella_source::geojson;
    use tessella_source::tiling::TilingOptions;
    use tessella_tile::cover::{self, ViewTransform};

    const DUMP: &str = include_str!("../../../tests/golden/symbol_style.dump");

    /// `(pass, layer, sublayer, x, y)` for one entry.
    type Slot = (u8, u32, i32, u32, u32);

    /// The camera the capture was taken at.
    fn probe() -> ViewTransform {
        ViewTransform {
            longitude: -0.11,
            latitude: 51.505,
            zoom: 13.0,
            width: 1024.0,
            height: 768.0,
            bearing: 0.0,
            pitch: 0.0,
        }
    }

    fn parse_key(key: &str) -> (u32, i32, u32, u32) {
        let mut parts = key.strip_prefix('L').expect("a layer prefix").split('.');
        let layer: u32 = parts.next().expect("layer").parse().expect("layer number");
        let sub: i32 = parts
            .next()
            .and_then(|part| part.strip_prefix('S'))
            .expect("sublayer")
            .parse()
            .expect("sublayer number");
        let mut fields = parts
            .next()
            .and_then(|tile| tile.strip_prefix('t'))
            .expect("a tile field")
            .split('_');
        fields.next();
        let x: u32 = fields.next().expect("x").parse().expect("x number");
        let y: u32 = fields.next().expect("y").parse().expect("y number");
        (layer, sub, x, y)
    }

    fn oracle_order() -> Vec<Slot> {
        DUMP.lines()
            .filter_map(|line| line.strip_prefix("draw "))
            .map(|rest| {
                let mut fields = rest.split(' ');
                fields.next();
                let key = fields.next().expect("a drawable key");
                let pass: u8 = fields
                    .next()
                    .and_then(|field| field.strip_prefix("pass="))
                    .expect("a pass")
                    .parse()
                    .expect("pass number");
                let (layer, sub, x, y) = parse_key(key);
                (pass, layer, sub, x, y)
            })
            .collect()
    }

    fn resolved_order() -> Vec<Slot> {
        let style = style();
        let Some(tessella_style::Source::Geojson(source)) = style.source("probe") else {
            panic!("a geojson source");
        };
        let features = geojson::read(&source.data).expect("features");

        #[allow(clippy::cast_possible_truncation)]
        let mut order = DrawOrder::new(style.layers.len() as u32);
        let mut next_id = 0;
        let mut tile_of_geometry = BTreeMap::new();

        for tile in cover::cover(&probe()).expect("covers") {
            let at = BuildTile::new(tile.z, tile.x, tile.y);
            let mut buckets = build_tile(&style, "probe", at, &features, TilingOptions::default())
                .expect("tile builds");
            buckets.extend(build_sourceless(&style, at).expect("background builds"));
            buckets.sort_by_key(|bucket| bucket.layer_index);

            for binding in order::bindings_for(
                ViewId(0),
                order::tile_of(tile.z, tile.x, tile.y),
                &buckets,
                &mut next_id,
            ) {
                tile_of_geometry.insert(binding.geometry.0, (tile.x, tile.y));
                order.bind(binding);
            }
        }

        order
            .resolve()
            .iter()
            .map(|entry| {
                let (x, y) = tile_of_geometry[&entry.geometry.0];
                (
                    entry.pass.bits(),
                    entry.layer_index,
                    entry.sub_layer_index,
                    x,
                    y,
                )
            })
            .collect()
    }

    /// The symbol style's draw order is the oracle's, entry for entry.
    #[test]
    fn the_symbol_style_draws_in_the_oracle_s_order() {
        let oracle = oracle_order();
        assert_eq!(oracle.len(), 14, "the capture's order section");
        assert_eq!(resolved_order(), oracle, "draw order diverges");
    }

    /// The symbol layer draws once, in the translucent pass, above the background.
    ///
    /// Stated separately because the whole-order comparison would also pass if both were wrong
    /// in the same way. The background is `Opaque | Translucent` and is genuinely drawn twice;
    /// the symbol layer is translucent only, and it precedes the background's second pass —
    /// which is mbgl ordering by a depth slot that runs opposite the style index.
    #[test]
    fn the_symbol_layer_draws_once_above_the_background() {
        let order = resolved_order();
        let symbols: Vec<&Slot> = order.iter().filter(|slot| slot.1 == 1).collect();
        assert_eq!(symbols.len(), 2, "one per tile that carries labels");
        assert!(
            symbols.iter().all(|slot| slot.0 == 2 && slot.2 == 0),
            "{symbols:?} is not translucent at sublayer 0"
        );

        let first_symbol = order.iter().position(|slot| slot.1 == 1).expect("a symbol");
        // Layer 0 in the key is the style's first layer, the background. Note that is *not* the
        // `layer=` field of a draw line: that is mbgl's depth slot, which runs opposite the
        // style index, and reading it as the style index would have the background on top.
        let last_background = order
            .iter()
            .rposition(|slot| slot.1 == 0)
            .expect("a background");
        assert!(
            first_symbol < last_background,
            "the background's translucent pass drew over the labels"
        );
    }
}

/// The symbol layer's uniform buffers, byte for byte.
///
/// The symbol capture has six: the global paint params, the background layer's two, and the
/// symbol layer's three — the per-drawable array at slot 2, the tile props at slot 3 and the
/// evaluated props at slot 5. The slot numbers are mbgl's own, generated into
/// `ubo_slots.rs`, and the sizes follow from the layouts generated beside them.
///
/// Two of the three are here. The drawable array carries three matrices per entry — the tile
/// matrix, the label-plane matrix and the GL coordinate matrix — and those are the projection
/// stage, not a paint one; they are their own piece.
mod symbol_ubos {
    use std::collections::BTreeMap;

    use tessella_capture_abi::generated::ubo_layouts::{
        SYMBOL_DRAWABLE_UBO, SYMBOL_EVALUATED_PROPS_UBO, SYMBOL_TILE_PROPS_UBO,
    };
    use tessella_capture_abi::generated::ubo_slots::{
        ID_SYMBOL_DRAWABLE_UBO, ID_SYMBOL_EVALUATED_PROPS_UBO, ID_SYMBOL_TILE_PROPS_UBO,
    };
    use tessella_orchestrate::ubo;
    use tessella_style::property::resolve_paint;

    const DUMP: &str = include_str!("../../../tests/golden/symbol_style.dump");

    /// The oracle's buffers, keyed by layer and slot: the size and the sorted 16-byte blocks.
    pub(super) fn oracle() -> BTreeMap<(i32, u32), (usize, Vec<String>)> {
        let mut out = BTreeMap::new();
        for line in DUMP.lines() {
            let Some(rest) = line.strip_prefix("ubo ") else {
                continue;
            };
            let mut fields = rest.split(' ');
            let key = fields.next().expect("a key");
            let (kind, index) = key.split_once(':').expect("kind:index");
            let layer = if kind == "global" {
                -1
            } else {
                index.parse::<i32>().expect("layer number")
            };
            let slot: u32 = fields
                .next()
                .and_then(|field| field.strip_prefix("slot="))
                .expect("a slot")
                .parse()
                .expect("slot number");
            let size: usize = fields
                .next()
                .and_then(|field| field.strip_prefix("size="))
                .expect("a size")
                .parse()
                .expect("size number");
            let bytes = fields
                .next()
                .and_then(|field| field.strip_prefix("bytes="))
                .expect("bytes");

            let mut blocks: Vec<String> = bytes
                .as_bytes()
                .chunks(32)
                .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
                .collect();
            blocks.sort();
            out.insert((layer, slot), (size, blocks));
        }
        out
    }

    pub(super) fn blocks(bytes: &[u8]) -> Vec<String> {
        let mut blocks: Vec<String> = bytes
            .chunks(16)
            .map(|chunk| chunk.iter().map(|byte| format!("{byte:02x}")).collect())
            .collect();
        blocks.sort();
        blocks
    }

    /// The slots and sizes are the generated tables', and the oracle agrees with both.
    ///
    /// A slot is where the shader looks: writing the evaluated props into the tile-props slot
    /// binds one buffer where the other belongs, which draws nothing recognizable and is not an
    /// error anywhere. The tables come from mbgl (DR-6), so this is the tables checked against a
    /// capture of the code they were generated from.
    #[test]
    fn the_slots_and_sizes_are_the_generated_ones() {
        let oracle = oracle();
        let drawables = 2;

        assert_eq!(
            oracle[&(1, ID_SYMBOL_DRAWABLE_UBO)].0,
            SYMBOL_DRAWABLE_UBO.stride as usize * drawables,
            "the drawable array is one padded entry per drawable"
        );
        assert_eq!(
            oracle[&(1, ID_SYMBOL_TILE_PROPS_UBO)].0,
            SYMBOL_TILE_PROPS_UBO.stride as usize * drawables
        );
        assert_eq!(
            oracle[&(1, ID_SYMBOL_EVALUATED_PROPS_UBO)].0,
            SYMBOL_EVALUATED_PROPS_UBO.size as usize,
            "the evaluated props are one buffer for the layer, not one per drawable"
        );
    }

    /// The tile props buffer matches the oracle's.
    #[test]
    fn the_tile_props_match_the_oracle() {
        let (size, expected) = &oracle()[&(1, ID_SYMBOL_TILE_PROPS_UBO)];
        // Text, no halo, gamma one at pitch zero — two drawables, both the same.
        let mine = ubo::pack_symbol_tile_props(2, true, false, 1.0);
        assert_eq!(mine.len(), *size);
        assert_eq!(&blocks(&mine), expected);
    }

    /// The evaluated props buffer matches the oracle's.
    ///
    /// Derived from the style's paint rather than written out, so it is the resolution being
    /// checked and not a transcription of the dump. The style names only `text-color`; every
    /// other value here is a spec default, and the icon half is the half that catches a
    /// zero-filled shortcut — `icon-color` defaults to opaque black, not to nothing.
    #[test]
    fn the_evaluated_props_match_the_oracle() {
        let style = super::through_the_builder::style();
        let layer = style
            .layers
            .iter()
            .find(|layer| layer.id == "labels")
            .expect("the symbol layer");
        let paint = resolve_paint(layer).expect("paint resolves");

        let (size, expected) = &oracle()[&(1, ID_SYMBOL_EVALUATED_PROPS_UBO)];
        let mine = ubo::symbol_props_from_paint(&paint, 13.0);
        assert_eq!(mine.len(), *size);
        assert_eq!(&blocks(&mine), expected);
    }
}

/// The symbol layer's per-drawable buffer, byte for byte.
///
/// The third and last of the capture's symbol buffers, and the one that is not a paint buffer:
/// three matrices per entry, in three different spaces. It is where the label-plane and GL
/// coordinate matrices are checked at all — nothing else in this build produces them, and a
/// point label draws correctly with either one wrong, because the walk between them is what a
/// *line* label needs.
mod symbol_drawable_ubo {
    use tessella_capture_abi::generated::ubo_layouts::SYMBOL_DRAWABLE_UBO;
    use tessella_layout::symbol_layout::{Alignment, Alignments, Placement};
    use tessella_orchestrate::ubo::{self, SymbolDrawableEntry};
    use tessella_tile::cover::ViewTransform;

    use super::symbol_ubos::{blocks, oracle};

    fn probe() -> ViewTransform {
        ViewTransform {
            longitude: -0.11,
            latitude: 51.505,
            zoom: 13.0,
            width: 1024.0,
            height: 768.0,
            bearing: 0.0,
            pitch: 0.0,
        }
    }

    /// The two tiles the golden's symbol drawables sit on, in the order it lists them.
    const TILES: [(u32, u32); 2] = [(4093, 2723), (4093, 2724)];

    /// Both alignments as the capture's style resolves them.
    const VIEWPORT: Alignments = Alignments {
        rotation: Alignment::Viewport,
        pitch: Alignment::Viewport,
    };

    #[test]
    fn the_drawable_buffer_matches_the_oracle() {
        let view = probe();
        let entries: Vec<SymbolDrawableEntry> = TILES
            .iter()
            .map(|(x, y)| {
                SymbolDrawableEntry::for_tile(
                    &view,
                    13,
                    *x,
                    *y,
                    0,
                    // The symbol layer is style index 1, sublayer 0.
                    1,
                    0,
                    [512.0, 512.0],
                    // The capture's style has no sprite, so the icon texture is unbound and its
                    // size is zero — which is a value the oracle carries rather than a
                    // placeholder, and is why it is passed rather than defaulted.
                    [0.0, 0.0],
                    16.0,
                    // The capture's style names neither alignment, and its placement is point,
                    // so `auto` resolves to viewport for both — which is the branch the golden
                    // pins and the reason it still holds now the other exists.
                    VIEWPORT,
                    Placement::Point,
                )
                .expect("the probe has a viewport")
            })
            .collect();

        let mine = ubo::pack_symbol_drawable_buffer(&entries, SYMBOL_DRAWABLE_UBO.stride);
        let (size, expected) = &oracle()[&(1, 2)];
        assert_eq!(mine.len(), *size, "the array is one stride per drawable");
        assert_eq!(&blocks(&mine), expected);
    }

    /// The coordinate matrix carries no tile, so both entries share it.
    ///
    /// Stated separately because the buffer comparison sorts its blocks and would pass if the
    /// two matrices were swapped between entries. This one is the viewport's alone — half the
    /// viewport on each axis with y flipped — and a version that folded in the tile would still
    /// draw a point label correctly.
    #[test]
    fn the_coordinate_matrix_is_the_viewport_s_alone() {
        let view = probe();
        let first = SymbolDrawableEntry::for_tile(
            &view,
            13,
            4093,
            2723,
            0,
            1,
            0,
            [512.0, 512.0],
            [0.0, 0.0],
            16.0,
            VIEWPORT,
            Placement::Point,
        )
        .expect("a viewport");
        let second = SymbolDrawableEntry::for_tile(
            &view,
            13,
            4093,
            2724,
            0,
            1,
            0,
            [512.0, 512.0],
            [0.0, 0.0],
            16.0,
            VIEWPORT,
            Placement::Point,
        )
        .expect("a viewport");

        assert_eq!(first.coord_matrix, second.coord_matrix);
        assert_ne!(
            first.matrix, second.matrix,
            "two different tiles share a placement matrix"
        );
        assert_ne!(
            first.label_plane_matrix, second.label_plane_matrix,
            "the label plane does not follow the tile"
        );

        // Two over the viewport on each axis, y negated. Written out because it is the one
        // matrix here with no camera in it, so a wrong one is a constant that is simply wrong.
        assert!((first.coord_matrix[0] - 2.0 / 1024.0).abs() < 1e-9);
        assert!((first.coord_matrix[5] + 2.0 / 768.0).abs() < 1e-9);
        assert_eq!(
            [
                first.coord_matrix[12],
                first.coord_matrix[13],
                first.coord_matrix[15]
            ],
            [-1.0, 1.0, 1.0]
        );
    }
}

/// The glyph vertex buffer's geometry half is byte-identical to the oracle's.
///
/// # What this could not check before
///
/// Three attributes share one buffer and only the middle one carries texture coordinates. The
/// dump hashed the buffer, so eliding the atlas-dependent part meant eliding all of it — and the
/// two attributes that describe *geometry* went unchecked, on a capture whose whole purpose is
/// checking geometry. Every other test here compares a buffer the symbol pipeline derives; this
/// compares the one it builds.
///
/// The probe emits a per-attribute hash now, so attribute 0 — each glyph's tile position and its
/// label's anchor — is comparable. Its bytes are the first eight of every twenty-four.
#[test]
fn the_glyph_layout_buffer_matches_the_oracle() {
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
                pending: 0,
                sections: vec![tessella_layout::symbol::Section { text: (*text).to_string(), scale: 1.0 }],
                text: (*text).to_string(),
                anchor: *anchor,
            })
            .collect();
        let buffers = build_symbols(&entries, &font, &SymbolOptions::default()).0;

        // The attribute's own bytes, gathered as the probe gathers them: `pos_offset` is the
        // first eight of every twenty-four.
        let mut field = Vec::with_capacity(buffers.vertices.len() * 8);
        for vertex in &buffers.vertices {
            for value in vertex.pos_offset {
                field.extend_from_slice(&value.to_le_bytes());
            }
        }

        assert_eq!(
            fnv1a(&field),
            symbol.layout_hash,
            "tile {:?}: the glyph positions and anchors differ from the oracle's",
            symbol.tile
        );
        compared += 1;
    }
    assert_eq!(compared, 2, "both tiles were compared");
}
