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

use tessella_capture_abi::envelope::{GeometryId, TextureId};
use tessella_capture_abi::{AttributeDataType, BuiltIn};
use tessella_layout::fill_extrusion::{self, FillExtrusionBucket};
use tessella_orchestrate::emit::{SlabArena, encode_extrusion, encode_extrusion_walls};
use tessella_orchestrate::binder::VertexLayout;

/// A square footprint, which is one building.
fn bucket() -> FillExtrusionBucket {
    let ring = vec![[0i16, 0], [0, 100], [100, 100], [100, 0], [0, 0]];
    let mut built = fill_extrusion::build(&[ring]);
    built.opaque = false;
    built
}

/// Encodes a roof and the walls that stand on it.
fn encode(atlas: Option<TextureId>) -> (SlabArena, tessella_orchestrate::Encoded) {
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
        atlas,
    );
    let walls = encode_extrusion_walls(&mut arena, GeometryId(2), shared, 0, atlas);
    (arena, walls)
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
    assert_eq!(attributes.len(), 1, "one per-vertex attribute: {attributes:?}");
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
    );
    let walls = encode_extrusion_walls(&mut arena, GeometryId(2), shared, 0, None);

    let instances = walls.instance_attributes();
    assert_eq!(instances.len(), 2, "outline position and decimals: {instances:?}");

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
