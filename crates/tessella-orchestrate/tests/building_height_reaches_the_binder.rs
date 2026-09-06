//! Does a real building's `render_height` reach the buffer the shader reads?

use tessella_orchestrate::TileId;
use tessella_orchestrate::tile::{Content, build_mvt_tile};
use tessella_style::document::Style;

#[test]
#[ignore = "reads a tile fetched out of band"]
fn render_height_reaches_the_paint_buffer() {
    let path = std::env::var("TSL_TILE").expect("TSL_TILE");
    let bytes = std::fs::read(&path).expect("tile bytes");
    let decoded = tessella_source::mvt::Tile::decode(&bytes).expect("decode");

    let source = std::fs::read_to_string(
        "/mnt/dev/maplibre-frontend/maplibre-native/benchmark/fixtures/renderer/liberty.json",
    )
    .expect("liberty");
    let style = Style::parse(&source).expect("parse");

    for id in [
        TileId::new(14, 8800, 5373),
        TileId::overscaled(14, 8800, 5373, 15),
        TileId::overscaled(14, 8800, 5373, 16),
    ] {
        println!("== tile z{} overscaled_z {} ==", id.z, id.overscaled_z);
        let buckets = build_mvt_tile(&style, "openmaptiles", id, &decoded).expect("build");
        for bucket in &buckets {
            if !matches!(bucket.content, Content::Fill3d(_)) {
                continue;
            }
            let data = bucket.binder.data();
            println!(
                "layer {}: binder stride {} bytes {}",
                bucket.layer_id,
                bucket.binder.stride(),
                data.len()
            );
            let stride = bucket.binder.stride();
            if stride == 0 {
                println!("  no data-driven slots at all");
                continue;
            }
            let mut nonzero = 0usize;
            let entries = data.len() / stride;
            for i in 0..entries {
                let mut base = [0u8; 4];
                let mut height = [0u8; 4];
                base.copy_from_slice(&data[i * stride..i * stride + 4]);
                height.copy_from_slice(&data[i * stride + 4..i * stride + 8]);
                let (b, h) = (f32::from_le_bytes(base), f32::from_le_bytes(height));
                if h != 0.0 {
                    nonzero += 1;
                }
                if i < 5 {
                    println!("  [{i}] {b} , {h}");
                }
            }
            println!("  entries {entries}, nonzero second field: {nonzero}");
        }
    }
}
