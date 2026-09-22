//! Uniform buffers, checked against the golden dump (§6.3, §9.1).
//!
//! # Why the comparison sorts
//!
//! mbgl's iteration over a layer's tiles is not deterministic: the same style at the same camera
//! permutes the consolidated buffer between runs, because the entry index is assigned from that
//! iteration. The probe canonicalizes by sorting 16-byte blocks, so this does too.
//!
//! That is not a weaker comparison than it looks. The blocks are sixteen bytes of exact float
//! patterns; a wrong matrix, a wrong color, or a missing entry all change the multiset. What it
//! deliberately does not check is which slot an entry landed in, because that is not a property
//! of the map — and asserting it would make the test fail on a rerun of the oracle rather than on
//! a defect.

use std::collections::BTreeMap;

use tessella_capture_abi::ProjectionMode;
use tessella_capture_abi::generated::{ubo_layouts, ubo_slots};
use tessella_orchestrate::ubo::{self, DrawableEntry, GlobalPaintParams};
use tessella_style::property::Color;
use tessella_tile::cover::{self, ViewTransform};

const DUMP: &str = include_str!("../../../tests/golden/hermetic_style.dump");

fn probe() -> ViewTransform {
    tessella_tile::camera::settled(&ViewTransform {
        longitude: -0.11,
        latitude: 51.505,
        zoom: 13.0,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
        ground_below: 0.0,
    })
}

/// `(layer, slot) -> (size, sorted 16-byte blocks)`. `layer` is `-1` for the global buffers.
fn oracle_buffers() -> BTreeMap<(i32, u32), (usize, Vec<String>)> {
    let mut out = BTreeMap::new();
    for line in DUMP.lines() {
        let Some(rest) = line.strip_prefix("ubo ") else {
            continue;
        };
        let mut fields = rest.split(' ');
        let key = fields.next().expect("a key");
        let (kind, index) = key.split_once(':').expect("kind:index");
        // `layer:<index>/<style layer id>` since the probe started naming groups. The name
        // is what tells two groups apart when they share an index, which a heatmap's
        // render-target groups do; this helper keys on the index alone and asserts below
        // that no two collide rather than dropping one.
        let index = index.split_once('/').map_or(index, |(n, _)| n);
        let layer = if kind == "global" {
            -1
        } else {
            index.parse::<i32>().expect("layer number")
        };
        let slot: u32 = fields
            .next()
            .and_then(|f| f.strip_prefix("slot="))
            .expect("a slot")
            .parse()
            .expect("slot number");
        let size: usize = fields
            .next()
            .and_then(|f| f.strip_prefix("size="))
            .expect("a size")
            .parse()
            .expect("size number");
        let bytes = fields
            .next()
            .and_then(|f| f.strip_prefix("bytes="))
            .expect("bytes");

        let mut blocks: Vec<String> = bytes
            .as_bytes()
            .chunks(32)
            .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
            .collect();
        blocks.sort();
        assert!(
            out.insert((layer, slot), (size, blocks)).is_none(),
            "two layer groups share index {layer} at slot {slot} -- key by the group name"
        );
    }
    out
}

/// Sorted 16-byte blocks of a packed buffer, in the dump's spelling.
fn blocks(bytes: &[u8]) -> Vec<String> {
    let mut blocks: Vec<String> = bytes
        .chunks(16)
        .map(|chunk| chunk.iter().map(|byte| format!("{byte:02x}")).collect())
        .collect();
    blocks.sort();
    blocks
}

/// The six tiles of the probe's cover, biased for a layer and sublayer.
fn cover_entries(layer_index: i32, sub_layer_index: i32) -> Vec<DrawableEntry> {
    let view = probe();
    cover::cover(&view)
        .expect("covers")
        .into_iter()
        .map(|tile| {
            DrawableEntry::for_tile(
                &view,
                ProjectionMode::Mercator,
                tile.z,
                tile.x,
                tile.y,
                tile.wrap,
                layer_index,
                sub_layer_index,
            )
            .expect("an unrotated camera")
        })
        .collect()
}

/// The frame-wide paint parameters match, byte for byte except the elided fade.
///
/// `symbol_fade_change` is elided in the golden file because it settles asynchronously and made
/// the dump nondeterministic; everything around it is compared exactly.
#[test]
fn the_global_paint_params_match_the_oracle() {
    let oracle = oracle_buffers();
    let (size, want) = oracle
        .get(&(-1, ubo_slots::ID_GLOBAL_PAINT_PARAMS_UBO))
        .expect("the oracle writes global paint params");
    assert_eq!(*size, ubo_layouts::GLOBAL_PAINT_PARAMS_UBO.size as usize);

    let packed = GlobalPaintParams::for_view(&probe(), [64.0, 64.0], 1.0).pack();
    assert_eq!(packed.len(), *size);

    // The dump elides the fade as eight dashes. Compare block by block, skipping the one it
    // falls in and checking the rest exactly.
    let mine = blocks(&packed);
    assert_eq!(mine.len(), want.len());
    let mut compared = 0;
    for (got, expected) in mine.iter().zip(want) {
        if expected.contains('-') {
            continue;
        }
        assert_eq!(got, expected);
        compared += 1;
    }
    assert!(compared >= 2, "only {compared} blocks were comparable");
}

/// Every scalar of the global block is the quantity mbgl puts there.
#[test]
fn the_global_scalars_are_what_mbgl_assigns() {
    let params = GlobalPaintParams::for_view(&probe(), [64.0, 64.0], 1.0);

    assert_eq!(params.world_size, [1024.0, 768.0], "the viewport, misnamed");
    assert_eq!(
        params.units_to_pixels,
        [512.0, -384.0],
        "half of it, y flipped"
    );
    assert_eq!(
        params.camera_to_center_distance, 1152.0,
        "1.5 times the height"
    );
    assert_eq!(params.aspect_ratio, 4.0 / 3.0);
    assert_eq!(params.map_zoom, 13.0);
    assert_eq!(params.pixel_ratio, 1.0);
}

/// The background layer's drawable buffer matches: six tile matrices at the union's stride.
#[test]
fn the_background_drawable_buffer_matches_the_oracle() {
    let oracle = oracle_buffers();
    let (size, want) = oracle
        .get(&(0, ubo_slots::ID_BACKGROUND_DRAWABLE_UBO))
        .expect("the oracle writes a background drawable buffer");

    let stride = ubo_layouts::BACKGROUND_DRAWABLE_UNION_UBO.stride;
    // The background is style layer 0, sublayer 0.
    let packed = ubo::pack_drawable_buffer(&cover_entries(0, 0), stride);

    assert_eq!(packed.len(), *size, "six tiles at the union's stride");
    assert_eq!(blocks(&packed), *want);
}

/// A fill layer's drawable buffer matches: twelve entries, the triangles and the outline sharing
/// the tile matrices.
#[test]
fn the_fill_drawable_buffer_matches_the_oracle() {
    let oracle = oracle_buffers();
    let (size, want) = oracle
        .get(&(1, ubo_slots::ID_FILL_DRAWABLE_UBO))
        .expect("the oracle writes a fill drawable buffer");

    // A fill layer is two drawables per tile — triangles at sublayer 1, outline at sublayer 2 —
    // over the same geometry. They differ only in the depth bias, which is the whole reason the
    // outline is not z-fighting with the fill it outlines.
    let mut entries = cover_entries(1, 1);
    entries.extend(cover_entries(1, 2));

    let stride = ubo_layouts::FILL_DRAWABLE_UNION_UBO.stride;
    let packed = ubo::pack_drawable_buffer(&entries, stride);

    assert_eq!(packed.len(), *size, "twelve entries at the union's stride");
    assert_eq!(blocks(&packed), *want);
}

/// The tile-properties buffer is present, full size and empty.
///
/// Its union holds only pattern variants, so a layer with no `fill-pattern` has nothing to put
/// in it — but the shader indexes it by the same entry number as the drawable buffer, so a short
/// one would read past the end.
#[test]
fn the_fill_tile_props_buffer_is_sized_and_empty() {
    let oracle = oracle_buffers();
    let (size, want) = oracle
        .get(&(1, ubo_slots::ID_FILL_TILE_PROPS_UBO))
        .expect("the oracle writes a fill tile props buffer");

    let stride = ubo_layouts::FILL_TILE_PROPS_UNION_UBO.stride;
    let packed = ubo::pack_tile_props_buffer(12, stride);

    assert_eq!(packed.len(), *size);
    assert!(packed.iter().all(|byte| *byte == 0), "nothing to put in it");
    assert_eq!(blocks(&packed), *want, "and the oracle agrees it is empty");
}

/// The fill layer's evaluated properties match, including the outline color inheriting the fill's.
#[test]
fn the_fill_evaluated_props_match_the_oracle() {
    let oracle = oracle_buffers();
    let (size, want) = oracle
        .get(&(1, ubo_slots::ID_FILL_EVALUATED_PROPS_UBO))
        .expect("the oracle writes fill evaluated props");

    // `#2f6f4f` at 0.8 opacity, with the outline color inheriting the fill's because the style
    // sets no `fill-outline-color`. That inheritance was measured on the binding side first;
    // this is the same fact arriving through the uniforms.
    let fill = Color::parse("#2f6f4f").expect("a color");
    let packed = ubo::pack_fill_props(fill, fill, 0.8, 1.0, 0.5, 1.0);

    assert_eq!(packed.len(), *size);
    assert_eq!(blocks(&packed), *want);
}

/// The background layer's properties match.
#[test]
fn the_background_props_match_the_oracle() {
    let oracle = oracle_buffers();
    let (size, want) = oracle
        .get(&(0, ubo_slots::ID_BACKGROUND_PROPS_UBO))
        .expect("the oracle writes background props");

    let color = Color::parse("#101418").expect("a color");
    let packed = ubo::pack_background_props(color, 1.0);

    assert_eq!(packed.len(), *size);
    assert_eq!(blocks(&packed), *want);
}

/// Buffers go on the ring as one envelope each, with the bytes inline.
#[test]
fn a_buffer_writes_one_envelope_with_its_bytes() {
    use tessella_capture_abi::EnvelopeKind;
    use tessella_capture_abi::envelope::ViewId;
    use tessella_capture_abi::ring::Ring;

    let packed = GlobalPaintParams::for_view(&probe(), [64.0, 64.0], 1.0).pack();
    let mut ring = Ring::new(1 << 14);
    let (producer, consumer) = ring.split();
    ubo::write(
        producer,
        ViewId(0),
        ubo::FRAME_WIDE,
        ubo_slots::ID_GLOBAL_PAINT_PARAMS_UBO,
        &packed,
    )
    .expect("writes");

    let record = consumer.peek().expect("an envelope");
    assert_eq!(record.kind, EnvelopeKind::UboUpdate);
    assert_eq!(record.payload, &packed[..], "the buffer travels inline");
}

/// The line drawable buffer matches the oracle: matrix, ratio and six mix factors.
///
/// One drawable per tile, not two — a line has no outline sublayer — at the line union's
/// stride of 128 rather than the fill union's 96.
#[test]
fn the_line_drawable_buffer_matches_the_oracle() {
    let oracle = oracle_buffers();
    let (size, want) = oracle
        .get(&(3, ubo_slots::ID_LINE_DRAWABLE_UBO))
        .expect("the oracle writes a line drawable buffer");

    let view = probe();
    let entries: Vec<ubo::LineDrawableEntry> = cover::cover(&view)
        .expect("covers")
        .into_iter()
        .map(|tile| {
            ubo::LineDrawableEntry::for_tile(
                &view,
                ProjectionMode::Mercator,
                tile.z,
                tile.x,
                tile.y,
                tile.wrap,
                3,
                0,
                // Nothing in the hermetic style's line paint varies with zoom.
                [0.0; 6],
                [0.0, 0.0],
            )
            .expect("an unrotated camera")
        })
        .collect();

    let stride = ubo_layouts::LINE_DRAWABLE_UNION_UBO.stride;
    let packed = ubo::pack_line_drawable_buffer(&entries, stride);

    assert_eq!(packed.len(), *size, "six entries at the union's stride");
    assert_eq!(blocks(&packed), *want);
}

/// The ratio is tile units per screen pixel inverted, and it is `1/16` at a tile's own zoom.
///
/// The line shader turns `line-width` into an extrusion with it, so getting it wrong scales
/// every line by a power of two — which looks like a projection bug rather than a uniform one.
#[test]
fn the_line_ratio_is_the_zoom_scale_over_sixteen() {
    assert_eq!(ubo::line_ratio(13, 13.0), 0.0625);
    assert_eq!(ubo::line_ratio(13, 14.0), 0.125);
    assert_eq!(ubo::line_ratio(13, 12.0), 0.03125);
    // A tile standing in above its own zoom scales up with the camera, not with itself.
    assert_eq!(ubo::line_ratio(11, 13.0), 0.25);
}

/// The line evaluated properties match the oracle.
///
/// Every value is the constant-or-default, so the style's data-driven `line-color` and
/// `line-width` contribute black and `1` here while their real values travel in the vertices.
#[test]
fn the_line_evaluated_props_match_the_oracle() {
    let oracle = oracle_buffers();
    let (size, want) = oracle
        .get(&(3, ubo_slots::ID_LINE_EVALUATED_PROPS_UBO))
        .expect("the oracle writes line evaluated props");

    let packed = ubo::pack_line_props(
        tessella_style::property::Color::black(),
        0.0, // line-blur
        1.0, // line-opacity
        0.0, // line-gap-width
        0.0, // line-offset
        1.0, // line-width, data-driven, so its default
        1.0, // line-floorwidth, likewise
    );

    assert_eq!(packed.len(), *size);
    assert_eq!(blocks(&packed), *want);
}

/// The evaluated-props buffers derive from the style, not from hardcoded values.
///
/// Both layers of each kind are checked, because the data-driven one is where the rule bites:
/// its color and opacity are attributes, so this block must carry the *spec defaults* rather
/// than either feature's value. A packer that evaluated the expression anyway would put one
/// feature's color into a layer-wide uniform and be right only where that feature happened to
/// be drawn.
#[test]
fn the_props_buffers_derive_from_the_style() {
    use tessella_style::Style;
    let style = Style::parse(include_str!(
        "../../tessella-style/tests/hermetic_style.json"
    ))
    .expect("style parses");
    let paint = |id: &str| {
        tessella_style::property::resolve_paint(style.layer(id).expect(id)).expect("resolves")
    };

    let oracle = oracle_buffers();

    // The constant fill layer: its own color and opacity, and they match the oracle's block.
    let (size, want) = oracle
        .get(&(1, ubo_slots::ID_FILL_EVALUATED_PROPS_UBO))
        .expect("fill-constant props");
    let packed = ubo::fill_props_from_paint(&paint("fill-constant"), 13.0);
    assert_eq!(packed.len(), *size);
    assert_eq!(blocks(&packed), *want, "fill-constant");

    // The data-driven fill layer: black and one, the spec defaults.
    let (size, want) = oracle
        .get(&(2, ubo_slots::ID_FILL_EVALUATED_PROPS_UBO))
        .expect("fill-datadriven props");
    let packed = ubo::fill_props_from_paint(&paint("fill-datadriven"), 13.0);
    assert_eq!(packed.len(), *size);
    assert_eq!(blocks(&packed), *want, "fill-datadriven");

    // And the line layer, whose color and width are attributes.
    let (size, want) = oracle
        .get(&(3, ubo_slots::ID_LINE_EVALUATED_PROPS_UBO))
        .expect("line props");
    let packed = ubo::line_props_from_paint(&paint("line-datadriven"), 13.0);
    assert_eq!(packed.len(), *size);
    assert_eq!(blocks(&packed), *want, "line-datadriven");
}

/// The line tile-properties buffer is present, full size and empty.
///
/// Its union holds only the pattern and SDF variants, so a plain line has nothing to put in it
/// — but the shader indexes it by the same entry number as the drawable buffer, so a short one
/// would read past the end.
#[test]
fn the_line_tile_props_buffer_is_sized_and_empty() {
    let oracle = oracle_buffers();
    let (size, want) = oracle
        .get(&(3, ubo_slots::ID_LINE_TILE_PROPS_UBO))
        .expect("the oracle writes a line tile props buffer");

    let stride = ubo_layouts::LINE_TILE_PROPS_UNION_UBO.stride;
    let packed = ubo::pack_tile_props_buffer(6, stride);

    assert_eq!(packed.len(), *size, "six entries at the union's stride");
    assert_eq!(blocks(&packed), *want);
    assert!(packed.iter().all(|byte| *byte == 0));
}

/// The circle drawable buffer matches the oracle: one entry, matrix and extrude scale.
///
/// One entry, not six — the style's point lies inside a single tile, and the layout drops
/// points outside the tile proper rather than keeping the buffered box every other layer uses.
#[test]
fn the_circle_drawable_buffer_matches_the_oracle() {
    let oracle = oracle_buffers();
    let (size, want) = oracle
        .get(&(4, ubo_slots::ID_CIRCLE_DRAWABLE_UBO))
        .expect("the oracle writes a circle drawable buffer");

    let view = probe();
    // The tile the point falls in.
    let entry = ubo::CircleDrawableEntry::for_tile(
        &view,
        ProjectionMode::Mercator,
        13,
        4093,
        2724,
        0,
        4,
        0,
        // The style leaves `circle-pitch-alignment` at its viewport default.
        ubo::circle_extrude_scale(false, 13, &view),
        [0.0; 7],
        [0.0, 0.0],
    )
    .expect("an unrotated camera");

    let stride = ubo_layouts::CIRCLE_DRAWABLE_UBO.stride;
    let packed = ubo::pack_circle_drawable_buffer(&[entry], stride);

    assert_eq!(packed.len(), *size, "one entry");
    assert_eq!(blocks(&packed), *want);
}

/// The extrude scale is in screen units when the circle faces the viewport, tile units when it
/// lies on the map.
#[test]
fn the_circle_extrude_scale_follows_the_pitch_alignment() {
    let view = probe();
    // `pixelsToGLUnits`: two over the width, minus two over the height.
    assert_eq!(
        ubo::circle_extrude_scale(false, 13, &view),
        [2.0 / 1024.0, -2.0 / 768.0]
    );
    // Map-aligned: tile units per pixel, which is sixteen at a tile's own zoom.
    assert_eq!(ubo::circle_extrude_scale(true, 13, &view), [16.0, 16.0]);
}

/// The circle evaluated properties match, flags included.
#[test]
fn the_circle_evaluated_props_match_the_oracle() {
    use tessella_style::Style;
    let oracle = oracle_buffers();
    let (size, want) = oracle
        .get(&(4, ubo_slots::ID_CIRCLE_EVALUATED_PROPS_UBO))
        .expect("the oracle writes circle evaluated props");

    let style = Style::parse(include_str!(
        "../../tessella-style/tests/hermetic_style.json"
    ))
    .expect("style parses");
    let paint = tessella_style::property::resolve_paint(
        style.layer("circle-constant").expect("circle-constant"),
    )
    .expect("resolves");

    let packed = ubo::circle_props_from_paint(&paint, 13.0);
    assert_eq!(packed.len(), *size);
    assert_eq!(blocks(&packed), *want);
}

/// The two flags are integers, and they are not the same value.
///
/// `circle-pitch-scale` defaults to `map` and `circle-pitch-alignment` to `viewport`, so the
/// oracle's block has a one beside a zero — which is what catches a packer that defaulted both
/// the same way, or that wrote them as floats.
#[test]
fn the_circle_flags_are_integers_and_differ() {
    let packed = ubo::pack_circle_props(
        tessella_style::property::Color::black(),
        tessella_style::property::Color::black(),
        5.0,
        0.0,
        1.0,
        0.0,
        1.0,
        true,
        false,
    );
    assert_eq!(&packed[52..56], &1i32.to_le_bytes(), "scale_with_map");
    assert_eq!(&packed[56..60], &0i32.to_le_bytes(), "pitch_with_map");
    assert_ne!(
        &packed[52..56],
        &1.0f32.to_le_bytes(),
        "an integer one, not a float one"
    );
}

/// The line ratio and the label plane's scale are reciprocals of one number.
///
/// They were two implementations of it — one through the `libm` crate in a `no_std` crate, one
/// through the system libm `tessella-tile` links against — free to round differently in the last
/// bit, with nothing comparing them. A line's width and a pitched label's plane both read it, so
/// a disagreement would draw a hairline mismatch nothing would attribute to a rounding seam.
///
/// Asserted at fractional zooms as well as whole ones. At a whole zoom the exponent is exact and
/// any two implementations agree; the fractional case is the one that separates them, and it is
/// the case `composite_style_z13_5` captures.
#[test]
fn the_line_ratio_is_the_label_planes_reciprocal() {
    for (z, zoom) in [
        (13u8, 13.0f64),
        (13, 13.5),
        (13, 14.25),
        (11, 13.0),
        (0, 0.0),
        (16, 12.75),
    ] {
        #[allow(clippy::cast_possible_truncation)]
        let expected = 1.0 / tessella_tile::camera::pixels_to_tile_units(z, zoom) as f32;
        assert_eq!(
            ubo::line_ratio(z, zoom),
            expected,
            "at tile zoom {z} viewed from {zoom}"
        );
    }
}

/// A hillshade's lights, as the evaluated-props block carries them.
///
/// The block has always had room for four -- `vec4` altitudes and azimuths, `vec4[4]` shadows
/// and highlights -- and only ever carried one, because the paint was read one value at a time.
mod hillshade_lights {
    use tessella_orchestrate::ubo;
    use tessella_style::Layer;
    use tessella_style::property::resolve_paint;

    fn paint_of(paint: &str) -> Paint {
        let layer: Layer = serde_json::from_str(&format!(
            r##"{{"id": "h", "type": "hillshade", "source": "s", "paint": {{{paint}}}}}"##
        ))
        .expect("a layer");
        resolve_paint(&layer).expect("the paint resolves")
    }

    /// A layer's resolved paint, which is what the packers read.
    type Paint =
        std::collections::BTreeMap<&'static str, tessella_style::property::ResolvedProperty>;

    /// The floats of the block, which is what the shader reads.
    fn floats(paint: &Paint) -> Vec<f32> {
        let bytes = ubo::hillshade_props_from_paint(paint, 10.0, 0.0);
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|four| f32::from_le_bytes(*four))
            .collect()
    }

    /// Four directions reach four slots, in the order the style wrote them.
    #[test]
    fn four_directions_reach_four_slots() {
        let paint = paint_of(
            r##""hillshade-illumination-direction": [270, 315, 0, 45],
                "hillshade-illumination-altitude": [30, 30, 30, 30]"##,
        );
        let values = floats(&paint);
        // Accent is the first four; altitudes the next four; azimuths the four after that.
        let azimuths = &values[8..12];
        for (slot, degrees) in [270.0f32, 315.0, 0.0, 45.0].into_iter().enumerate() {
            assert!(
                (azimuths[slot] - degrees.to_radians()).abs() < 1e-6,
                "light {slot} points at {} rather than {degrees}",
                azimuths[slot].to_degrees()
            );
        }
        assert_eq!(ubo::hillshade_light_count(&paint, 10.0), 4);
    }

    /// A list shorter than the longest repeats its own last entry, which is mbgl's `padArray`.
    ///
    /// Not a default: a style that writes four directions and one color means that one color
    /// for all four lights, and zeroing the other three would light three of them into nothing.
    #[test]
    fn a_short_list_repeats_its_last_entry() {
        let paint = paint_of(
            r##""hillshade-illumination-direction": [0, 90, 180, 270],
                "hillshade-shadow-color": "#ff0000""##,
        );
        let values = floats(&paint);
        // Accent, then four altitudes, then four azimuths, then the four shadow colors.
        let shadows = &values[12..28];
        let first: Vec<f32> = shadows[0..4].to_vec();
        assert!(first[0] > 0.9, "the written color is the first light's");
        for slot in 1..4 {
            assert_eq!(
                &shadows[slot * 4..slot * 4 + 4],
                first.as_slice(),
                "light {slot} was not padded from the one before it"
            );
        }
        assert_eq!(ubo::hillshade_light_count(&paint, 10.0), 4);
    }

    /// A layer that writes nothing still lights once, as mbgl's empty-list push does.
    #[test]
    fn a_layer_that_says_nothing_lights_once() {
        let paint = paint_of(r##""hillshade-exaggeration": 0.5"##);
        assert_eq!(ubo::hillshade_light_count(&paint, 10.0), 1);
    }

    /// The method's numbering is mbgl's enum order, which is not the spec's listing order.
    #[test]
    fn the_method_is_mbgls_numbering() {
        for (name, number) in [
            ("standard", 0),
            ("combined", 1),
            ("igor", 2),
            ("multidirectional", 3),
            ("basic", 4),
        ] {
            let paint = paint_of(&format!(r##""hillshade-method": "{name}""##));
            assert_eq!(ubo::hillshade_method(&paint, 10.0), number, "{name}");
        }
        // And the default, which is what a style saying nothing asks for.
        let paint = paint_of(r##""hillshade-exaggeration": 0.5"##);
        assert_eq!(ubo::hillshade_method(&paint, 10.0), 0);
    }
}
