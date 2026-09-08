//! What `resolve_paint` makes of liberty's data-driven building height.

use tessella_style::document::Style;
use tessella_style::property::resolve_paint;

/// liberty, if this machine has a maplibre-native checkout to read it from.
///
/// The style is one of mbgl's benchmark fixtures rather than anything this repository ships, and it
/// is not vendored: what may be redistributed here is what maplibre-native itself carries, and that
/// question is settled per file rather than by assumption. So the test reads it where a developer
/// has it and skips where nobody does -- which is every CI runner, and is why this suite failed on
/// every push for over a week while passing for everyone who ran it locally.
///
/// `pmtiles_archive.rs` answers the same question the same way. This one did not, and was the only
/// unguarded fixture test that was not also `#[ignore]`d.
fn liberty() -> Option<String> {
    std::fs::read_to_string(
        "/mnt/dev/maplibre-frontend/maplibre-native/benchmark/fixtures/renderer/liberty.json",
    )
    .ok()
}

#[test]
fn building_3d_height_resolves_to_the_features_own_value() {
    let Some(source) = liberty() else {
        eprintln!("skipped: no maplibre-native checkout to read liberty.json from");
        return;
    };
    let style = Style::parse(&source).expect("parse");
    let layer = style
        .layers
        .iter()
        .find(|layer| layer.id == "building-3d")
        .expect("building-3d");

    let paint = resolve_paint(layer).expect("resolve");
    for name in [
        "fill-extrusion-height",
        "fill-extrusion-base",
        "fill-extrusion-color",
    ] {
        let resolved = paint.get(name).expect("a resolved property");
        println!("{name}: binding {:?}", resolved.binding);
        println!("   expression {:?}", resolved.expression);
    }
}
