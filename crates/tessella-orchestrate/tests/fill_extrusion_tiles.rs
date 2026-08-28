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

/// Two geometries, each drawn once per pass.
///
/// The roof and the walls raised over it, in the depth pass and again in the colour pass — four
/// drawables, which is what the capture shows on every tile of an extrusion layer. Whether there
/// is a depth pass is mbgl's `doDepthPass = (!opaque || hasPattern)`; without it every wall
/// alpha-blends against the walls behind it, a city made of glass rather than of buildings, so
/// the count is not an optimisation but the difference between two pictures.
#[test]
fn an_extrusion_is_two_geometries_in_one_pass_or_two() {
    let tile = Tile::decode(REAL_TILE).expect("the fixture decodes");

    let opaque =
        build_mvt_tile(&style_with(""), "src", TileId::new(0, 0, 0), &tile).expect("builds");
    assert_eq!(
        opaque[0].drawable_count(),
        2,
        "an opaque unpatterned extrusion is its roof and its walls, once each"
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
        4,
        "and a translucent one draws both again in the depth pass in front of the colour one"
    );
}

/// Both passes are 3D, and only the second writes colour — and the stencil follows the first.
///
/// `IS_3D` has been in the ABI since R0 and nothing set it until now: an extrusion is the first
/// geometry in this build that leaves the map plane.
///
/// # What this used to assert
///
/// That neither pass is stencilled, on the reasoning that a building's walls legitimately
/// overhang the tile that owns its footprint and clipping them to the tile square would slice
/// every building on a boundary in half. The reasoning is sound; the fact was wrong. mbgl writes
/// `colorBuilder->setEnableStencil(doDepthPass)`, the pattern capture's colour-pass drawable
/// carries `flags=1111` — is3D, stencil, depth, colour — and the extrusion layer appears in the
/// capture's stencil section with a mask per tile.
///
/// The two are reconciled by the condition. With no depth pass nothing has written this layer's
/// stencil, so testing against it would do exactly the slicing the old comment described. With
/// one, the prepass has laid down what the colour pass tests against.
#[test]
fn the_two_passes_differ_in_colour_and_in_stencil() {
    use tessella_orchestrate::view;

    let depth = view::extrusion_depth_flags();
    let color = view::extrusion_color_flags(true);

    assert!(depth.contains(DrawFlags::IS_3D));
    assert!(color.contains(DrawFlags::IS_3D));
    assert!(depth.contains(DrawFlags::ENABLE_DEPTH));
    assert!(color.contains(DrawFlags::ENABLE_DEPTH));

    assert!(
        !depth.contains(DrawFlags::ENABLE_COLOR),
        "the depth pass writes no colour"
    );
    assert!(color.contains(DrawFlags::ENABLE_COLOR));

    assert!(
        !depth.contains(DrawFlags::ENABLE_STENCIL),
        "the prepass writes the stencil rather than testing it"
    );
    assert!(
        color.contains(DrawFlags::ENABLE_STENCIL),
        "and the colour pass tests it: `flags=1111` in the capture"
    );

    // Without a prepass there is nothing to test against.
    assert!(!view::extrusion_color_flags(false).contains(DrawFlags::ENABLE_STENCIL));
}

/// A pattern earns a depth pass whatever the opacity says.
///
/// mbgl's `doDepthPass = (!opaque || hasPattern)`. Both halves were quoted in this build's
/// comments and only the first was implemented, so an opaque patterned extrusion emitted one
/// drawable where the capture has two — and, through the condition above, an unstencilled one.
/// A pattern is sampled per fragment and can be transparent wherever the sprite is, so the
/// surface is not opaque however opaque the layer's opacity is.
#[test]
fn an_opaque_patterned_extrusion_still_gets_a_depth_pass() {
    use tessella_layout::fill_extrusion::FillExtrusionBucket;

    let plain = |opaque, patterned| FillExtrusionBucket {
        opaque,
        patterned,
        ..FillExtrusionBucket::default()
    };

    assert!(plain(false, false).needs_depth_pass(), "translucent");
    assert!(
        plain(false, true).needs_depth_pass(),
        "translucent, patterned"
    );
    assert!(
        plain(true, true).needs_depth_pass(),
        "opaque and patterned: the half that was missing"
    );
    assert!(
        !plain(true, false).needs_depth_pass(),
        "and an opaque unpatterned extrusion needs only its colour pass"
    );
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

/// A filter is evaluated at the tile's own zoom, as mbgl's layouts evaluate it.
///
/// mbgl's `zoom` there is `parameters.tileID.overscaledZ`, and it reaches the filter through the
/// same `EvaluationContext` the paint properties use. The builders passed no zoom at all, so
/// `["zoom"]` in a filter raised an evaluation error, and a filter that errors admits nothing —
/// the layer drew at *no* zoom rather than at the wrong ones. That is the shape of the bug that
/// hides: a blank layer looks like a style choice, where a misplaced one looks like a bug.
#[test]
fn a_filter_sees_the_tiles_zoom() {
    let tile = Tile::decode(REAL_TILE).expect("the fixture decodes");
    let style = serde_json::from_str::<Style>(
        r#"{"version": 8, "sources": {"src": {"type": "vector", "tiles": []}},
            "layers": [{"id": "buildings", "type": "fill-extrusion", "source": "src",
                        "source-layer": "water",
                        "filter": ["step", ["zoom"], false, 3, true]}]}"#,
    )
    .expect("a style");

    let below = build_mvt_tile(&style, "src", TileId::new(2, 0, 0), &tile).expect("builds");
    let above = build_mvt_tile(&style, "src", TileId::new(4, 0, 0), &tile).expect("builds");

    let geometry = |buckets: &[tessella_orchestrate::tile::LayerBucket]| match &buckets[0].content {
        tessella_orchestrate::tile::Content::Fill3d(bucket) => !bucket.vertices.is_empty(),
        other => panic!("expected an extrusion, got {other:?}"),
    };
    assert!(!geometry(&below), "the filter excludes everything below z3");
    assert!(geometry(&above), "and admits it above");
}

/// The drawable block is an extrusion's own shape, not a fill's.
///
/// Every layer kind has its own block, and this one differs from a fill's in the fields that
/// decide whether a building has a height at all: `height_factor` and the tile's split pixel
/// coordinate sit where a fill keeps its mix factors. Packing a fill entry into it puts the
/// colour interpolation — zero, for a constant colour — where `height_factor` belongs, and every
/// building comes out flat on the ground. It draws, and what it draws is a fill layer.
#[test]
fn the_drawable_block_is_an_extrusions_own() {
    use tessella_capture_abi::generated::ubo_layouts::FILL_EXTRUSION_DRAWABLE_UBO;
    use tessella_orchestrate::ubo::{
        ExtrusionDrawableEntry, height_factor, pack_extrusion_drawable_buffer,
    };
    use tessella_tile::camera;
    use tessella_tile::cover::ViewTransform;

    let view = camera::settled(&ViewTransform {
        longitude: 13.4,
        latitude: 52.5,
        zoom: 14.0,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
    });
    let entry = ExtrusionDrawableEntry::for_tile(&view, 14, 8802, 5373, 0, 1, 0, [0.0, 0.0, 0.0])
        .expect("an entry");

    let packed = pack_extrusion_drawable_buffer(&[entry], FILL_EXTRUSION_DRAWABLE_UBO.stride);
    assert_eq!(packed.len(), FILL_EXTRUSION_DRAWABLE_UBO.stride as usize);

    let at = |offset: usize| {
        f32::from_le_bytes(packed[offset..offset + 4].try_into().expect("four bytes"))
    };
    // The offsets are the generated layout's, read by name rather than counted by hand.
    let field = |name: &str| {
        FILL_EXTRUSION_DRAWABLE_UBO
            .fields
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("no field {name}"))
            .offset as usize
    };
    assert_eq!(
        at(field("height_factor")),
        height_factor(14),
        "the height factor is not where the shader reads it"
    );
    assert_eq!(at(field("height_factor")), -4.0, "-2^14 / 512 / 8");

    // The pixel coordinate is split because one f32 cannot hold it at a high zoom: 8802 tiles of
    // 512 pixels is over four million, past f32's exact integer range once a fraction is added.
    let upper = at(field("pixel_coord_upper"));
    let lower = at(field("pixel_coord_lower"));
    #[allow(clippy::cast_possible_truncation)]
    let rejoined = (upper as i32) << 16 | (lower as i32);
    assert_eq!(rejoined, 8802 * 512, "the halves do not rejoin");
}
