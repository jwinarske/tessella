//! Every bucket kind reaches the wire, and describes itself the way its shader reads it.
//!
//! Three of the six kinds had no encoder at all: a line, a circle and an extrusion were built
//! from a tile and then had nowhere to go. Nothing downstream noticed, because a consumer that
//! never receives a geometry draws nothing and a producer that never encodes one emits no error
//! — the layer is simply absent, which looks exactly like a style that does not draw it.
//!
//! What is checked here is the part a shader cannot survive being wrong about: which attributes
//! a geometry supplies, at which offsets, in which buffer, and at which stride. A descriptor
//! that names the vertex buffer with the binder's stride reads one buffer through the other's
//! step and produces geometry made of noise — which renders, and is wrong.

use tessella_capture_abi::envelope::GeometryId;
use tessella_capture_abi::{AttributeDataType, BuiltIn, declared_for};
use tessella_orchestrate::binder::{
    CIRCLE_FAMILY, FILL_EXTRUSION_FAMILY, LINE_FAMILY, attribute_ids, layout, permutation_key,
};
use tessella_orchestrate::emit::LineDraw;
use tessella_orchestrate::tile::{Content, TileId, build_mvt_tile};
use tessella_orchestrate::{Encoded, SlabArena, encode_circle, encode_extrusion, encode_line};
use tessella_source::mvt::Tile;
use tessella_style::Style;

const REAL_TILE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/real-world-0-0-0.mvt");

fn style_with(kind: &str, extra: &str) -> Style {
    serde_json::from_str(&format!(
        r#"{{"version": 8, "sources": {{"src": {{"type": "vector", "tiles": []}}}},
            "layers": [{{"id": "l", "type": "{kind}", "source": "src",
                        "source-layer": "water"{extra}}}]}}"#
    ))
    .expect("a style")
}

/// Builds one layer of the fixture and encodes it, returning what reached the record.
fn encode(kind: &str, extra: &str, family: &[BuiltIn], shader: BuiltIn) -> Encoded {
    let tile = Tile::decode(REAL_TILE).expect("the fixture decodes");
    let buckets = build_mvt_tile(&style_with(kind, extra), "src", TileId::new(0, 0, 0), &tile)
        .expect("the tile builds");
    let bucket = &buckets[0];

    let ids = attribute_ids(family);
    let key = permutation_key(&bucket.paint, &ids);
    let vertex_layout = layout(&bucket.binder, &ids, |attr_id| {
        declared_for(shader, attr_id).map(|a| (a.binding, a.declared))
    });

    let mut arena = SlabArena::new();
    let data = bucket.binder.data();
    match &bucket.content {
        Content::Line(b) => encode_line(
            &mut arena,
            GeometryId(1),
            b,
            &LineDraw {
                layout: &vertex_layout,
                attributes: data,
                permutation_key: key,
                pattern_atlas: None,
                dash_atlas: None,
                gradient_ramp: None,
                pattern_vertices: None,
                elevation: None,
            },
        ),
        Content::Circle(b) => encode_circle(
            &mut arena,
            GeometryId(1),
            b,
            &vertex_layout,
            data,
            key,
            None,
        ),
        Content::Fill3d(b) => {
            // The roof; its walls are a second drawable over the same buffer.
            encode_extrusion(
                &mut arena,
                GeometryId(1),
                b,
                &vertex_layout,
                data,
                key,
                None,
            )
            .0
        }
        other => panic!("unexpected content: {other:?}"),
    }
}

/// A line supplies both fixed attributes, from one buffer at the line vertex's stride.
///
/// The second is the one that matters. A `LineBucket` holds the centerline doubled, and the
/// extrusion that turns it into a quad lives in `data` — so a geometry that supplied only the
/// position would describe a ribbon of degenerate triangles. It would draw, and draw nothing.
#[test]
fn a_line_supplies_its_position_and_its_extrusion() {
    let encoded = encode(
        "line",
        r#", "paint": {"line-width": 2}"#,
        LINE_FAMILY,
        BuiltIn::LineShader,
    );
    assert_eq!(encoded.record.builtin_shader, BuiltIn::LineShader as i32);
    assert!(encoded.record.vertex_count > 0, "no vertices were encoded");

    let attrs = encoded.attributes();
    let position = attrs.iter().find(|a| a.attr_id == 0).expect("a position");
    let data = attrs
        .iter()
        .find(|a| a.attr_id == 1)
        .expect("the line data");

    assert_eq!(position.offset, 0);
    assert_eq!(data.offset, 4, "the data follows two shorts");
    assert_eq!(position.stride, 8, "a line vertex is eight bytes");
    assert_eq!(data.stride, 8);
    assert_eq!(
        position.source, data.source,
        "both come out of the vertex buffer, not the binder's"
    );
    assert_eq!(position.data_type, AttributeDataType::Short2 as u8);
    assert_eq!(data.data_type, AttributeDataType::UByte4 as u8);
}

/// A circle's vertex is a position and nothing else: the radius is a uniform.
#[test]
fn a_circle_supplies_only_its_center() {
    let encoded = encode("circle", "", CIRCLE_FAMILY, BuiltIn::CircleShader);
    assert_eq!(encoded.record.builtin_shader, BuiltIn::CircleShader as i32);
    let attrs = encoded.attributes();
    let position = attrs.iter().find(|a| a.attr_id == 0).expect("a position");
    assert_eq!(position.stride, 4, "two shorts and no more");
    assert!(
        attrs
            .iter()
            .all(|a| a.attr_id != 1 || a.source != position.source),
        "nothing else may claim the vertex buffer"
    );
}

/// An extrusion supplies the packed fraction beside the position, at the extrusion stride.
#[test]
fn an_extrusion_supplies_its_packed_fraction() {
    let encoded = encode(
        "fill-extrusion",
        "",
        FILL_EXTRUSION_FAMILY,
        BuiltIn::FillExtrusionShader,
    );
    assert_eq!(
        encoded.record.builtin_shader,
        BuiltIn::FillExtrusionShader as i32
    );
    let attrs = encoded.attributes();
    let position = attrs.iter().find(|a| a.attr_id == 0).expect("a position");
    let decimals = attrs.iter().find(|a| a.attr_id == 1).expect("the decimals");
    assert_eq!(position.stride, 8);
    assert_eq!(decimals.offset, 4);
    assert_eq!(decimals.data_type, AttributeDataType::UShort2 as u8);
    assert_eq!(position.source, decimals.source);
}

/// Every attribute binds where its shader declares it, rather than where the encoder guessed.
///
/// The binding is what the consumer uses to attach a buffer to a shader input. An encoder that
/// invents one produces a geometry the shader reads through the wrong slot, and the generated
/// table is the only place the true answer lives.
///
/// The rule covers an attribute the table does *not* carry as well, and that half is what this
/// used to skip: it checked ids 0 and 1 only, which is why the skirt at id 2 rode the wire for
/// months with no table entry (tessella#331). The ABI has one answer for an attribute a shader
/// does not declare -- binding `-1`, which tells the consumer to drop it -- so an undeclared
/// attribute at a positive slot is neither bindable nor droppable, and a consumer reading the
/// table loses bytes the producer meant it to have.
#[test]
fn every_attribute_binds_where_the_shader_declares() {
    for (kind, extra, family, shader) in [
        (
            "line",
            r#", "paint": {"line-width": 2}"#,
            LINE_FAMILY,
            BuiltIn::LineShader,
        ),
        ("circle", "", CIRCLE_FAMILY, BuiltIn::CircleShader),
        (
            "fill-extrusion",
            "",
            FILL_EXTRUSION_FAMILY,
            BuiltIn::FillExtrusionShader,
        ),
    ] {
        let encoded = encode(kind, extra, family, shader);
        declared_or_dropped(kind, shader, &encoded);
    }
}

/// Every attribute of an `Encoded` is in its shader's table, or is on the wire at `-1`.
fn declared_or_dropped(what: &str, shader: BuiltIn, encoded: &Encoded) {
    assert!(
        !encoded.attributes().is_empty(),
        "{what}: nothing was described at all"
    );
    for attribute in encoded.attributes() {
        let Some(declared) = declared_for(shader, attribute.attr_id) else {
            assert_eq!(
                attribute.binding, -1,
                "{what}: attribute {} is on the wire at binding {} and no table declares it, so a \
                 consumer building its vertex input from the table loses it",
                attribute.attr_id, attribute.binding
            );
            continue;
        };
        assert_eq!(
            attribute.binding, declared.binding,
            "{what}: attribute {} bound at {} but declared at {}",
            attribute.attr_id, attribute.binding, declared.binding
        );
        assert_eq!(
            attribute.declared_data_type, declared.declared as u8,
            "{what}: attribute {} declares the wrong type",
            attribute.attr_id
        );
    }
}

/// Every encoder over the raster vertex describes its skirt flag, which is this producer's own
/// attribute rather than mbgl's.
///
/// None of the three is built from an MVT layer, so none goes through `encode` above -- and each
/// sends a third attribute mbgl's shader has no input for. It is not optional: with the per-layer
/// curtain not emitted, every `gross` row of the parity sweep is identical to the pixel while
/// `terrain_cover_p` counts up to 2382 holes of 1,620,000 at the high-pitch cameras.
///
/// Hillshade is here because this test found it. tessella#331 named the raster and the relief, and
/// the third shares `alloc_raster`'s vertex with them and had the same undeclared slot.
#[test]
fn every_raster_family_encoder_describes_its_skirt() {
    use tessella_capture_abi::envelope::{TextureFilter, TextureId};
    use tessella_layout::raster::RasterBucket;
    use tessella_orchestrate::emit::{encode_color_relief, encode_hillshade, encode_raster};

    let bucket = RasterBucket::whole_tile();

    let mut arena = SlabArena::new();
    let raster = encode_raster(
        &mut arena,
        GeometryId(1),
        &bucket,
        TextureId(1),
        None,
        TextureFilter::Linear,
    );
    declared_or_dropped("raster", BuiltIn::RasterShader, &raster);

    let relief = encode_color_relief(
        &mut arena,
        GeometryId(2),
        &bucket,
        TextureId(2),
        TextureId(3),
        TextureId(4),
        None,
    );
    declared_or_dropped("color relief", BuiltIn::ColorReliefShader, &relief);

    let hillshade = encode_hillshade(&mut arena, GeometryId(3), &bucket, TextureId(5), None);
    declared_or_dropped("hillshade", BuiltIn::HillshadeShader, &hillshade);

    // And the skirt is the third of the three, named rather than merely consistent: the checks
    // above pass just as well for a producer that stopped sending it.
    for (what, shader, encoded) in [
        ("raster", BuiltIn::RasterShader, &raster),
        ("color relief", BuiltIn::ColorReliefShader, &relief),
        ("hillshade", BuiltIn::HillshadeShader, &hillshade),
    ] {
        let attributes = encoded.attributes();
        let skirt = attributes
            .iter()
            .find(|attribute| attribute.binding == 2)
            .unwrap_or_else(|| panic!("{what}: no attribute at binding 2"));
        let declared = declared_for(shader, skirt.attr_id).expect("the table declares it");
        assert_eq!(declared.name, "tessellaSkirtVertexAttribute", "{what}");
        assert_eq!(declared.declared, AttributeDataType::Short2, "{what}");
    }
}

/// The segment run survives the trip, so a consumer can draw the bucket in pieces.
#[test]
fn segments_reach_the_record() {
    let encoded = encode(
        "line",
        r#", "paint": {"line-width": 2}"#,
        LINE_FAMILY,
        BuiltIn::LineShader,
    );
    let segments = encoded.segments();
    assert!(!segments.is_empty(), "a bucket with vertices has segments");
    let total: u32 = segments.iter().map(|s| s.vertex_length).sum();
    assert_eq!(
        total, encoded.record.vertex_count,
        "segments cover the buffer"
    );
}
