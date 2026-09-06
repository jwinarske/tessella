//! What `resolve_paint` makes of liberty's data-driven building height.

use tessella_style::document::Style;
use tessella_style::property::resolve_paint;

#[test]
fn building_3d_height_resolves_to_the_features_own_value() {
    let source = std::fs::read_to_string(
        "/mnt/dev/maplibre-frontend/maplibre-native/benchmark/fixtures/renderer/liberty.json",
    )
    .expect("liberty");
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
