// SPDX-License-Identifier: BSD-2-Clause
//! A location indicator's two drawables, from the paint to the bytes on the wire.
//!
//! The layer is the one that belongs to no tile and no source. Everything else in a frame is
//! either cover geometry or a viewport quad; a puck is one object at a coordinate the paint
//! names, and its vertices are offsets in world pixels at the camera's own scale. So what this
//! checks is the seams -- that the builder reads the paint mbgl reads, that the two drawables
//! are announced in the order their uniform blocks are packed in, and that the border says on
//! the wire that it is a strip.
//!
//! Drawing it as a fan instead is the failure worth naming: seventy-two indices read as a
//! triangle list is twenty-four triangles over a ring, which fills pixels and looks like a
//! shading bug rather than a topology one.

extern crate alloc;

use tessella_capture_abi::envelope::{DrawFlags, Topology, ViewId};
use tessella_capture_abi::{ProjectionMode, generated::ubo_layouts};
use tessella_orchestrate::order::bindings_for;
use tessella_orchestrate::tile::build_location_indicators;
use tessella_orchestrate::{Content, LayerBucket};
use tessella_style::Style;
use tessella_tile::cover::ViewTransform;

const VIEW: ViewId = ViewId(0);

const STYLE: &str = r#"{
 "version": 8,
 "sources": {},
 "layers": [
  { "id": "puck", "type": "location-indicator",
    "paint": {
      "location": [ 52.52, 13.405, 0 ],
      "accuracy-radius": 240,
      "accuracy-radius-color": "rgba(63,111,255,0.28)",
      "accuracy-radius-border-color": "rgba(200,220,255,0.85)"
    }}
 ]
}"#;

fn view() -> ViewTransform {
    ViewTransform {
        longitude: 13.405,
        latitude: 52.52,
        zoom: 14.0,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
    }
}

fn build(style: &Style) -> Vec<LayerBucket> {
    build_location_indicators(style, &view(), ProjectionMode::Mercator, None)
        .expect("the paint compiles")
}

/// The circle is built where the paint puts it, with the counts the oracle's dump carries.
#[test]
fn the_paint_becomes_one_bucket_of_two_drawables() {
    let style = Style::parse(STYLE).expect("style parses");
    let built = build(&style);
    assert_eq!(built.len(), 1);
    let Content::LocationIndicator(circle) = &built[0].content else {
        panic!("a location indicator");
    };
    assert_eq!(circle.vertices.len(), 73);
    assert_eq!(circle.fill_indices.len(), 216);
    assert_eq!(circle.border_indices.len(), 72);
    assert_eq!(built[0].drawable_count(), 2);
}

/// `location` is latitude first, which is mbgl's order and not the style's usual one.
///
/// Read the other way round the puck lands off Somalia and the circle is built at a latitude of
/// 13.4 instead of 52.5 -- a different Mercator scale, so the ring comes out a different size and
/// nothing in the frame says why.
#[test]
fn the_location_is_latitude_then_longitude() {
    let style = Style::parse(STYLE).expect("style parses");
    let built = build(&style);
    let Content::LocationIndicator(circle) = &built[0].content else {
        panic!("a location indicator");
    };
    let world = tessella_tile::camera::world_size(14.0);
    let berlin = tessella_layout::location_indicator::LocationIndicatorBucket::new(
        [13.405, 52.52],
        240.0,
        0.0,
        world,
    );
    assert_eq!(circle.vertices, berlin.vertices);

    // And the transposed reading really is a different circle, so the check above has teeth.
    let swapped = tessella_layout::location_indicator::LocationIndicatorBucket::new(
        [52.52, 13.405],
        240.0,
        0.0,
        world,
    );
    assert_ne!(circle.vertices, swapped.vertices);
}

/// Both drawables bind, unclipped and depthless, in the order the uniform blocks are packed in.
///
/// mbgl builds every one of a puck's drawables with `setEnableDepth(false)` and
/// `setEnableStencil(false)`. A stencil is the one that would show: the anchor is `0/0/0` and the
/// clip masks are the cover's, so a puck asking to be masked would be tested against a tile mask
/// nothing wrote and vanish.
#[test]
fn the_two_drawables_bind_unclipped_and_in_order() {
    let style = Style::parse(STYLE).expect("style parses");
    let built = build(&style);
    let mut next = 0;
    let bindings = bindings_for(
        VIEW,
        tessella_orchestrate::order::tile_of(0, 0, 0),
        &built,
        &mut next,
        true,
    );
    assert_eq!(bindings.len(), 2);
    for (index, binding) in bindings.iter().enumerate() {
        assert_eq!(binding.sub_layer_index, i32::try_from(index).expect("two"));
        assert!(!binding.flags.contains(DrawFlags::ENABLE_STENCIL));
        assert!(!binding.flags.contains(DrawFlags::ENABLE_DEPTH));
        assert!(binding.flags.contains(DrawFlags::ENABLE_COLOR));
    }
}

/// A layer mbgl would disable draws nothing: no radius, or nothing to see in either color.
#[test]
fn a_disabled_puck_builds_no_bucket() {
    for paint in [
        r#""accuracy-radius": 0"#,
        r#""accuracy-radius": 240, "accuracy-radius-color": "rgba(0,0,0,0)", "accuracy-radius-border-color": "rgba(0,0,0,0)""#,
    ] {
        let text = alloc_style(paint);
        let style = Style::parse(&text).expect("style parses");
        assert!(build(&style).is_empty(), "{paint}");
    }
}

/// A globe draws none. mbgl has no globe and so no puck on one, and the circle is world pixels
/// placed by the plane's projection -- neither of which a sphere has.
#[test]
fn a_globe_draws_no_puck() {
    let style = Style::parse(STYLE).expect("style parses");
    let built = build_location_indicators(&style, &view(), ProjectionMode::Globe, Some(&sheet()))
        .expect("compiles");
    assert!(built.is_empty());
}

/// The interior is triangles and the border is a strip, on the wire.
#[test]
fn the_border_is_a_strip_and_the_interior_is_not() {
    use tessella_capture_abi::envelope::GeometryId;
    let style = Style::parse(STYLE).expect("style parses");
    let built = build(&style);
    let Content::LocationIndicator(circle) = &built[0].content else {
        panic!("a location indicator");
    };
    let mut arena = tessella_orchestrate::SlabArena::new();
    let (interior, vertices) =
        tessella_orchestrate::emit::encode_location_indicator(&mut arena, GeometryId(1), circle);
    let border = tessella_orchestrate::emit::encode_location_indicator_border(
        &mut arena,
        GeometryId(2),
        circle,
        vertices,
    );
    assert_eq!(interior.record.topology(), Some(Topology::Triangles));
    assert_eq!(border.record.topology(), Some(Topology::LineStrip));
    // One vertex buffer under both, which is what sharing the allocation is for.
    assert_eq!(
        interior.record.vertex_count, border.record.vertex_count,
        "the border draws the interior's vertices"
    );
}

/// The two blocks are packed in sub-layer order, with each drawable's own color.
///
/// Packed the other way round, the disc would be drawn in the border's color and the ring in the
/// disc's -- a picture that is entirely plausible and entirely wrong.
#[test]
fn each_drawable_gets_its_own_color() {
    use tessella_orchestrate::ubo::{
        LocationIndicatorEntry, location_indicator_matrix, pack_location_indicator_drawable_buffer,
    };
    let matrix = location_indicator_matrix(&view(), [13.405, 52.52]).expect("a matrix");
    let interior = tessella_style::Color {
        r: 0.25,
        g: 0.5,
        b: 1.0,
        a: 0.28,
    };
    let border = tessella_style::Color {
        r: 0.8,
        g: 0.9,
        b: 1.0,
        a: 0.85,
    };
    let stride = ubo_layouts::LOCATION_INDICATOR_DRAWABLE_UBO.stride;
    let packed = pack_location_indicator_drawable_buffer(
        &[
            LocationIndicatorEntry {
                matrix,
                color: interior,
            },
            LocationIndicatorEntry {
                matrix,
                color: border,
            },
        ],
        stride,
    );
    assert_eq!(packed.len(), 2 * stride as usize);
    let color_at = |slot: usize| -> [f32; 4] {
        let at = slot * stride as usize + 64;
        core::array::from_fn(|index| {
            let bytes = &packed[at + index * 4..at + index * 4 + 4];
            f32::from_le_bytes(bytes.try_into().expect("four bytes"))
        })
    };
    assert_eq!(
        color_at(0),
        [interior.r, interior.g, interior.b, interior.a]
    );
    assert_eq!(color_at(1), [border.r, border.g, border.b, border.a]);
    // And the matrix is where the shader reads it, before the color rather than after.
    let first = f32::from_le_bytes(packed[0..4].try_into().expect("four bytes"));
    assert_eq!(first, matrix[0]);
}

/// A sheet holding the three images the quad tests name, packed the way the atlas packs one.
///
/// Built here rather than parsed: what these tests need from a sprite is its padded rectangle and
/// its pixel ratio, and a fixture would put a JSON parser and an atlas packer between the test and
/// the thing it is checking.
fn sheet() -> alloc::collections::BTreeMap<String, tessella_glyph::sprite::IconPosition> {
    use tessella_glyph::atlas::Rect;
    use tessella_glyph::sprite::IconPosition;
    let mut out = alloc::collections::BTreeMap::new();
    for (name, size, x) in [("shadow", 50, 0), ("bearing", 40, 60), ("top", 20, 120)] {
        out.insert(
            name.to_string(),
            IconPosition {
                // Padded: the rectangle is the picture plus a pixel on every side.
                padded_rect: Rect {
                    x,
                    y: 0,
                    width: size + 2,
                    height: size + 2,
                },
                pixel_ratio: 1.0,
                sdf: false,
                content: None,
                text_fit_width: None,
                text_fit_height: None,
            },
        );
    }
    out
}

const IMAGED: &str = r#"{
 "version": 8,
 "sources": {},
 "layers": [
  { "id": "puck", "type": "location-indicator",
    "layout": { "shadow-image": "shadow", "bearing-image": "bearing", "top-image": "top" },
    "paint": {
      "location": [ 52.52, 13.405, 0 ],
      "accuracy-radius": 0,
      "bearing": 0,
      "shadow-image-size": 1.0,
      "bearing-image-size": 1.0,
      "top-image-size": 1.0,
      "perspective-compensation": 0,
      "image-tilt-displacement": 8
    }}
 ]
}"#;

fn quads(pitch: f64) -> Vec<tessella_layout::location_indicator::PuckQuad> {
    let style = Style::parse(IMAGED).expect("style parses");
    let mut camera = view();
    camera.pitch = pitch;
    let built =
        build_location_indicators(&style, &camera, ProjectionMode::Mercator, Some(&sheet()))
            .expect("the paint compiles");
    let Content::LocationIndicator(puck) = &built[0].content else {
        panic!("a location indicator");
    };
    // No accuracy radius, so the circle is absent and the quads are the whole layer.
    assert!(!puck.has_circle());
    puck.quads.clone()
}

/// A puck with images and no accuracy radius is three drawables, and they are the quads.
#[test]
fn images_draw_without_a_circle() {
    let quads = quads(0.0);
    assert_eq!(quads.len(), 3);
    use tessella_layout::location_indicator::PuckImage;
    assert_eq!(
        quads.iter().map(|quad| quad.image).collect::<Vec<_>>(),
        [PuckImage::Shadow, PuckImage::Bearing, PuckImage::Top]
    );
}

/// Each quad is its image's own size: half a diagonal is the logical width times `sqrt(2) / 2`.
#[test]
fn a_quad_is_the_size_of_its_image() {
    for (quad, width) in quads(0.0).iter().zip([50.0_f64, 40.0, 20.0]) {
        let half = f64::from(quad.corners[2][0] - quad.corners[0][0])
            .hypot(f64::from(quad.corners[2][1] - quad.corners[0][1]))
            / 2.0;
        assert!(
            (half - width * core::f64::consts::SQRT_2 / 2.0).abs() < 1e-3,
            "{:?} {half}",
            quad.image
        );
    }
}

/// The shadow sinks and the hat rises when the camera pitches, and nothing moves when it does not.
///
/// The sign is the thing. mbgl works in viewport coordinates with y down -- the layer declares its
/// own flipped `latLngToScreenCoordinate` -- while `tessella_tile::screen` is `TransformState`'s,
/// where y is up. Transcribing mbgl's two lines across that flip without it put the shadow above
/// the thing casting it, for 72 gross pixels at a pitched camera and none at a flat one.
#[test]
fn the_shadow_sinks_and_the_hat_rises_under_pitch() {
    let centers = |pitch: f64| -> Vec<f32> {
        quads(pitch)
            .iter()
            .map(|quad| quad.corners.iter().map(|corner| corner[1]).sum::<f32>() / 4.0)
            .collect()
    };
    // Flat: every quad is centered on the puck, whatever the displacement says.
    for center in centers(0.0) {
        assert!(center.abs() < 1e-4, "{center}");
    }
    // Pitched: north is a smaller y in world pixels, so the hat's center is negative.
    let [shadow, bearing, top] = centers(60.0)[..] else {
        panic!("three quads")
    };
    assert!(shadow > 0.0, "the shadow sinks: {shadow}");
    assert!(bearing.abs() < 1e-4, "the bearing image stays: {bearing}");
    assert!(top < 0.0, "the hat rises: {top}");
    // And by the same amount, which is `pitch * image-tilt-displacement` either way.
    assert!((shadow + top).abs() < 1e-4, "{shadow} {top}");
    let want = 60.0_f64.to_radians() * 8.0;
    assert!((f64::from(shadow) - want).abs() < 1e-3, "{shadow} {want}");
}

fn alloc_style(paint: &str) -> String {
    format!(
        r#"{{"version":8,"sources":{{}},"layers":[
        {{"id":"puck","type":"location-indicator","paint":{{"location":[52.52,13.405,0],{paint}}}}}]}}"#
    )
}
