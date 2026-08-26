//! A fill-extrusion layer through the tile builder, the painter order and the props buffer.
//!
//! The geometry itself is checked in `tessella-layout`; this is the wiring around it — that an
//! extrusion layer builds from a vector tile at all, that it becomes the right number of
//! drawables, and that its properties land at mbgl's offsets.

use tessella_capture_abi::envelope::DrawFlags;
use tessella_orchestrate::tile::{TileId, build_mvt_tile};
use tessella_orchestrate::ubo::pack_fill_extrusion_props;
use tessella_source::mvt::Tile;
use tessella_style::Style;

const REAL_TILE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/real-world-0-0-0.mvt");

fn style_with(paint: &str) -> Style {
    serde_json::from_str(&format!(
        r#"{{"version": 8, "sources": {{"src": {{"type": "vector", "tiles": []}}}},
            "layers": [{{"id": "buildings", "type": "fill-extrusion", "source": "src",
                        "source-layer": "water"{paint}}}]}}"#
    ))
    .expect("a style")
}

/// An extrusion layer builds from a vector tile's polygons.
///
/// The layer type was in `LayerKind` and refused by the builder until now — it fell through to
/// the `continue` that names the unbuilt kinds — so a style with one drew nothing at all. The
/// fixture's `water` layer stands in for a building layer: what is under test is that polygons
/// reach an extrusion bucket, not what they are of.
#[test]
fn an_extrusion_layer_builds_from_polygons() {
    let tile = Tile::decode(REAL_TILE).expect("the fixture decodes");
    let buckets = build_mvt_tile(&style_with(""), "src", TileId::new(0, 0, 0), &tile)
        .expect("the tile builds");

    assert_eq!(buckets.len(), 1);
    let extrusion = match &buckets[0].content {
        tessella_orchestrate::tile::Content::Fill3d(bucket) => bucket,
        other => panic!("expected an extrusion, got {other:?}"),
    };
    assert!(!extrusion.vertices.is_empty(), "no outline was built");
    assert!(!extrusion.indices.is_empty(), "no roof was built");
    assert!(!extrusion.segments.is_empty());
}

/// A translucent extrusion is two drawables; an opaque one is a single pass.
///
/// mbgl's `doDepthPass = (!opaque || hasPattern)`, with `opaque` meaning an opacity of one.
/// Without the depth pass, every wall alpha-blends against the walls behind it — a city made of
/// glass rather than of buildings — so the count is not an optimisation but the difference
/// between two pictures.
#[test]
fn a_translucent_extrusion_takes_two_passes() {
    let tile = Tile::decode(REAL_TILE).expect("the fixture decodes");

    let opaque =
        build_mvt_tile(&style_with(""), "src", TileId::new(0, 0, 0), &tile).expect("builds");
    assert_eq!(
        opaque[0].drawable_count(),
        1,
        "an opaque extrusion is one pass"
    );

    let translucent = build_mvt_tile(
        &style_with(r#", "paint": {"fill-extrusion-opacity": 0.6}"#),
        "src",
        TileId::new(0, 0, 0),
        &tile,
    )
    .expect("builds");
    assert_eq!(
        translucent[0].drawable_count(),
        2,
        "a translucent extrusion needs a depth pass in front of its colour pass"
    );
}

/// Both passes are 3D, and only the second writes colour.
///
/// `IS_3D` has been in the ABI since R0 and nothing set it until now: an extrusion is the first
/// geometry in this build that leaves the map plane.
#[test]
fn the_two_passes_differ_only_in_writing_colour() {
    use tessella_orchestrate::view;

    let depth = view::extrusion_depth_flags();
    let color = view::extrusion_color_flags();

    assert!(depth.contains(DrawFlags::IS_3D));
    assert!(color.contains(DrawFlags::IS_3D));
    assert!(depth.contains(DrawFlags::ENABLE_DEPTH));
    assert!(color.contains(DrawFlags::ENABLE_DEPTH));

    assert!(
        !depth.contains(DrawFlags::ENABLE_COLOR),
        "the depth pass writes no colour"
    );
    assert!(color.contains(DrawFlags::ENABLE_COLOR));

    // Neither is stencilled. mbgl sets no stencil mode on either extrusion builder — a
    // building's walls legitimately overhang the tile that owns its footprint, and clipping
    // them to the tile square would slice every building on a tile boundary in half.
    assert!(!depth.contains(DrawFlags::ENABLE_STENCIL));
    assert!(!color.contains(DrawFlags::ENABLE_STENCIL));
}

/// The props buffer lands each value at mbgl's own offset.
///
/// `fill_extrusion_layer_ubo.hpp` numbers every field. Three of the five blocks are the *light*,
/// which is what makes an extrusion the first thing here whose colour depends on more than its
/// paint: a build that packed the paint and left the light at zero draws every building flat
/// black.
#[test]
fn the_props_buffer_matches_the_oracles_offsets() {
    let packed = pack_fill_extrusion_props(
        [0.1, 0.2, 0.3, 0.4],
        [0.5, 0.6, 0.7],
        [0.8, 0.9, 1.0],
        11.0,
        22.0,
        0.33,
        1.0,
        0.44,
    );
    assert_eq!(packed.len(), 80, "sizeof(FillExtrusionPropsUBO)");

    let at = |offset: usize| {
        f32::from_le_bytes(packed[offset..offset + 4].try_into().expect("four bytes"))
    };
    assert_eq!([at(0), at(4), at(8), at(12)], [0.1, 0.2, 0.3, 0.4], "color");
    assert_eq!([at(16), at(20), at(24)], [0.5, 0.6, 0.7], "light_color");
    assert_eq!(at(28), 0.0, "pad1");
    assert_eq!([at(32), at(36), at(40)], [0.8, 0.9, 1.0], "light_position");
    assert_eq!(at(44), 11.0, "base");
    assert_eq!(at(48), 22.0, "height");
    assert_eq!(at(52), 0.33, "light_intensity");
    assert_eq!(at(56), 1.0, "vertical_gradient");
    assert_eq!(at(60), 0.44, "opacity");
    assert_eq!(at(64), 0.0, "fade");
    assert_eq!(at(68), 1.0, "from_scale");
    assert_eq!(at(72), 1.0, "to_scale");
    assert_eq!(at(76), 0.0, "pad2");
}

/// The three data-driven properties are the three an extrusion is.
///
/// Colour, height and base vary per feature — a building layer varies all three, which is the
/// whole point of it — so unlike a raster layer they have to reach the shader as attributes.
#[test]
fn colour_height_and_base_are_data_driven() {
    use tessella_style::property::resolve_paint;

    let style = style_with("");
    let paint = resolve_paint(style.layer("buildings").expect("the layer")).expect("resolves");

    let driven: Vec<&str> = paint
        .iter()
        .filter(|(_, property)| property.spec.data_driven)
        .map(|(name, _)| *name)
        .collect();
    assert_eq!(
        driven,
        vec![
            "fill-extrusion-base",
            "fill-extrusion-color",
            "fill-extrusion-height",
            "fill-extrusion-pattern",
        ]
    );
}
