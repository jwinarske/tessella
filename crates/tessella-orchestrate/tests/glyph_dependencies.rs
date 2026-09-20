//! Working out which glyphs a tile needs, before anything is shaped.
//!
//! The connection between the decoder and the glyph manager. What a tile needs is not a property
//! of the style but of the *data*: a style naming one font stack needs a handful of ranges over
//! Iceland and hundreds over Japan, and the only way to know is to look at the labels.
//!
//! Collected in one pass before shaping, because shaping needs advances, advances need glyphs,
//! and glyphs cross the network. Discovering a missing glyph mid-shape turns one round trip per
//! tile into one per label.

use std::collections::BTreeSet;

use tessella_glyph::manager::{FontStack, GlyphManager};
use tessella_glyph::pbf::Range;
use tessella_layout::symbol::glyph_dependencies;
use tessella_source::mvt::{GeomType, Tile};
use tessella_style::Layer;
use tessella_style::expression::Feature;

const TILE: &[u8] = include_bytes!("../../../tests/live-fixtures/world_z7-5-16-11.mvt");

fn places_layer(font: &str) -> Layer {
    serde_json::from_str(&format!(
        r#"{{"id": "labels", "type": "symbol", "source": "v", "source-layer": "places",
             "layout": {{"text-field": "{{name}}", "text-font": ["{font}"]}}}}"#
    ))
    .expect("a symbol layer")
}

/// The fixture's point features, as expression features.
fn place_features(tile: &Tile) -> Vec<&dyn Feature> {
    let layer = tile
        .layers
        .iter()
        .find(|layer| layer.name == "places")
        .expect("a places layer");
    layer
        .features()
        .filter(|feature| feature.geom_type() == GeomType::Point)
        .map(|feature| Box::leak(Box::new(feature)) as &dyn Feature)
        .collect()
}

/// A real tile's labels ask for the codepoints they actually contain.
#[test]
fn a_tile_asks_for_the_glyphs_its_labels_use() {
    let tile = Tile::decode(TILE).expect("the fixture decodes");
    let features = place_features(&tile);
    let layer = places_layer("TestFont");

    let deps = glyph_dependencies([&layer], 5.0, &features, |_| true);

    assert_eq!(deps.len(), 1, "one font stack");
    let (stack, codepoints) = deps.iter().next().expect("a stack");
    assert_eq!(stack, &vec!["TestFont".to_string()]);

    // The tile's place names are European: Latin letters, accents, a slash and spaces.
    assert!(codepoints.contains(&u32::from(b'P')), "Paris");
    assert!(
        codepoints.contains(&u32::from(b' ')),
        "San Marino has a space"
    );
    assert!(codepoints.contains(&0x00e9), "Orléans has an e-acute");
    assert!(codepoints.contains(&u32::from(b'/')), "Schweiz/Suisse");
    assert!(
        codepoints.len() > 30,
        "only {} codepoints",
        codepoints.len()
    );

    // And it asks for nothing it does not use: this tile has no Cyrillic or CJK.
    assert!(!codepoints.iter().any(|codepoint| *codepoint > 0x0500));
}

/// Two font stacks are two sets of dependencies.
///
/// A style commonly sets one font for cities and another for countries. Merging them would make
/// the manager fetch every codepoint in both faces, which is a doubling that never shows up as
/// an error.
#[test]
fn two_stacks_ask_separately() {
    let tile = Tile::decode(TILE).expect("the fixture decodes");
    let features = place_features(&tile);
    let regular = places_layer("TestFont Regular");
    let bold = places_layer("TestFont Bold");

    let deps = glyph_dependencies([&regular, &bold], 5.0, &features, |_| true);

    assert_eq!(deps.len(), 2);
    let stacks: Vec<&Vec<String>> = deps.keys().collect();
    assert_ne!(stacks[0], stacks[1]);
}

/// A layer the predicate rejects contributes nothing.
///
/// Which layers read a source is the tile builder's question, and it is passed in rather than
/// guessed at here — a layer over a different source would otherwise make a tile fetch fonts
/// for labels it never draws.
#[test]
fn a_layer_that_does_not_draw_from_the_source_is_skipped() {
    let tile = Tile::decode(TILE).expect("the fixture decodes");
    let features = place_features(&tile);
    let layer = places_layer("TestFont");

    let deps = glyph_dependencies([&layer], 5.0, &features, |_| false);
    assert!(deps.is_empty());
}

/// Features without labels ask for nothing.
#[test]
fn unlabeled_features_ask_for_nothing() {
    let tile = Tile::decode(TILE).expect("the fixture decodes");
    let layer = tile
        .layers
        .iter()
        .find(|layer| layer.name == "water")
        .expect("a water layer");
    let features: Vec<&dyn Feature> = layer
        .features()
        .map(|feature| Box::leak(Box::new(feature)) as &dyn Feature)
        .collect();

    let deps = glyph_dependencies([&places_layer("TestFont")], 5.0, &features, |_| true);
    assert!(deps.is_empty(), "water has no names");
}

/// The dependencies collapse into the ranges the manager actually fetches.
///
/// The point of collecting them: thirty-odd distinct codepoints across seventy-five labels are
/// one range file, so a tile of European place names costs one request rather than one per
/// label — which is what discovering glyphs during shaping would cost.
#[test]
fn a_tile_of_labels_collapses_into_few_ranges() {
    let tile = Tile::decode(TILE).expect("the fixture decodes");
    let features = place_features(&tile);
    let deps = glyph_dependencies([&places_layer("TestFont")], 5.0, &features, |_| true);

    let (_, codepoints) = deps.iter().next().expect("a stack");
    let ranges: BTreeSet<Range> = codepoints.iter().filter_map(|c| Range::of(*c)).collect();

    assert!(
        ranges.len() <= 2,
        "{} codepoints fell into {} ranges: {ranges:?}",
        codepoints.len(),
        ranges.len()
    );
    assert!(
        ranges.contains(&Range {
            first: 0,
            last: 255
        }),
        "{ranges:?}"
    );
}

/// What the manager is asked for is exactly what was collected.
///
/// The two sides meeting: the layout side names a stack as a list of fonts, and the manager
/// wants a `FontStack` whose name becomes the URL. A mismatch here is a request for a font
/// nobody named.
#[test]
fn the_manager_takes_what_was_collected() {
    let tile = Tile::decode(TILE).expect("the fixture decodes");
    let features = place_features(&tile);
    let deps = glyph_dependencies([&places_layer("TestFont")], 5.0, &features, |_| true);

    let manager = GlyphManager::new("https://example.com/fonts/{fontstack}/{range}.pbf");
    for (fonts, codepoints) in &deps {
        let stack = FontStack(fonts.clone());
        let owed = manager.owed(&stack, codepoints.iter().copied());

        assert!(!owed.is_empty(), "a fresh manager owes every range");
        assert_eq!(
            manager.url_for(&stack, owed[0]),
            "https://example.com/fonts/TestFont/0-255.pbf"
        );
    }
}

/// A `format` section that names its own stack asks for its glyphs under that stack.
///
/// The spec's own rich-text example is a two-line label: the English name in the layer's face,
/// then the local name in another. A section's `text-font` overrides the layer's for that run
/// alone, so the glyphs it draws are in a different atlas -- and asking for them under the
/// layer's stack fetches a range the section will never draw from while leaving the one it will
/// unfetched.
#[test]
fn a_section_asks_under_the_font_it_names() {
    let tile = Tile::decode(TILE).expect("the fixture decodes");
    let features = place_features(&tile);
    let layer: Layer = serde_json::from_str(
        r#"{"id": "labels", "type": "symbol", "source": "v", "source-layer": "places",
            "layout": {"text-font": ["Layer Face"], "text-field":
              ["format",
                ["get", "name"], {"font-scale": 1.2},
                "\n", {},
                ["get", "name"], {"font-scale": 0.8,
                                  "text-font": ["literal", ["Section Face"]]}]}}"#,
    )
    .expect("a symbol layer");

    let deps = glyph_dependencies([&layer], 5.0, &features, |_| true);

    let stacks: Vec<&Vec<String>> = deps.keys().collect();
    assert!(
        stacks
            .iter()
            .any(|stack| stack.as_slice() == ["Section Face".to_string()]),
        "the section's own stack was never asked for: {stacks:?}"
    );
    assert!(
        stacks
            .iter()
            .any(|stack| stack.as_slice() == ["Layer Face".to_string()]),
        "the layer's stack still answers for the sections that name none: {stacks:?}"
    );

    // And each carries the codepoints of its own run rather than the whole label's, which is
    // the same set here because both sections draw the same property.
    let section = deps
        .get(&vec!["Section Face".to_string()])
        .expect("the section's stack");
    assert!(
        !section.is_empty(),
        "the section's stack asked for no glyphs"
    );
}
