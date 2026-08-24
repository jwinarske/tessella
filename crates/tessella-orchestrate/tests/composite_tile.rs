//! Zoom-interpolated (composite) paint properties, checked against their own golden dump.
//!
//! # Why a second golden and not an extension of the first
//!
//! The hermetic style has no property that varies with zoom *and* per feature, so its dump
//! cannot say anything about composite binding — its four `_t` fields are zero for the same
//! reason a correct implementation's would be. Rather than change the R0 reference, which is
//! frozen and is what the whole stream is diffed against, this is a second style run through
//! the same probe at the same camera.
//!
//! It is a minimal delta: identical source geometry, identical layer count and order, and the
//! only difference is that two layers' paint became `interpolate` curves over `zoom` whose
//! stops are `match` expressions over a feature property. The vertex and index buffers in the
//! two dumps are byte-identical, which is what makes the paint buffers the only thing under
//! test.
//!
//! # What the oracle settles that nothing else could
//!
//! - The slot doubles, and the *supplied* type becomes the declared one: a composite colour is
//!   `Float4` where a source-only colour is `Float2` supplied against a `Float4` declaration.
//! - The two endpoints are laid out grouped by end — `[min…, max…]` — not interleaved per
//!   component.
//! - The range is `[bucket zoom, bucket zoom + 1]`, and the bucket zoom is the tile's
//!   *overscaled* zoom.
//! - Colours are mixed component-wise on premultiplied channels, with `a * (1 - t) + b * t`.
//!   The algebraically equal `a + (b - a) * t` differs in the last bits and fails this diff.

use tessella_orchestrate::tile::{TileId, bucket_for, build_tile};
use tessella_source::geojson;
use tessella_source::tiling::TilingOptions;
use tessella_style::{Source, Style};

const COMPOSITE: &str = include_str!("../../tessella-style/tests/composite_style.json");

fn style() -> Style {
    Style::parse(COMPOSITE).expect("style parses")
}

fn features() -> Vec<tessella_source::GeoJsonFeature> {
    let style = style();
    let Some(Source::Geojson(source)) = style.source("probe") else {
        panic!("the probe style has one geojson source");
    };
    geojson::read(&source.data).expect("features read")
}

fn build(x: u32, y: u32) -> Vec<tessella_orchestrate::LayerBucket> {
    build_tile(
        &style(),
        TileId::new(13, x, y),
        &features(),
        TilingOptions::default(),
    )
    .expect("tile builds")
}

/// FNV-1a 64, the hash the probe uses over a raw buffer.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h = (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The composite paint buffers are byte-identical to the oracle's, on every tile.
#[test]
fn composite_paint_buffers_are_byte_identical_to_the_oracle() {
    let expected = [
        (
            (4092, 2723),
            0x53a1_7006_7fb2_654du64,
            0x78c3_cd28_2628_d5d5u64,
        ),
        ((4092, 2724), 0x53a1_7006_7fb2_654d, 0x78c3_cd28_2628_d5d5),
        ((4093, 2723), 0xff59_12f7_d26b_b8b7, 0x3b58_16d9_ef9a_019d),
        ((4093, 2724), 0xff59_12f7_d26b_b8b7, 0x3b58_16d9_ef9a_019d),
        ((4094, 2723), 0x3725_a62c_938d_c17f, 0xb397_5247_6d1e_19c5),
        ((4094, 2724), 0x3725_a62c_938d_c17f, 0x3b58_16d9_ef9a_019d),
    ];

    for ((x, y), fill_hash, line_hash) in expected {
        let buckets = build(x, y);

        let fill = bucket_for(&buckets, "fill-composite").expect("fill-composite");
        assert_eq!(fill.binder.stride(), 40, "fill stride at {x}/{y}");
        assert_eq!(
            fnv1a(fill.binder.data()),
            fill_hash,
            "fill paint at {x}/{y}"
        );

        let line = bucket_for(&buckets, "line-composite").expect("line-composite");
        assert_eq!(line.binder.stride(), 32, "line stride at {x}/{y}");
        assert_eq!(
            fnv1a(line.binder.data()),
            line_hash,
            "line paint at {x}/{y}"
        );
    }
}

/// Composite slots are twice the width of source-only ones, at the oracle's offsets.
///
/// Named so that a layout change fails with the property that moved rather than with a hash.
/// The offsets are the oracle's: fill 0/16/24 at stride 40, line 0/16/24 at stride 32.
#[test]
fn composite_slots_sit_where_the_oracle_reads_them() {
    let buckets = build(4093, 2723);

    let fill = bucket_for(&buckets, "fill-composite").expect("fill-composite");
    let layout: Vec<(&str, usize, usize, bool)> = fill
        .binder
        .slots()
        .iter()
        .map(|s| (s.name, s.offset, s.width, s.interpolated))
        .collect();
    assert_eq!(
        layout,
        [
            ("fill-color", 0, 16, true),
            ("fill-opacity", 16, 8, true),
            ("fill-outline-color", 24, 16, true),
        ]
    );

    let line = bucket_for(&buckets, "line-composite").expect("line-composite");
    let layout: Vec<(&str, usize, usize, bool)> = line
        .binder
        .slots()
        .iter()
        .map(|s| (s.name, s.offset, s.width, s.interpolated))
        .collect();
    assert_eq!(
        layout,
        [
            ("line-color", 0, 16, true),
            ("line-floorwidth", 16, 8, true),
            ("line-width", 24, 8, true),
        ]
    );
}

/// The two endpoints differ, and the low one is the value at the bucket zoom.
///
/// A binder that evaluated both ends at the same zoom would produce a buffer of the right
/// length whose halves were equal — and would render correctly at integer zoom, which is
/// exactly where a person would check.
#[test]
fn the_two_endpoints_are_a_range_and_not_a_repeat() {
    let buckets = build(4093, 2723);
    let line = bucket_for(&buckets, "line-composite").expect("line-composite");
    let stride = line.binder.stride();

    for vertex in line.binder.data().chunks_exact(stride) {
        // line-width at offset 24: [min, max]. The style's curve runs 4 -> 12 (kind "a") over
        // zoom 13..15, so at a bucket zoom of 13 the ends are 4 and 8.
        let min = f32::from_le_bytes(vertex[24..28].try_into().expect("four bytes"));
        let max = f32::from_le_bytes(vertex[28..32].try_into().expect("four bytes"));
        assert_eq!((min, max), (4.0, 8.0));

        // The colour's two ends are two packed pairs, and they are not the same pair.
        assert_ne!(&vertex[0..8], &vertex[8..16], "colour ends coincide");
    }
}

/// The bucket zoom is the tile's overscaled zoom, and it changes the endpoints.
///
/// This is the fact that puts `overscaled_z` in the store key. The same canonical tile used at
/// a different zoom is a different bucket, and a store that keyed only on `(z, x, y)` would
/// serve one view the other's values.
#[test]
fn the_endpoints_follow_the_overscaled_zoom() {
    let at = |overscaled_z: u8| {
        let buckets = build_tile(
            &style(),
            TileId::overscaled(13, 4093, 2723, overscaled_z),
            &features(),
            TilingOptions::default(),
        )
        .expect("tile builds");
        let line = bucket_for(&buckets, "line-composite").expect("line-composite");
        let v = line.binder.data();
        (
            f32::from_le_bytes(v[24..28].try_into().expect("four bytes")),
            f32::from_le_bytes(v[28..32].try_into().expect("four bytes")),
        )
    };

    // The curve runs width 4 at zoom 13 to 12 at zoom 15, so a bucket at 13 spans [4, 8] and
    // one at 14 spans [8, 12]. Above the last stop the curve clamps, so 15 spans [12, 12].
    assert_eq!(at(13), (4.0, 8.0));
    assert_eq!(at(14), (8.0, 12.0));
    assert_eq!(at(15), (12.0, 12.0));
}

/// The zoom-mix factor matches the oracle at a fractional camera zoom.
///
/// # Why this needs its own capture
///
/// At an exactly integer camera zoom over a tile of that zoom, every mix factor is zero — the
/// camera sits on the low end of the range. The R0 probe jumps to zoom 13.0, so its dump and a
/// composite style's dump have *byte-identical* uniform buffers, and an implementation that
/// never computed this at all would pass against either. The third golden is the same style at
/// zoom 13.5, where mbgl writes `color_t = opacity_t = 0.5`.
#[test]
fn the_zoom_mix_factor_matches_the_oracle() {
    use tessella_orchestrate::ubo::fill_interpolations;

    let style = style();
    let paint = tessella_style::property::resolve_paint(
        style.layer("fill-composite").expect("fill-composite"),
    )
    .expect("resolves");

    // The oracle's drawable UBO at zoom 13.5 carries the block
    // `0000003f 0000003f 00000000 00000000` twelve times — two factors of 0.5 and two pads.
    assert_eq!(fill_interpolations(&paint, 13.0, 13.5, 1), [0.5, 0.5]);
    assert_eq!(fill_interpolations(&paint, 13.0, 13.5, 2), [0.5, 0.5]);

    // At the ends of the range, and clamped beyond them.
    assert_eq!(fill_interpolations(&paint, 13.0, 13.0, 1), [0.0, 0.0]);
    assert_eq!(fill_interpolations(&paint, 13.0, 14.0, 1), [1.0, 1.0]);
    assert_eq!(fill_interpolations(&paint, 13.0, 20.0, 1), [1.0, 1.0]);
    assert_eq!(fill_interpolations(&paint, 13.0, 2.0, 1), [0.0, 0.0]);

    // And it follows the bucket zoom, not just the camera's.
    assert_eq!(fill_interpolations(&paint, 14.0, 13.5, 1), [0.0, 0.0]);
    assert_eq!(fill_interpolations(&paint, 14.0, 14.5, 1), [0.5, 0.5]);
}

/// A layer with no zoom-varying property mixes nothing, whatever the camera is doing.
///
/// The hermetic style's data-driven fill is the case: its `match` on a feature property has one
/// value at every zoom, so a factor other than zero would blend a vertex's two halves when only
/// the first half was written.
#[test]
fn a_source_only_layer_has_no_mix() {
    use tessella_orchestrate::ubo::fill_interpolations;

    let hermetic = Style::parse(include_str!(
        "../../tessella-style/tests/hermetic_style.json"
    ))
    .expect("style parses");
    for id in ["fill-constant", "fill-datadriven"] {
        let paint = tessella_style::property::resolve_paint(hermetic.layer(id).expect(id))
            .expect("resolves");
        assert_eq!(
            fill_interpolations(&paint, 13.0, 13.5, 1),
            [0.0, 0.0],
            "{id}"
        );
        assert_eq!(
            fill_interpolations(&paint, 13.0, 13.5, 2),
            [0.0, 0.0],
            "{id}"
        );
    }
}

/// A `step` curve over zoom selects rather than blends, so its factor is zero.
///
/// mbgl returns zero for a step explicitly. Letting the range formula run instead would give a
/// rising factor that blends between two values the style asked to switch between.
#[test]
fn a_step_curve_does_not_mix() {
    use tessella_orchestrate::ubo::fill_interpolations;

    let stepped = Style::parse(
        r##"{"version": 8, "sources": {}, "layers": [
             {"id": "l", "type": "fill", "source": "s", "paint": {
                "fill-color": ["step", ["zoom"],
                  ["match", ["get", "kind"], "a", "#c04030", "#3050c0"],
                  14, ["match", ["get", "kind"], "a", "#20a080", "#a02080"]]}}]}"##,
    )
    .expect("style parses");
    let paint =
        tessella_style::property::resolve_paint(stepped.layer("l").expect("l")).expect("resolves");
    assert!(
        matches!(
            paint.get("fill-color").expect("fill-color").binding,
            tessella_style::Binding::Attribute { interpolated: true }
        ),
        "a step over zoom with a feature-dependent output is still composite"
    );
    assert_eq!(fill_interpolations(&paint, 13.0, 13.5, 1), [0.0, 0.0]);
}

/// The whole fill drawable buffer matches the oracle at zoom 13.5.
///
/// Matrix and mix factors together, over the cover, at the union's stride — the same comparison
/// `ubo_buffers` makes against the R0 dump, but at a camera zoom where the factors are not
/// zero. Passing this and the byte-exact paint buffers together is what says the two halves of
/// a composite property agree: the endpoints in the vertices, and the scalar that picks between
/// them.
#[test]
fn the_fill_drawable_buffer_matches_the_oracle_at_a_fractional_zoom() {
    use std::collections::BTreeMap;
    use tessella_capture_abi::generated::{ubo_layouts, ubo_slots};
    use tessella_orchestrate::ubo::{self, DrawableEntry, fill_interpolations};
    use tessella_tile::cover::{self, ViewTransform};

    const DUMP: &str = include_str!("../../../tests/golden/composite_style_z13_5.dump");

    let view = tessella_tile::camera::settled(&ViewTransform {
        longitude: -0.11,
        latitude: 51.505,
        zoom: 13.5,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
    });

    // `(layer, slot) -> sorted 16-byte blocks`, as the probe canonicalizes them.
    let mut oracle: BTreeMap<(i32, u32), (usize, Vec<String>)> = BTreeMap::new();
    for line in DUMP.lines() {
        let Some(rest) = line.strip_prefix("ubo ") else {
            continue;
        };
        let (head, tail) = rest.split_once(" bytes=").expect("a bytes= field");
        let mut fields = head.split_whitespace();
        let layer = fields
            .next()
            .and_then(|f| f.strip_prefix("layer:"))
            .map_or(-1, |n| n.parse().expect("a layer index"));
        let slot: u32 = fields
            .next()
            .and_then(|f| f.strip_prefix("slot="))
            .expect("a slot")
            .parse()
            .expect("a slot number");
        let size: usize = fields
            .next()
            .and_then(|f| f.strip_prefix("size="))
            .expect("a size")
            .parse()
            .expect("a size number");
        let hex = tail.split_whitespace().next().expect("the bytes");
        let mut blocks: Vec<String> = hex
            .as_bytes()
            .chunks(32)
            .map(|c| String::from_utf8(c.to_vec()).expect("hex"))
            .collect();
        blocks.sort();
        oracle.insert((layer, slot), (size, blocks));
    }

    let paint = tessella_style::property::resolve_paint(
        style().layer("fill-composite").expect("fill-composite"),
    )
    .expect("resolves");

    let tiles = cover::cover(&view).expect("covers");
    let mut entries = Vec::new();
    for sub_layer_index in [1, 2] {
        for tile in &tiles {
            // The bucket zoom is the tile's own here: at camera zoom 13.5 the cover is z13 and
            // nothing is standing in for anything.
            let interpolations =
                fill_interpolations(&paint, f64::from(tile.z), view.zoom, sub_layer_index);
            entries.push(
                DrawableEntry::for_tile_with(
                    &view,
                    tile.z,
                    tile.x,
                    tile.y,
                    tile.wrap,
                    2,
                    sub_layer_index,
                    interpolations,
                )
                .expect("an unrotated camera"),
            );
        }
    }

    let stride = ubo_layouts::FILL_DRAWABLE_UNION_UBO.stride;
    let packed = ubo::pack_drawable_buffer(&entries, stride);
    let (size, want) = oracle
        .get(&(2, ubo_slots::ID_FILL_DRAWABLE_UBO))
        .expect("the oracle writes a fill drawable buffer for layer 2");

    let mut got: Vec<String> = packed
        .chunks(16)
        .map(|chunk| chunk.iter().map(|byte| format!("{byte:02x}")).collect())
        .collect();
    got.sort();

    assert_eq!(packed.len(), *size, "entries at the union's stride");
    assert_eq!(got, *want);
}
