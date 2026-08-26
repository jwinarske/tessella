//! A raster layer: the quad a tile is stretched over, and the colour the shader adjusts it by.
//!
//! Almost nothing compared to a fill or a line, and that is the point. A raster tile *is* an
//! image, so the geometry is a rectangle and the interesting work is the texture beside it — but
//! the rectangle has to be right, and the colour factors are not the property values.

use tessella_layout::raster::{RasterBucket, contrast_factor, saturation_factor, spin_weights};
use tessella_orchestrate::tile::{TileId, build_mvt_tile};
use tessella_source::mvt::Tile;
use tessella_style::Style;

const REAL_TILE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/real-world-0-0-0.mvt");

fn satellite() -> Style {
    serde_json::from_str(
        r#"{"version": 8, "sources": {"sat": {"type": "raster", "tiles": [], "tileSize": 256}},
            "layers": [{"id": "imagery", "type": "raster", "source": "sat"}]}"#,
    )
    .expect("a style")
}

/// A raster layer draws whether or not the source decoded any features.
///
/// The difference from every other layer here, all of which build from features and produce
/// nothing when there are none. A raster tile carries no features at all — a layer that waited
/// for some would draw no imagery ever, which is a blank map with a working style.
#[test]
fn a_raster_layer_draws_without_features() {
    let tile = Tile::decode(REAL_TILE).expect("the fixture decodes");
    let buckets =
        build_mvt_tile(&satellite(), "sat", TileId::new(0, 0, 0), &tile).expect("the tile builds");

    assert_eq!(buckets.len(), 1, "one raster layer");
    let bucket = buckets[0]
        .content
        .as_raster()
        .expect("a raster layer builds a raster bucket");
    assert_eq!(bucket.quads(), 1, "one quad for a whole tile");
    assert_eq!(buckets[0].drawable_count(), 1);
}

/// The whole-tile quad covers the extent, and samples the image over the same range.
#[test]
fn the_quad_covers_the_tile_and_the_image() {
    let bucket = RasterBucket::whole_tile();
    assert_eq!(bucket.vertices.len(), 4);
    assert_eq!(bucket.indices, vec![0, 1, 2, 1, 2, 3]);

    let corners: Vec<[i16; 2]> = bucket
        .vertices
        .iter()
        .map(|vertex| vertex.position)
        .collect();
    assert_eq!(
        corners,
        vec![[0, 0], [8192, 0], [0, 8192], [8192, 8192]],
        "top-left, top-right, bottom-left, bottom-right"
    );

    // Texture coordinates track the position for a whole tile. They are separate attributes
    // because a *masked* quad samples a sub-rectangle of the image while covering a
    // sub-rectangle of the tile, and the two are not the same rectangle.
    for vertex in &bucket.vertices {
        #[allow(clippy::cast_sign_loss)]
        let expected = [vertex.position[0] as u16, vertex.position[1] as u16];
        assert_eq!(vertex.texture, expected);
    }
}

/// A mask entry's quad is the quadrant it names, and the entries tile the parent exactly.
///
/// The mask itself is not built — `StencilTiles` has no word for a quadrant, which is a wire
/// question rather than a code one. The shape is kept so adding it later is a caller passing a
/// mask, and this is what says the shape is right.
#[test]
fn a_masked_quad_is_the_quadrant_it_names() {
    let mut bucket = RasterBucket::default();
    for (x, y) in [(0u32, 0u32), (1, 0), (0, 1), (1, 1)] {
        bucket.add_quad(1, x, y);
    }
    assert_eq!(bucket.quads(), 4);

    // The four quadrants together cover the tile once: every corner of the extent appears, and
    // no quad runs past it.
    let positions: Vec<[i16; 2]> = bucket
        .vertices
        .iter()
        .map(|vertex| vertex.position)
        .collect();
    assert!(positions.contains(&[0, 0]));
    assert!(positions.contains(&[8192, 8192]));
    assert!(positions.contains(&[4096, 4096]), "the centre is missing");
    assert!(
        positions
            .iter()
            .all(|point| point[0] <= 8192 && point[1] <= 8192),
        "a quadrant ran past the tile"
    );

    // And each quad's indices reach its own vertices.
    assert_eq!(bucket.indices.len(), 24);
    assert_eq!(&bucket.indices[6..12], &[4, 5, 6, 5, 6, 7]);
}

/// Hue rotation redistributes the channels and preserves their sum.
///
/// The weights are a rotation about the grey axis of the colour cube, so they sum to one at every
/// angle — a rotation moves colour between channels and does not create or destroy it. A version
/// that normalised wrongly brightens or darkens as the hue turns, which reads as a broken image
/// rather than a broken rotation.
#[test]
fn hue_rotation_preserves_the_channel_sum() {
    for degrees in [0.0f32, 45.0, 90.0, 180.0, -120.0, 360.0] {
        let weights = spin_weights(degrees);
        let sum: f32 = weights[..3].iter().sum();
        assert!(
            (sum - 1.0).abs() < 1e-5,
            "at {degrees} degrees the weights sum to {sum}"
        );
        assert_eq!(weights[3], 0.0, "the fourth weight is padding");
    }

    // No rotation leaves each channel to itself.
    assert_eq!(spin_weights(0.0), [1.0, 0.0, 0.0, 0.0]);
}

/// Saturation and contrast are asymmetric, and mbgl's asymmetry.
///
/// Reducing either is linear; raising either is a reciprocal that runs away as it approaches its
/// limit. Reading them as symmetric — a single multiply — gives a picture that is nearly right at
/// small values and visibly wrong at large ones, which is the shape of a defect nobody reports
/// until a style uses it hard.
#[test]
fn the_colour_factors_are_asymmetric() {
    // Neutral is neutral.
    assert_eq!(saturation_factor(0.0), 0.0);
    assert_eq!(contrast_factor(0.0), 1.0);

    // Reducing is linear.
    assert_eq!(saturation_factor(-0.5), 0.5);
    assert_eq!(contrast_factor(-0.5), 0.5);

    // Raising is not, and grows faster than linearly.
    let half = contrast_factor(0.5);
    let three_quarters = contrast_factor(0.75);
    assert!((half - 2.0).abs() < 1e-6, "{half}");
    assert!(
        three_quarters - half > half - contrast_factor(0.25),
        "contrast rose linearly"
    );

    // The 1.001 is what keeps saturation finite at the property's own maximum.
    assert!(
        saturation_factor(1.0).is_finite(),
        "a saturation of one divided by zero"
    );
}

/// A raster layer's paint resolves, and none of it is data-driven.
///
/// A raster tile is an image rather than a set of features, so there is no feature for a property
/// to vary over — which is why the layer has no paint binder while every other tiled layer does.
#[test]
fn a_raster_layers_paint_is_all_uniform() {
    use tessella_style::property::resolve_paint;

    let style = satellite();
    let layer = style.layer("imagery").expect("the layer");
    let paint = resolve_paint(layer).expect("paint resolves");

    assert!(!paint.is_empty(), "a raster layer has paint properties");
    for (name, property) in &paint {
        assert!(
            !property.spec.data_driven,
            "{name} is marked data-driven on a layer with no features"
        );
    }
}

/// The evaluated-props buffer lands each value at mbgl's own offset.
///
/// `raster_layer_ubo.hpp` numbers every field in a comment, which is what this checks against.
/// The buffer is the shader's interface: a value at the wrong offset is read as a different
/// property entirely — a saturation read as a brightness — and produces a picture rather than an
/// error, so the offsets want asserting rather than reasoning about.
#[test]
fn the_raster_props_buffer_matches_the_oracles_offsets() {
    use tessella_layout::raster::RasterColour;
    use tessella_orchestrate::ubo::pack_raster_props;

    let colour = RasterColour {
        spin_weights: [0.1, 0.2, 0.3, 0.0],
        saturation_factor: 0.7,
        contrast_factor: 0.8,
    };
    let packed = pack_raster_props(colour, 0.4, 0.5, 0.6, 0.9);
    assert_eq!(packed.len(), 64, "sizeof(RasterEvaluatedPropsUBO)");

    let at = |offset: usize| {
        f32::from_le_bytes(packed[offset..offset + 4].try_into().expect("four bytes"))
    };
    assert_eq!([at(0), at(4), at(8), at(12)], colour.spin_weights);
    assert_eq!([at(16), at(20)], [0.0, 0.0], "tl_parent");
    assert_eq!(at(24), 1.0, "scale_parent");
    assert_eq!(at(28), 0.9, "buffer_scale");
    assert_eq!(at(32), 1.0, "fade_t");
    assert_eq!(at(36), 0.4, "opacity");
    assert_eq!(at(40), 0.5, "brightness_low");
    assert_eq!(at(44), 0.6, "brightness_high");
    assert_eq!(at(48), 0.7, "saturation_factor");
    assert_eq!(at(52), 0.8, "contrast_factor");
    assert_eq!([at(56), at(60)], [0.0, 0.0], "the two pads");
}

/// The drawable buffer is a matrix and nothing else, one aligned slot per tile.
#[test]
fn the_raster_drawable_buffer_is_one_matrix_per_tile() {
    use tessella_orchestrate::ubo::pack_raster_drawable_buffer;

    let mut first = [0.0f32; 16];
    first[0] = 7.0;
    let mut second = [0.0f32; 16];
    second[15] = 9.0;

    // A stride wider than the struct is what an alignment requirement produces; the second
    // matrix has to start at the second slot and not at byte 64.
    let packed = pack_raster_drawable_buffer(&[first, second], 256);
    assert_eq!(packed.len(), 512);
    assert_eq!(f32::from_le_bytes(packed[0..4].try_into().unwrap()), 7.0);
    assert_eq!(
        f32::from_le_bytes(packed[256 + 60..256 + 64].try_into().unwrap()),
        9.0
    );
    assert!(
        packed[64..256].iter().all(|byte| *byte == 0),
        "the padding between slots is not zeroed"
    );
}
