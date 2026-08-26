//! A raster layer: the quad a tile is stretched over, and the colour the shader adjusts it by.
//!
//! Almost nothing compared to a fill or a line, and that is the point. A raster tile *is* an
//! image, so the geometry is a rectangle and the interesting work is the texture beside it — but
//! the rectangle has to be right, and the colour factors are not the property values.

use std::sync::Arc;

use tessella_layout::raster::{RasterBucket, contrast_factor, saturation_factor, spin_weights};
use tessella_orchestrate::tile::{TileId, build_mvt_tile, build_raster_tile};
use tessella_source::image::Image;
use tessella_source::mvt::Tile;
use tessella_style::Style;
use tessella_tile::mask::{MaskEntry, WHOLE_TILE};

const REAL_TILE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/real-world-0-0-0.mvt");

fn satellite() -> Style {
    serde_json::from_str(
        r#"{"version": 8, "sources": {"sat": {"type": "raster", "tiles": [], "tileSize": 256}},
            "layers": [{"id": "imagery", "type": "raster", "source": "sat"}]}"#,
    )
    .expect("a style")
}

/// The mask a settled cover always produces.
const WHOLE: &[MaskEntry] = &[WHOLE_TILE];

/// A 2x2 opaque image, standing in for a decoded tile.
fn picture() -> Arc<Image> {
    Arc::new(Image {
        width: 2,
        height: 2,
        pixels: vec![255; 2 * 2 * 4],
    })
}

/// A raster layer draws from an image and no features at all.
///
/// The difference from every other layer here, all of which build from features and produce
/// nothing when there are none. A raster tile carries no features — a layer that waited for some
/// would draw no imagery ever, which is a blank map with a working style.
#[test]
fn a_raster_layer_draws_without_features() {
    let buckets =
        build_raster_tile(&satellite(), "sat", picture(), WHOLE).expect("the tile builds");

    assert_eq!(buckets.len(), 1, "one raster layer");
    let content = buckets[0]
        .content
        .as_raster()
        .expect("a raster layer builds a raster bucket");
    assert_eq!(content.bucket.quads(), 1, "one quad for a whole tile");
    assert_eq!(content.image.size(), (2, 2), "the picture rides with it");
    assert_eq!(buckets[0].drawable_count(), 1);
}

/// Two raster layers over one source share one decoded picture.
///
/// A style drawing imagery twice — a base pass and a tinted overlay — is two buckets and one
/// image, and a raster tile is a quarter of a megabyte. Copying it per layer is the kind of
/// allocation §11.5 counts.
#[test]
fn two_layers_over_one_source_share_the_picture() {
    let style: Style = serde_json::from_str(
        r#"{"version": 8, "sources": {"sat": {"type": "raster", "tiles": []}},
            "layers": [{"id": "base", "type": "raster", "source": "sat"},
                       {"id": "tint", "type": "raster", "source": "sat",
                        "paint": {"raster-opacity": 0.3}}]}"#,
    )
    .expect("a style");

    let buckets = build_raster_tile(&style, "sat", picture(), WHOLE).expect("the tile builds");
    assert_eq!(buckets.len(), 2);

    let first = buckets[0].content.as_raster().expect("raster");
    let second = buckets[1].content.as_raster().expect("raster");
    assert!(
        Arc::ptr_eq(&first.image, &second.image),
        "the picture was copied per layer"
    );
}

/// A raster layer over a feature source draws nothing.
///
/// Not a silent skip but the only correct answer: a raster layer's picture *is* its source's
/// tile, so a raster layer pointed at a vector source names a source with no picture to give.
/// Emitting a quad anyway would put geometry on the wire sampling a texture nothing uploaded,
/// which draws whatever the consumer last bound there.
#[test]
fn a_raster_layer_over_a_vector_source_draws_nothing() {
    let style: Style = serde_json::from_str(
        r#"{"version": 8, "sources": {"vec": {"type": "vector", "tiles": []}},
            "layers": [{"id": "confused", "type": "raster", "source": "vec"}]}"#,
    )
    .expect("a style");

    let tile = Tile::decode(REAL_TILE).expect("the fixture decodes");
    let buckets =
        build_mvt_tile(&style, "vec", TileId::new(0, 0, 0), &tile).expect("the tile builds");
    assert!(buckets.is_empty(), "a raster layer built from features");
}

/// Only the layers drawing from this source are built.
#[test]
fn another_sources_raster_layer_is_not_built() {
    let style: Style = serde_json::from_str(
        r#"{"version": 8,
            "sources": {"sat": {"type": "raster", "tiles": []},
                        "dem": {"type": "raster", "tiles": []}},
            "layers": [{"id": "imagery", "type": "raster", "source": "sat"},
                       {"id": "relief", "type": "raster", "source": "dem"}]}"#,
    )
    .expect("a style");

    let buckets = build_raster_tile(&style, "sat", picture(), WHOLE).expect("the tile builds");
    assert_eq!(buckets.len(), 1);
    assert_eq!(buckets[0].layer_id, "imagery");
    assert_eq!(buckets[0].layer_index, 0, "painter order is kept");
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

/// A raster drawable reaches the wire with both samplers bound to its own picture.
///
/// `RasterShaderSource` declares two textures and `render_raster_layer.cpp` sets the same image
/// to each. Slot 1 is the parent tile a fading tile blends against, and with no fade in progress
/// it is this tile. Binding only slot 0 leaves the second sampler unbound, and what a shader
/// reads from an unbound sampler is the backend's business rather than a defined black.
#[test]
fn the_encoded_raster_binds_its_picture_to_both_samplers() {
    use tessella_capture_abi::BuiltIn;
    use tessella_capture_abi::envelope::{GeometryId, TextureId, TextureRef, WireRecord};
    use tessella_orchestrate::emit::{SlabArena, encode_raster};

    let picture = TextureId(17);
    let bucket = RasterBucket::whole_tile();
    let mut arena = SlabArena::default();
    let encoded = encode_raster(&mut arena, GeometryId(2), &bucket, picture);

    assert_eq!(encoded.record.builtin_shader, BuiltIn::RasterShader as i32);
    assert_eq!(encoded.record.vertex_count, 4);
    assert_eq!(encoded.record.texture_refs.count, 2);

    let size = core::mem::size_of::<TextureRef>();
    let start = encoded.record.texture_refs.offset as usize;
    let bound: Vec<TextureRef> = (0..2)
        .map(|index| {
            TextureRef::from_bytes(&encoded.payload[start + index * size..]).expect("a ref")
        })
        .collect();

    assert_eq!(bound[0].slot, 0);
    assert_eq!(bound[1].slot, 1);
    assert!(
        bound.iter().all(|reference| reference.texture == picture),
        "the two samplers do not carry the same picture"
    );
}

/// Both vertex attributes read the one interleaved buffer, four bytes apart.
///
/// Position and texture coordinate travel together — mbgl declares them as one vertex — so a
/// descriptor pointing the second at its own slab would be describing a layout the bytes do not
/// have, and the consumer believes descriptors.
#[test]
fn the_raster_attributes_share_one_interleaved_buffer() {
    use tessella_capture_abi::AttributeDataType;
    use tessella_capture_abi::envelope::{GeometryId, TextureId};
    use tessella_orchestrate::emit::{SlabArena, encode_raster};

    let mut arena = SlabArena::default();
    let encoded = encode_raster(
        &mut arena,
        GeometryId(2),
        &RasterBucket::whole_tile(),
        TextureId(1),
    );

    let attributes = encoded.attributes();
    assert_eq!(attributes.len(), 2);
    assert_eq!(attributes[0].source, attributes[1].source, "two slabs");
    assert_eq!(attributes[0].offset, 0);
    assert_eq!(attributes[1].offset, 4);
    for attribute in &attributes {
        assert_eq!(attribute.stride, 8);
        assert_eq!(attribute.data_type, AttributeDataType::Short2 as u8);
        assert_eq!(
            attribute.declared_data_type,
            AttributeDataType::Short2 as u8
        );
    }

    // One segment however many quads a mask produced: they share the buffer.
    let segments = encoded.segments();
    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].vertex_length, 4);
    assert_eq!(segments[0].index_length, 6);
}

/// A decoded tile uploads whole, as RGBA, with no rect list.
///
/// Zero rects is what the envelope spells "all of it". A raster tile arrives complete and is
/// never touched again — a new tile is a new texture, not a region of an old one — so the
/// glyph atlas's dirty-rect machinery has nothing to describe here.
#[test]
fn a_raster_tile_uploads_whole() {
    use tessella_capture_abi::envelope::TextureId;
    use tessella_orchestrate::texture::{self, RASTER_TILE_FORMAT};
    use tessella_source::image::Image;

    let image = Image {
        width: 256,
        height: 256,
        pixels: vec![7; 256 * 256 * 4],
    };
    let upload = texture::raster_tile(TextureId(9), &image).expect("an upload");

    assert_eq!(upload.record.texture, TextureId(9));
    assert_eq!(upload.record.size.width, 256);
    assert_eq!(upload.record.size.height, 256);
    assert_eq!(upload.record.rect_count, 0, "a whole-texture upload");
    assert_eq!(upload.record.format, RASTER_TILE_FORMAT as u8);
    assert_eq!(upload.record.pixels.count as usize, image.pixels.len());
    assert_eq!(upload.pixels, image.pixels);

    // RGBA rather than the glyph atlas's single channel: a raster tile may carry alpha, and a
    // format that dropped it would draw a tile's transparent corner as opaque black.
    assert_ne!(RASTER_TILE_FORMAT, texture::GLYPH_ATLAS_FORMAT);
}

/// An image with no pixels produces no upload rather than a zero-sized texture.
#[test]
fn an_empty_picture_uploads_nothing() {
    use tessella_capture_abi::envelope::TextureId;
    use tessella_orchestrate::texture;
    use tessella_source::image::Image;

    for image in [
        Image {
            width: 0,
            height: 4,
            pixels: Vec::new(),
        },
        Image {
            width: 4,
            height: 0,
            pixels: Vec::new(),
        },
    ] {
        assert!(texture::raster_tile(TextureId(9), &image).is_none());
    }
}

/// The picture is uploaded before the geometry that names it.
///
/// A protocol fault rather than an arithmetic one, and invisible to any test that checks one
/// function's return value: a `GeometryAdd` carrying a `TextureRef` the consumer has never seen
/// an upload for binds nothing, and the tile draws as whatever was last in that slot. The
/// ordering is the producer's to guarantee because the ring is ordered and the consumer acts on
/// records as they arrive.
#[test]
fn the_picture_reaches_the_ring_before_the_geometry_that_binds_it() {
    use tessella_capture_abi::EnvelopeKind;
    use tessella_capture_abi::envelope::{GeometryId, TextureId};
    use tessella_capture_abi::ring::Ring;
    use tessella_orchestrate::emit::{self, SlabArena};
    use tessella_orchestrate::texture;
    use tessella_source::image::Image;

    let picture = TextureId(9);
    let image = Image {
        width: 4,
        height: 4,
        pixels: vec![255; 4 * 4 * 4],
    };

    let mut ring = Ring::new(1 << 16);
    let (producer, consumer) = ring.split();

    let upload = texture::raster_tile(picture, &image).expect("an upload");
    texture::write(producer, &upload).expect("writes");

    let mut arena = SlabArena::default();
    let encoded = emit::encode_raster(
        &mut arena,
        GeometryId(2),
        &RasterBucket::whole_tile(),
        picture,
    );
    emit::write(producer, &encoded).expect("writes");

    let mut kinds = Vec::new();
    while let Some(record) = consumer.peek() {
        kinds.push(record.kind);
        let consumed = record.consumed();
        consumer.advance(consumed);
    }
    assert_eq!(
        kinds,
        vec![EnvelopeKind::TextureUpdate, EnvelopeKind::GeometryAdd]
    );
}

/// A mask becomes geometry: one quad per entry, at that entry's own size.
///
/// mbgl's `RasterBucket::setMask` builds a quad at `EXTENT >> z` for each entry, and the
/// correspondence is the whole reason a mask needs no place on the wire. What travels is
/// vertices, and vertices already travel.
#[test]
fn a_mask_becomes_one_quad_per_entry() {
    // Three quarters of a tile whose top-left child has loaded — mbgl's `OneChild` case.
    let mask = [
        MaskEntry { z: 1, x: 0, y: 1 },
        MaskEntry { z: 1, x: 1, y: 0 },
        MaskEntry { z: 1, x: 1, y: 1 },
    ];
    let bucket = RasterBucket::masked(&mask);
    assert_eq!(bucket.quads(), 3);

    let corners: Vec<[i16; 2]> = bucket
        .vertices
        .iter()
        .map(|vertex| vertex.position)
        .collect();
    // The quarter that loaded is the one *not* drawn: nothing starts at the origin.
    assert!(!corners.contains(&[0, 0]), "the covered quarter was drawn");
    assert!(corners.contains(&[0, 4096]), "the bottom-left quarter");
    assert!(corners.contains(&[4096, 0]), "the top-right quarter");
    assert!(corners.contains(&[4096, 4096]), "the bottom-right quarter");
    assert!(
        corners
            .iter()
            .all(|point| point[0] <= 8192 && point[1] <= 8192)
    );
}

/// A mask of different depths produces quads of different sizes.
///
/// The property a quadrant-only implementation cannot have. mbgl's `Complex` case masks one tile
/// into six rectangles across three levels, and each level's extent is half the one above.
#[test]
fn a_deeper_mask_entry_is_a_smaller_quad() {
    let bucket = RasterBucket::masked(&[
        MaskEntry { z: 1, x: 1, y: 0 },
        MaskEntry { z: 3, x: 7, y: 6 },
    ]);
    assert_eq!(bucket.quads(), 2);

    let side = |quad: usize| -> i16 {
        let base = quad * 4;
        bucket.vertices[base + 1].position[0] - bucket.vertices[base].position[0]
    };
    assert_eq!(side(0), 4096, "one level down is half the extent");
    assert_eq!(side(1), 1024, "three levels down is an eighth");
}

/// An empty mask is an empty bucket, which draws nothing.
///
/// The opposite of the whole-tile mask, and the pair a caller most easily confuses. A parent
/// covered by four children must draw *nothing*; reading empty as "no restriction" renders every
/// pixel of that region twice, which for a translucent raster layer is visibly darker.
#[test]
fn a_fully_covered_tile_draws_nothing() {
    let bucket = RasterBucket::masked(&[]);
    assert_eq!(bucket.quads(), 0);
    assert!(bucket.is_empty());

    // And the layer reports no drawable for it, rather than an empty one.
    let buckets = build_raster_tile(&satellite(), "sat", picture(), &[]).expect("the tile builds");
    assert_eq!(buckets.len(), 1, "the layer is still present");
    assert_eq!(buckets[0].drawable_count(), 0, "but draws nothing");
}

/// The whole-tile mask is the same bucket `whole_tile()` builds.
///
/// Which is what lets the two paths converge: a settled cover produces `{(0, 0, 0)}` for every
/// tile, and that has to be byte-identical to the unmasked bucket or every settled frame would
/// re-upload geometry that had not changed.
#[test]
fn the_whole_tile_mask_is_the_unmasked_bucket() {
    assert_eq!(RasterBucket::masked(WHOLE), RasterBucket::whole_tile());
}
