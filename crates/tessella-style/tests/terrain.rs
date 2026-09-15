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
