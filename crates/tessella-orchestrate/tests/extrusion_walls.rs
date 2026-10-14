//! A fill extrusion's walls, drawn as instances over its own outline (§R3).
//!
//! # What the capture shows
//!
//! Two shaders per tile on an extrusion layer, not one: `sh0018` at five vertices — the roof and
//! ground outline — and `sh0019` at four. The four are a unit quad, and the building's outline
//! is fed to it *per instance*, so one quad raised once per outline point is every wall on the
//! map. This build emitted only the first, which draws roofs and outlines: a flat city rather
//! than an empty one, and wrong in a way that looks deliberate.
//!
//! # Why the numbers are not written here
//!
//! Because they are mbgl's and they are not the roof's. `idFillExtrusionOutlinePosAttribute` is
//! attribute *2* at binding 1, where the non-instanced shader's attribute 2 is its normal;
//! `idFillExtrusionDecimalsEdAttribute` is attribute 1 at binding 2. Both come from the
//! generated table, which had no entry for either instanced shader until the generator was
//! taught that mbgl wraps their `using` declarations and declares their per-instance attributes
//! in a second array. The assertions below are the capture's `iattr` lines.

use tessella_capture_abi::envelope::TextureFilter;
use tessella_capture_abi::envelope::{GeometryId, TextureId};
use tessella_capture_abi::{AttributeDataType, BuiltIn};
use tessella_layout::fill_extrusion::{self, FillExtrusionBucket};
use tessella_orchestrate::binder::VertexLayout;
use tessella_orchestrate::emit::{SlabArena, encode_extrusion, encode_extrusion_walls};

/// A square footprint, which is one building.
fn bucket() -> FillExtrusionBucket {
    let ring = vec![[0i16, 0], [0, 100], [100, 100], [100, 0], [0, 0]];
    let mut built = fill_extrusion::build(&[ring]);
    built.opaque = false;
    built
}

/// Encodes a roof and the walls that stand on it.
fn encode(atlas: Option<TextureId>) -> (SlabArena, tessella_orchestrate::Encoded) {
    let (arena, _, walls) = encode_on(atlas, None);
    (arena, walls)
}

/// The same, on a ground, and keeping the roof.
fn encode_on(
    atlas: Option<TextureId>,
    elevation: Option<TextureId>,
) -> (
    SlabArena,
    tessella_orchestrate::Encoded,
    tessella_orchestrate::Encoded,
) {
    let mut arena = SlabArena::new();
    let bucket = bucket();
    let layout = VertexLayout::default();
    let (roof, shared) = encode_extrusion(
        &mut arena,
        GeometryId(1),
        &bucket,
        &layout,
        &[],
        0,
        atlas,
        TextureFilter::Linear,
        elevation,
    );
    let walls = encode_extrusion_walls(
        &mut arena,
        GeometryId(2),
        shared,
        &layout,
        0,
        atlas,
        TextureFilter::Linear,
        elevation,
    );
    (arena, roof, walls)
}

/// A raised extrusion names the ground it stands on, roof and walls alike.
///
/// The elevation is a texture like any other on the wire, and the raised variant of this family
/// samples it per vertex -- so a drawable the frame marked `ON_TERRAIN` and did not give one to is
/// a set the consumer cannot write at all: "the set has 1 texture bindings and 0 arrived", and the
/// layer draws nothing rather than drawing flat. The fill, the line and the circle each passed it
/// and these two did not, which nothing could see until a consumer had a raised extrusion module
/// to bind -- tessella_emblema#123.
///
/// Both halves, because they are one building: a wall stands on the ground its roof is on.
#[test]
fn a_raised_extrusion_names_the_ground() {
    use tessella_capture_abi::terrain_ubo::TERRAIN_ELEVATION_SLOT;

    let ground = TextureId(77);
    let (_, roof, walls) = encode_on(None, Some(ground));
    for (what, encoded) in [("the roof", &roof), ("the walls", &walls)] {
        let refs = encoded.texture_refs();
        assert_eq!(refs.len(), 1, "{what} names {} textures", refs.len());
        assert_eq!(refs[0].texture, ground, "{what} names another texture");
        assert_eq!(
            refs[0].slot, TERRAIN_ELEVATION_SLOT,
            "{what} binds the ground at a slot the raised module does not sample"
        );
    }

    // And a flat one names nothing, which is what every drawable off a style with no terrain is.
    let (_, flat_roof, flat_walls) = encode_on(None, None);
    assert!(flat_roof.texture_refs().is_empty());
    assert!(flat_walls.texture_refs().is_empty());
}

/// The template is four vertices and six indices, whatever the building is.
#[test]
fn the_walls_are_one_quad_however_many_buildings_there_are() {
    let (_, walls) = encode(None);
    assert_eq!(
        walls.record.vertex_count, 4,
        "mbgl's `fillExtrusionVertices`: a unit quad"
    );
    let segments = walls.segments();
    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].vertex_length, 4);
    assert_eq!(
        segments[0].index_length, 6,
        "mbgl's `quadTriangleIndices`, the same six a background quad uses"
    );
}

/// Per vertex: the template corner alone.
#[test]
fn the_template_carries_only_its_corner() {
    let (_, walls) = encode(None);
    let attributes = walls.attributes();
    assert_eq!(
        attributes.len(),
        1,
        "one per-vertex attribute: {attributes:?}"
    );
    let corner = &attributes[0];
    assert_eq!(corner.attr_id, 0);
    assert_eq!(corner.binding, 0);
    assert_eq!(corner.stride, 4, "two `i16`");
    assert_eq!(corner.data_type, AttributeDataType::Short2 as u8);
}

/// Per instance: the roof's own buffer, at the roof's own stride.
///
/// The same bytes as the roof's vertex attributes, walked one outline point at a time instead of
/// one corner at a time — which is what makes this a second drawable rather than a second copy
/// of the geometry.
#[test]
fn the_instances_are_the_roofs_outline() {
    let mut arena = SlabArena::new();
    let bucket = bucket();
    let layout = VertexLayout::default();
    let (roof, shared) = encode_extrusion(
        &mut arena,
        GeometryId(1),
        &bucket,
        &layout,
        &[],
        0,
        None,
        TextureFilter::Linear,
        None,
    );
    let walls = encode_extrusion_walls(
        &mut arena,
        GeometryId(2),
        shared,
        &layout,
        0,
        None,
        TextureFilter::Linear,
        None,
    );

    let instances = walls.instance_attributes();
    assert_eq!(
        instances.len(),
        2,
        "outline position and decimals: {instances:?}"
    );

    let by_id: std::collections::BTreeMap<u32, _> =
        instances.iter().map(|a| (a.attr_id, a)).collect();

    // `iattr id=2 bind=1 dt=9 off=0 stride=8` in the capture.
    let position = by_id.get(&2).expect("outline position");
    assert_eq!(position.binding, 1);
    assert_eq!(position.offset, 0);
    assert_eq!(position.stride, 8);
    assert_eq!(position.data_type, AttributeDataType::Short2 as u8);

    // `iattr id=1 bind=2 dt=13 off=4 stride=8`.
    let decimals = by_id.get(&1).expect("decimals and edge distance");
    assert_eq!(decimals.binding, 2);
    assert_eq!(decimals.offset, 4);
    assert_eq!(decimals.stride, 8);
    assert_eq!(decimals.data_type, AttributeDataType::UShort2 as u8);

    // And they name the roof's buffer, not a second copy of it.
    let roof_position = roof
        .attributes()
        .iter()
        .find(|a| a.attr_id == 0)
        .expect("the roof's position")
        .source;
    assert_eq!(position.source.slab, roof_position.slab);
    assert_eq!(position.source.offset, roof_position.offset);
    assert_eq!(
        position.source.length, roof_position.length,
        "the walls stand on the roof's outline rather than a copy of it"
    );
}

/// The pattern variant is the instanced one, not the roof's.
#[test]
fn a_patterned_extrusion_takes_the_instanced_pattern_shader() {
    let (_, plain) = encode(None);
    assert_eq!(
        plain.record.builtin_shader,
        BuiltIn::FillExtrusionInstancedShader as i32
    );

    let (_, patterned) = encode(Some(TextureId(7)));
    assert_eq!(
        patterned.record.builtin_shader,
        BuiltIn::FillExtrusionPatternInstancedShader as i32,
        "shader 19 in the capture, beside the roof's 18"
    );
}

/// The slab's last instance carries the discard flag, which is what bounds the lookahead.
///
/// `tessella-layout`'s own suite proves the *bucket* ends on a flagged point. This proves the
/// bytes the consumer divides by the stride do, which is the form the invariant has to hold in:
/// the instance count is not on the wire — it is the outline slab's length over its stride — so
/// the last instance is the slab's last eight bytes, and the shader reads one past them unless
/// those bytes say to stop.
///
/// mbgl's body is `instance[gl_InstanceIndex + 1]` with nothing clamping the index, and it is
/// safe only because the discard test returns first. A producer that stopped flagging the closing
/// point would not draw anything wrong here; it would read past the end of a storage buffer on
/// every device, which is why this is checked on the producer's side of the wire.
#[test]
fn the_slabs_last_instance_is_flagged_discarded() {
    let mut arena = SlabArena::new();
    let bucket = bucket();
    let layout = VertexLayout::default();
    let (_, shared) = encode_extrusion(
        &mut arena,
        GeometryId(1),
        &bucket,
        &layout,
        &[],
        0,
        None,
        TextureFilter::Linear,
        None,
    );
    let walls = encode_extrusion_walls(
        &mut arena,
        GeometryId(2),
        shared,
        &layout,
        0,
        None,
        TextureFilter::Linear,
        None,
    );

    let decimals = walls
        .instance_attributes()
        .iter()
        .find(|attribute| attribute.attr_id == 1)
        .copied()
        .expect("the decimals and edge distance attribute");
    let bytes = arena
        .resolve_written(decimals.source)
        .expect("the outline slab this frame wrote");

    let stride = decimals.stride as usize;
    assert_eq!(
        bytes.len() % stride,
        0,
        "the slab does not divide into instances: {} over {stride}",
        bytes.len()
    );
    let instances = bytes.len() / stride;
    assert_eq!(
        instances,
        bucket.vertices.len(),
        "one instance per outline point, which is how the consumer derives the count"
    );

    let at = (instances - 1) * stride + decimals.offset as usize;
    let last = u16::from_le_bytes([bytes[at], bytes[at + 1]]);
    assert_eq!(
        last & 1,
        1,
        "the last instance is where `gl_InstanceIndex + 1` leaves the slab"
    );

    // And it is the only one here: a single ring, so every other point has an edge leaving it.
    let flagged = (0..instances)
        .filter(|instance| {
            let at = instance * stride + decimals.offset as usize;
            u16::from_le_bytes([bytes[at], bytes[at + 1]]) & 1 == 1
        })
        .count();
    assert_eq!(flagged, 1, "one closing point, one flag");
}

/// And they reach the wire, through a real frame.
///
/// The encoder being right is half of it; the other half is that the drawable dispatch asks for
/// the wall record at all. It caches one list of records per bucket and picks by sub-layer,
/// where it used to cache a single record and copy it — which was correct for an extrusion with
/// one geometry and silently wrong the moment it had two.
mod through_a_frame {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use tessella_capture_abi::ProjectionMode;

    use tessella_capture_abi::EnvelopeKind;
    use tessella_capture_abi::envelope::{GeometryAdd, ViewId, WireRecord as _};
    use tessella_capture_abi::ring::Ring;
    use tessella_orchestrate::SlabArena;
    use tessella_orchestrate::frame::{self, Frame};
    use tessella_orchestrate::tile::{TileId, build_mvt_tile};
    use tessella_source::mvt::Tile;
    use tessella_style::Style;
    use tessella_style::light::Light;
    use tessella_tile::camera;
    use tessella_tile::cover::{self, ViewTransform};

    const REAL_TILE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/real-world-0-0-0.mvt");

    const STYLE: &str = r##"{
      "version": 8,
      "sources": {"src": {"type": "vector", "tiles": []}},
      "layers": [
        {"id": "blocks", "type": "fill-extrusion", "source": "src", "source-layer": "water",
         "paint": {"fill-extrusion-height": 20, "fill-extrusion-opacity": 0.8}}
      ]
    }"##;

    /// Emits one frame and returns the geometry records, by shader.
    fn shaders() -> BTreeMap<i32, Vec<GeometryAdd>> {
        let style = Style::parse(STYLE).expect("the style parses");
        let view = camera::settled(&ViewTransform {
            longitude: 0.0,
            latitude: 0.0,
            zoom: 3.0,
            width: 512.0,
            height: 512.0,
            bearing: 0.0,
            pitch: 45.0,
            ground_below: 0.0,
        });
        let tiles = cover::cover(&view).expect("covers");
        let decoded = Tile::decode(REAL_TILE).expect("the fixture decodes");
        let mut buckets = Vec::new();
        for tile in &tiles {
            let id = TileId::new(tile.z, tile.x, tile.y);
            let built = build_mvt_tile(&style, "src", id, &decoded).expect("the tile builds");
            buckets.push((id, Arc::new(built)));
        }

        let mut ring = Ring::new(1 << 24);
        let (producer, consumer) = ring.split();
        let mut arena = SlabArena::new();
        frame::emit(
            producer,
            &mut arena,
            &Frame {
                published_projection: None,
                projection: ProjectionMode::Mercator,
                style: &style,
                view: &view,
                view_id: ViewId(0),
                tiles: &tiles,
                buckets: &buckets,
                origins: &[],
                light: &Light::default(),
                fonts: None,
                patterns: None,
                states: None,
            },
        )
        .expect("the frame emits");

        let mut out: BTreeMap<i32, Vec<GeometryAdd>> = BTreeMap::new();
        while let Some(record) = consumer.peek() {
            if record.kind == EnvelopeKind::GeometryAdd
                && let Some(add) = GeometryAdd::from_bytes(record.record)
            {
                out.entry(add.builtin_shader).or_default().push(add);
            }
            let consumed = record.consumed();
            consumer.advance(consumed);
        }
        out
    }

    /// A frame with an extrusion layer carries both shaders, not just the roof's.
    #[test]
    fn a_frame_carries_the_walls_as_well_as_the_roof() {
        use tessella_capture_abi::BuiltIn;

        let by_shader = shaders();
        let roofs = by_shader
            .get(&(BuiltIn::FillExtrusionShader as i32))
            .map_or(0, Vec::len);
        let walls = by_shader
            .get(&(BuiltIn::FillExtrusionInstancedShader as i32))
            .map_or(0, Vec::len);

        assert!(
            roofs > 0,
            "no roof reached the wire: {:?}",
            by_shader.keys()
        );
        assert_eq!(
            walls, roofs,
            "one wall drawable per roof, which is what the capture shows on every tile"
        );
    }

    /// The wall records are the instanced ones, and the roof records are not.
    #[test]
    fn only_the_walls_carry_instance_attributes() {
        use tessella_capture_abi::BuiltIn;

        let by_shader = shaders();
        for record in by_shader
            .get(&(BuiltIn::FillExtrusionInstancedShader as i32))
            .expect("walls")
        {
            assert_eq!(
                record.instance_attrs.count, 2,
                "the outline position and the packed decimals"
            );
            assert_eq!(record.vertex_count, 4, "a unit quad");
        }
        for record in by_shader
            .get(&(BuiltIn::FillExtrusionShader as i32))
            .expect("roofs")
        {
            assert_eq!(
                record.instance_attrs.count, 0,
                "the roof is drawn once, not once per anything"
            );
        }
    }
}
