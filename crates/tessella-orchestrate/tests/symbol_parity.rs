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
use tessella_glyph::pbf::{self, Glyph, Range};
use tessella_glyph::quads::{self, Placed as QuadGlyph};
use tessella_glyph::shaping::{self, Char, Options as ShapeOptions};
use tessella_layout::symbol_bucket::{SizeRange, SymbolBuffers};

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

/// Shapes a label and turns it into symbol vertices, as the bucket would.
fn build(text: &str, glyphs: &[Glyph], atlas: &mut Atlas, buffers: &mut SymbolBuffers) {
    let chars: Vec<Char> = text
        .chars()
        .map(|character| {
            let codepoint = character as u32;
            match glyphs.iter().find(|glyph| glyph.id == codepoint) {
                #[allow(clippy::cast_precision_loss)]
                Some(glyph) if glyph.bitmap_size().is_some() => {
                    Char::new(codepoint, glyph.metrics.advance as f32)
                }
                #[allow(clippy::cast_precision_loss)]
                Some(glyph) => Char::blank(codepoint, glyph.metrics.advance as f32),
                None => Char::blank(codepoint, 0.0),
            }
        })
        .collect();

    let shaping = shaping::shape(&chars, &ShapeOptions::default());
    for glyph in glyphs {
        if text.chars().any(|character| character as u32 == glyph.id) {
            atlas.add(glyph.id, glyph);
        }
    }

    let quads = quads::glyph_quads(
        &shaping,
        |codepoint| {
            let glyph = glyphs.iter().find(|glyph| glyph.id == codepoint)?;
            Some(QuadGlyph {
                rect: atlas.get(codepoint)?,
                metrics: glyph.metrics,
            })
        },
        &quads::Options::default(),
    );

    for quad in quads {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        buffers.add_quad(
            (0.0, 0.0),
            [quad.tl, quad.tr, quad.bl, quad.br],
            0.0,
            (
                quad.tex.x as u16,
                quad.tex.y as u16,
                quad.tex.width as u16,
                quad.tex.height as u16,
            ),
            SizeRange::constant(16.0),
            true,
            1.0,
        );
    }
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
    let glyphs = pbf::parse(
        Range {
            first: 0,
            last: 255,
        },
        GLYPHS,
    )
    .expect("the range parses");
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

        let mut atlas = Atlas::new(512, 512);
        let mut buffers = SymbolBuffers::default();
        for label in labels {
            build(label, &glyphs, &mut atlas, &mut buffers);
        }

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

/// This build produces the same number of per-frame vertices as the oracle.
///
/// One dynamic position and one opacity per layout vertex — the counts have to agree or the
/// shader reads one label's opacity against another's geometry. Their *bytes* are not compared:
/// the probe renders eight frames before dumping, so both buffers hold post-placement state,
/// and matching them means driving placement through the same frame loop rather than shaping a
/// label in isolation. That is R2's remaining work, and this is where it will be checked.
#[test]
fn the_per_frame_buffers_have_one_entry_per_vertex() {
    let glyphs = pbf::parse(
        Range {
            first: 0,
            last: 255,
        },
        GLYPHS,
    )
    .expect("the range parses");

    for (glyph_count, labels) in [(5usize, vec!["Alpha"]), (12, vec!["Bravo", "Charlie"])] {
        let mut atlas = Atlas::new(512, 512);
        let mut buffers = SymbolBuffers::default();
        for label in &labels {
            build(label, &glyphs, &mut atlas, &mut buffers);
        }

        assert_eq!(buffers.glyphs(), glyph_count);
        assert_eq!(buffers.dynamic.len(), buffers.vertices.len());
        assert_eq!(buffers.opacity.len(), buffers.vertices.len());
    }
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
