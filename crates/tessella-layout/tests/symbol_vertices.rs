//! Symbol vertices, in the byte layout the shader declares.
//!
//! There is no golden dump with a symbol layer in it yet — R2's oracle capture is still to come
//! — so what is pinned here is the packing itself, transcribed from mbgl's `layoutVertex`. Every
//! one of these numbers is a scale the shader divides back out, and getting one wrong scales
//! that term by a power of two: a label in the right place at the wrong size, or the right size
//! in the wrong place.

use tessella_capture_abi::generated::shader_attributes::SYMBOL_ICON_SHADER;
use tessella_layout::symbol_bucket::{
    MAX_PACKED_SIZE, SIZE_PACK_FACTOR, SizeRange, SymbolBuffers, dynamic_vertex, layout_vertex,
    opacity_vertex, pack_size,
};

/// The anchor rides in tile units and the corner offset in thirty-seconds of a pixel.
///
/// They share one `Short4` because some devices allow only eight vertex attributes, which is
/// mbgl's stated reason and the thing that makes the layout look arbitrary.
#[test]
fn the_anchor_and_the_offset_share_one_attribute() {
    let vertex = layout_vertex(
        (1000.0, 2000.0),
        (-4.0, 4.0),
        0.0,
        (10, 20),
        SizeRange::constant(16.0),
        true,
        (0.0, 0.0),
        (0.0, 0.0),
    );

    assert_eq!(vertex.pos_offset[0], 1000, "the anchor is whole tile units");
    assert_eq!(vertex.pos_offset[1], 2000);
    assert_eq!(vertex.pos_offset[2], -128, "-4 pixels in thirty-seconds");
    assert_eq!(vertex.pos_offset[3], 128);
}

/// A glyph's along-line offset is folded into the vertical corner offset.
///
/// It is not a separate attribute — there is no room for one — so it is added before the
/// thirty-second scaling, which means a label following a line and one sitting at a point pack
/// identically and the shader needs to know nothing about the difference.
#[test]
fn the_glyph_offset_folds_into_the_corner() {
    let plain = layout_vertex(
        (0.0, 0.0),
        (0.0, 4.0),
        0.0,
        (0, 0),
        SizeRange::constant(16.0),
        true,
        (0.0, 0.0),
        (0.0, 0.0),
    );
    let along_line = layout_vertex(
        (0.0, 0.0),
        (0.0, 4.0),
        2.0,
        (0, 0),
        SizeRange::constant(16.0),
        true,
        (0.0, 0.0),
        (0.0, 0.0),
    );

    assert_eq!(plain.pos_offset[3], 128);
    assert_eq!(along_line.pos_offset[3], 128 + 64, "two more pixels");
    assert_eq!(
        plain.pos_offset[2], along_line.pos_offset[2],
        "x is untouched"
    );
}

/// The size is packed times 128, shifted up one, with the SDF flag in the vacated bit.
#[test]
fn the_size_carries_the_sdf_flag_in_its_low_bit() {
    let (min, max) = pack_size(
        SizeRange {
            min: 16.0,
            max: 24.0,
        },
        true,
    );
    assert_eq!(min, ((16 * SIZE_PACK_FACTOR) << 1) + 1);
    assert_eq!(max, 24 * SIZE_PACK_FACTOR);

    let (min, _) = pack_size(
        SizeRange {
            min: 16.0,
            max: 24.0,
        },
        false,
    );
    assert_eq!(
        min,
        (16 * SIZE_PACK_FACTOR) << 1,
        "and zero when it is not SDF"
    );
    assert_eq!(min & 1, 0);
}

/// Sizes are capped so the shift cannot carry them out of a `u16`.
///
/// 255 times 128 shifted up one is the largest value that still fits, which is why the maximum
/// glyph size is 255 rather than something rounder.
#[test]
fn an_oversized_symbol_is_capped_rather_than_wrapping() {
    let (min, max) = pack_size(
        SizeRange {
            min: 1000.0,
            max: 1000.0,
        },
        true,
    );
    assert_eq!(max, MAX_PACKED_SIZE);
    assert_eq!(min, (MAX_PACKED_SIZE << 1) + 1);
    // The whole thing still fits, which is the point of the cap.
    assert!(u32::from(min) <= u32::from(u16::MAX));
}

/// Pixel offset in sixteenths, minimum font scale in two-hundred-and-fifty-sixths.
///
/// Three different fixed-point scales in one vertex, each the precision that term needs against
/// the range it covers. Confusing two of them is a silent factor of sixteen.
#[test]
fn the_pixel_offset_and_font_scale_have_their_own_scales() {
    let vertex = layout_vertex(
        (0.0, 0.0),
        (0.0, 0.0),
        0.0,
        (0, 0),
        SizeRange::constant(16.0),
        true,
        (2.0, -3.0),
        (1.0, 0.5),
    );

    assert_eq!(vertex.pixel_offset[0], 32, "2 pixels in sixteenths");
    assert_eq!(vertex.pixel_offset[1], -48);
    assert_eq!(vertex.pixel_offset[2], 256, "a scale of 1");
    assert_eq!(vertex.pixel_offset[3], 128, "a scale of a half");
}

/// The opacity vertex carries a fade and a flag in one float.
#[test]
fn the_opacity_vertex_packs_both() {
    // Fully opaque and placed: 127 shifted up, plus the flag.
    assert_eq!(opacity_vertex(true, 1.0), f32::from((127u8 << 1) | 1));
    // Opaque but not placed.
    assert_eq!(opacity_vertex(false, 1.0), f32::from(127u8 << 1));
    // Transparent and placed is the state a label starts a fade in.
    assert_eq!(opacity_vertex(true, 0.0), 1.0);
}

/// The dynamic vertex is the placed position and the label's angle.
#[test]
fn the_dynamic_vertex_is_position_and_angle() {
    assert_eq!(dynamic_vertex((100.0, 200.0), 0.5), [100.0, 200.0, 0.5]);
}

/// A quad is four vertices and two triangles sharing a diagonal.
#[test]
fn a_quad_is_four_vertices_and_two_triangles() {
    let mut buffers = SymbolBuffers::default();
    buffers.add_quad(
        (1000.0, 2000.0),
        [(-4.0, 4.0), (28.0, 4.0), (-4.0, 36.0), (28.0, 36.0)],
        0.0,
        (10, 20, 32, 32),
        SizeRange::constant(16.0),
        true,
        1.0,
    );

    assert_eq!(buffers.vertices.len(), 4);
    assert_eq!(buffers.dynamic.len(), 4, "one per vertex");
    assert_eq!(buffers.opacity.len(), 4);
    assert_eq!(buffers.indices, [0, 1, 2, 1, 2, 3]);
    assert_eq!(buffers.glyphs(), 1);
}

/// Each corner takes the texel of its own corner of the atlas rectangle.
///
/// The pairing a caller cannot get wrong because it is not a caller's to make: a top-left corner
/// paired with a bottom-right texel draws the glyph mirrored, which is a bug that looks like a
/// font problem.
#[test]
fn each_corner_samples_its_own_texel() {
    let mut buffers = SymbolBuffers::default();
    buffers.add_quad(
        (0.0, 0.0),
        [(0.0, 0.0), (10.0, 0.0), (0.0, 10.0), (10.0, 10.0)],
        0.0,
        (100, 200, 32, 16),
        SizeRange::constant(16.0),
        true,
        1.0,
    );

    let texels: Vec<(u16, u16)> = buffers
        .vertices
        .iter()
        .map(|vertex| (vertex.data[0], vertex.data[1]))
        .collect();
    assert_eq!(
        texels,
        [(100, 200), (132, 200), (100, 216), (132, 216)],
        "top-left, top-right, bottom-left, bottom-right"
    );
}

/// A second quad's indices refer to its own vertices.
///
/// The failure this rules out is a base index that does not advance: every glyph after the first
/// would draw the first glyph's shape.
#[test]
fn a_second_quad_indexes_its_own_vertices() {
    let mut buffers = SymbolBuffers::default();
    for _ in 0..2 {
        buffers.add_quad(
            (0.0, 0.0),
            [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)],
            0.0,
            (0, 0, 8, 8),
            SizeRange::constant(16.0),
            true,
            1.0,
        );
    }

    assert_eq!(buffers.indices, [0, 1, 2, 1, 2, 3, 4, 5, 6, 5, 6, 7]);
    assert_eq!(buffers.glyphs(), 2);
}

/// The attributes the shader declares are the ones being filled.
///
/// Generated from mbgl under DR-6, so this fails if the shader's layout changes upstream rather
/// than quietly producing vertices it no longer reads.
#[test]
fn the_layout_matches_the_shader() {
    let names: Vec<&str> = SYMBOL_ICON_SHADER
        .iter()
        .map(|attribute| attribute.name)
        .collect();
    assert!(names.contains(&"idSymbolPosOffsetVertexAttribute"));
    assert!(names.contains(&"idSymbolDataVertexAttribute"));
    assert!(names.contains(&"idSymbolPixelOffsetVertexAttribute"));
    assert!(names.contains(&"idSymbolProjectedPosVertexAttribute"));
    assert!(names.contains(&"idSymbolFadeOpacityVertexAttribute"));

    use tessella_capture_abi::generated::mbgl_enums::AttributeDataType;
    let by_name = |name: &str| {
        SYMBOL_ICON_SHADER
            .iter()
            .find(|attribute| attribute.name == name)
            .expect("declared")
    };
    assert_eq!(
        by_name("idSymbolPosOffsetVertexAttribute").declared,
        AttributeDataType::Short4
    );
    assert_eq!(
        by_name("idSymbolDataVertexAttribute").declared,
        AttributeDataType::UShort4
    );
    assert_eq!(
        by_name("idSymbolPixelOffsetVertexAttribute").declared,
        AttributeDataType::Short4
    );
    assert_eq!(
        by_name("idSymbolProjectedPosVertexAttribute").declared,
        AttributeDataType::Float3
    );
    assert_eq!(
        by_name("idSymbolFadeOpacityVertexAttribute").declared,
        AttributeDataType::Float
    );
}
