//! What a real building feature carries, straight out of a z14 tile.

use tessella_source::mvt::Tile;

#[test]
#[ignore = "reads a tile fetched out of band"]
fn building_features_carry_render_height() {
    let path = std::env::var("TSL_TILE").expect("TSL_TILE");
    let bytes = std::fs::read(&path).expect("tile bytes");
    let tile = Tile::decode(&bytes).expect("decode");

    let layer = tile.layer("building").expect("a building layer");
    println!(
        "building: {} features, extent {}",
        layer.len(),
        layer.extent
    );
    for index in 0..layer.len().min(4) {
        let feature = layer.feature(index).expect("feature");
        println!("  [{index}] {:?}", feature.properties());
    }
    let mut heights: Vec<f64> = Vec::new();
    for index in 0..layer.len() {
        let feature = layer.feature(index).expect("feature");
        for (key, value) in feature.properties() {
            if &**key == "render_height"
                && let tessella_source::mvt::Value::Number(number) = value
            {
                heights.push(*number);
            }
        }
    }
    heights.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!("render_height over {} features:", heights.len());
    println!("  min {:?} max {:?}", heights.first(), heights.last());
    let mut counts: std::collections::BTreeMap<i64, usize> = Default::default();
    for h in &heights {
        *counts.entry(*h as i64).or_default() += 1;
    }
    println!("  distinct values: {counts:?}");
}

/// The whole chain from a real feature to the number the binder would write.
#[test]
#[ignore = "reads a tile fetched out of band"]
fn get_render_height_evaluates_against_a_real_feature() {
    use tessella_style::expression::{Expression, Feature};

    let path = std::env::var("TSL_TILE").expect("TSL_TILE");
    let bytes = std::fs::read(&path).expect("tile bytes");
    let tile = Tile::decode(&bytes).expect("decode");
    let layer = tile.layer("building").expect("a building layer");

    let source = tessella_style::Value::Array(alloc_vec());
    let expression = Expression::parse(&source).expect("the expression liberty uses");

    for index in 0..layer.len().min(6) {
        let feature = layer.feature(index).expect("feature");
        let direct = feature.property("render_height");
        let zoomless = expression.evaluate(None, Some(&feature as &dyn Feature));
        let zoomed = expression.evaluate(Some(14.0), Some(&feature as &dyn Feature));
        println!("[{index}] property()={direct:?}");
        println!("      evaluate(None)={zoomless:?}");
        println!("      evaluate(14)={zoomed:?}");
    }
}

/// `["get", "render_height"]`, built in the style's own value type.
fn alloc_vec() -> Vec<tessella_style::Value> {
    vec![
        tessella_style::Value::String("get".into()),
        tessella_style::Value::String("render_height".into()),
    ]
}
