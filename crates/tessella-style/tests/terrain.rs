// SPDX-License-Identifier: BSD-2-Clause
//! The style's `terrain` member.
//!
//! Two properties, and the reason this has a test file of its own is that there is no oracle to
//! catch a mistake in it later. maplibre-native has no terrain at all -- no parser member, no
//! `setTerrain`, and a style carrying one renders identically to a style without -- so the spec
//! is the whole of the authority and the defaults are worth pinning where they can be read.

use tessella_style::Style;

fn style(terrain: &str) -> Style {
    let text = format!(
        r#"{{"version":8,"sources":{{"dem":{{"type":"raster-dem","url":"http://x/d.json"}}}},
           "terrain":{terrain},"layers":[]}}"#
    );
    Style::parse(&text).expect("style parses")
}

/// The spec's two properties, with the spec's default for the one that has one.
#[test]
fn a_terrain_is_a_source_and_an_exaggeration() {
    let both = style(r#"{"source":"dem","exaggeration":1.5}"#);
    let terrain = both.terrain.as_ref().expect("a terrain");
    assert_eq!(terrain.source, "dem");
    assert!((terrain.exaggeration() - 1.5).abs() < f64::EPSILON);

    // Absent is one, which is the spec's default and not a sentinel for "unset".
    let bare = style(r#"{"source":"dem"}"#);
    let terrain = bare.terrain.as_ref().expect("a terrain");
    assert!((terrain.exaggeration() - 1.0).abs() < f64::EPSILON);
}

/// A style with no terrain has none, which is not the same as one exaggerated to nothing.
///
/// Zero flattens the surface and keeps the draping; absent keeps the map flat and draped over
/// nothing at all. A build that collapsed the two would have no way to animate an exaggeration
/// down to zero without the layer set changing under it.
#[test]
fn no_terrain_is_not_a_flat_terrain() {
    let none = Style::parse(r#"{"version":8,"sources":{},"layers":[]}"#).expect("style parses");
    assert!(none.terrain.is_none());

    let flat = style(r#"{"source":"dem","exaggeration":0}"#);
    let terrain = flat.terrain.as_ref().expect("a terrain");
    assert_eq!(terrain.exaggeration(), 0.0);
}

/// The spec gives exaggeration a minimum of zero. Below it is clamped rather than mirrored.
///
/// A negative exaggeration is not upside-down terrain, it is a number the property has no
/// meaning for -- and the rest of the style is still a map, so it is not a reason to refuse one.
#[test]
fn a_value_outside_the_spec_is_clamped() {
    assert_eq!(
        style(r#"{"source":"dem","exaggeration":-2}"#)
            .terrain
            .expect("a terrain")
            .exaggeration(),
        0.0
    );
    // A non-finite one falls back to the default rather than poisoning every elevation with a
    // NaN. It cannot arrive from JSON -- serde refuses `1e999` as out of range before this sees
    // it -- so the guard is for a `Terrain` built in code, which is how a runtime API sets one.
    let infinite = tessella_style::Terrain {
        source: "dem".into(),
        exaggeration: f64::INFINITY,
    };
    assert!((infinite.exaggeration() - 1.0).abs() < f64::EPSILON);
    let nan = tessella_style::Terrain {
        source: "dem".into(),
        exaggeration: f64::NAN,
    };
    assert!((nan.exaggeration() - 1.0).abs() < f64::EPSILON);
}

/// A usable terrain is one whose source is declared and is a `raster-dem`.
#[test]
fn a_terrain_resolves_to_its_dem() {
    let usable = style(r#"{"source":"dem","exaggeration":1}"#);
    let (id, tiles) = usable.terrain_dem().expect("a usable terrain");
    assert_eq!(id, "dem");
    assert_eq!(tiles.url.as_deref(), Some("http://x/d.json"));
}

/// And one that names something else is inert rather than an error.
///
/// The map draws flat. There is no elevation to read, and inventing one -- treating a vector
/// source as a DEM, or defaulting to sea level and drawing a terrain that is not there -- is
/// worse than drawing the map the rest of the style describes.
#[test]
fn a_terrain_naming_the_wrong_thing_is_inert() {
    for terrain in [
        // A source the style does not declare.
        r#"{"source":"missing"}"#,
        // One it declares, that is not a DEM.
        r#"{"source":"vector"}"#,
    ] {
        let text = format!(
            r#"{{"version":8,"sources":{{"vector":{{"type":"vector","url":"http://x/v.json"}}}},
               "terrain":{terrain},"layers":[]}}"#
        );
        let parsed = Style::parse(&text).expect("style parses");
        // The member is kept -- a round trip is lossless -- and it resolves to nothing.
        assert!(parsed.terrain.is_some(), "{terrain}");
        assert!(parsed.terrain_dem().is_none(), "{terrain}");
    }
    // And a style with no terrain at all resolves to nothing either.
    let none = Style::parse(r#"{"version":8,"sources":{},"layers":[]}"#).expect("style parses");
    assert!(none.terrain_dem().is_none());
}

/// A usable terrain becomes a layer over its DEM, first in the list.
///
/// The spec puts terrain at the top level rather than in the layer list. Synthesizing a layer is
/// what lets everything downstream treat the ground as what it is -- an index, a bucket per tile,
/// a place in painter order, a uniform block -- instead of branching for the one thing that is
/// drawn and is not a layer. `Annotations::synthesize` settled the same question the same way.
#[test]
fn a_terrain_becomes_a_layer() {
    use tessella_style::{LayerKind, TERRAIN_LAYER_ID};
    let mut style = style(r#"{"source":"dem","exaggeration":1.5}"#);
    assert!(style.layers.is_empty());
    style.synthesize_terrain();

    assert_eq!(style.layers.len(), 1);
    let layer = &style.layers[0];
    assert_eq!(layer.id, TERRAIN_LAYER_ID);
    assert_eq!(layer.kind, LayerKind::Terrain);
    assert_eq!(layer.source.as_deref(), Some("dem"));
    // First, because it is the ground and everything else is drawn on it.
    assert_eq!(
        style.layers.first().map(|l| l.id.as_str()),
        Some(TERRAIN_LAYER_ID)
    );
}

/// It is idempotent, and it takes the layer away when the terrain does.
///
/// A style whose terrain changed -- or was removed -- must not keep the layer the last one made.
#[test]
fn synthesizing_twice_makes_one_layer() {
    use tessella_style::TERRAIN_LAYER_ID;
    let mut style = style(r#"{"source":"dem"}"#);
    style.synthesize_terrain();
    style.synthesize_terrain();
    style.synthesize_terrain();
    assert_eq!(style.layers.len(), 1);

    style.terrain = None;
    style.synthesize_terrain();
    assert!(style.layers.is_empty());

    // And an inert terrain makes none either: there is no DEM behind it to read.
    let mut inert =
        Style::parse(r#"{"version":8,"sources":{},"terrain":{"source":"gone"},"layers":[]}"#)
            .expect("style parses");
    inert.synthesize_terrain();
    assert!(
        inert
            .layers
            .iter()
            .all(|layer| layer.id != TERRAIN_LAYER_ID)
    );
}

/// The synthesized layer goes under the style's own, whatever they are.
#[test]
fn the_ground_is_under_everything() {
    use tessella_style::TERRAIN_LAYER_ID;
    let text = r##"{"version":8,
      "sources":{"dem":{"type":"raster-dem","url":"http://x/d.json"}},
      "terrain":{"source":"dem"},
      "layers":[{"id":"bg","type":"background","paint":{"background-color":"#101014"}},
                {"id":"water","type":"fill","source":"dem"}]}"##;
    let mut style = Style::parse(text).expect("style parses");
    style.synthesize_terrain();
    let ids: Vec<&str> = style.layers.iter().map(|layer| layer.id.as_str()).collect();
    assert_eq!(ids, [TERRAIN_LAYER_ID, "bg", "water"]);
}

/// It survives a round trip, so a style read and written back still asks for its terrain.
#[test]
fn a_terrain_round_trips() {
    let original = style(r#"{"source":"dem","exaggeration":2.25}"#);
    let text = serde_json::to_string(&original).expect("serializes");
    let again = Style::parse(&text).expect("re-parses");
    assert_eq!(original.terrain, again.terrain);
    // And a style without one does not grow an empty member on the way out.
    let none = Style::parse(r#"{"version":8,"sources":{},"layers":[]}"#).expect("style parses");
    let text = serde_json::to_string(&none).expect("serializes");
    assert!(!text.contains("terrain"), "{text}");
}
