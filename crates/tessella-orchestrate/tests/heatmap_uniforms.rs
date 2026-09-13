//! The heatmap layer's uniform blocks, checked against `heatmap_style.dump`.
//!
//! # Why this dump is keyed by layer group *name*
//!
//! `RenderHeatmapLayer::update` builds a render target per style layer and hardcodes the tile
//! group inside it to index 0. Two heatmap layers therefore produce two groups both claiming
//! zero, and the probe used to key its uniform records on the index alone — so the second
//! layer's evaluated properties overwrote the first's, and the background's whole consolidated
//! buffer went with them. Nine buffers arrived and five were recorded.
//!
//! The probe now carries the group's name, which is the style layer id. The other dumps gained
//! the name too and lost nothing, because outside a render target the indices are already
//! unique — which is why nobody noticed.
//!
//! # What the style is for
//!
//! Two heatmap layers over eight points, one constant and one not. That is the pair the blocks
//! need: `HeatmapEvaluatedPropsUBO` carries `weight` and `radius` whether or not they are
//! data-driven, because mbgl writes `constantOr(defaultValue())` there and lets the shader's
//! `#pragma mapbox: initialize` choose. A single constant layer cannot tell a packer that
//! writes the evaluated value from one that writes the *default* — they agree.

use std::collections::BTreeMap;

use tessella_capture_abi::ProjectionMode;
use tessella_capture_abi::generated::{ubo_layouts, ubo_slots};
use tessella_orchestrate::ubo;
use tessella_tile::cover::ViewTransform;

const DUMP: &str = include_str!("../../../tests/golden/heatmap_style.dump");

fn probe() -> ViewTransform {
    tessella_tile::camera::settled(&ViewTransform {
        longitude: -0.11,
        latitude: 51.505,
        zoom: 13.0,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
    })
}

/// `(group index, group name, slot) -> (size, sorted 16-byte blocks)`.
///
/// All three, because neither alone is an identity here. Two heatmap layers share index 0 for
/// their tile groups, and one heatmap layer appears under two indices — 0 for the group inside
/// its render target and its style position for the texture pass — both at slot 5.
fn oracle_buffers() -> BTreeMap<(i32, String, u32), (usize, Vec<String>)> {
    let mut out = BTreeMap::new();
    for line in DUMP.lines() {
        let Some(rest) = line.strip_prefix("ubo ") else {
            continue;
        };
        let mut fields = rest.split(' ');
        let key = fields.next().expect("a key");
        let (kind, index) = key.split_once(':').expect("kind:index");
        let (group, name) = if kind == "global" {
            (-1, String::new())
        } else {
            let (number, name) = index.split_once('/').expect("the probe names layer groups");
            (number.parse::<i32>().expect("group index"), name.to_owned())
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
        assert!(
            out.insert((group, name, slot), (size, blocks_of(bytes)))
                .is_none(),
            "two groups share a name and slot in {key}"
        );
    }
    out
}

/// The probe's canonicalization: 16-byte blocks, sorted.
fn blocks_of(hex: &str) -> Vec<String> {
    let mut blocks: Vec<String> = hex
        .as_bytes()
        .chunks(32)
        .map(|chunk| String::from_utf8(chunk.to_vec()).expect("hex"))
        .collect();
    blocks.sort();
    blocks
}

fn blocks(bytes: &[u8]) -> Vec<String> {
    let mut out: Vec<String> = bytes
        .chunks(16)
        .map(|chunk| chunk.iter().map(|byte| format!("{byte:02x}")).collect())
        .collect();
    out.sort();
    out
}

/// A constant layer's evaluated properties are its own values.
#[test]
fn the_constant_layers_evaluated_props_match_the_oracle() {
    let oracle = oracle_buffers();
    let (size, want) = oracle
        .get(&(
            0,
            "heatmap-constant".to_owned(),
            ubo_slots::ID_HEATMAP_EVALUATED_PROPS_UBO,
        ))
        .expect("the oracle writes the constant layer's evaluated props");

    // weight 1, radius 20, intensity 1 — the style's own numbers.
    let packed = ubo::pack_heatmap_props(1.0, 20.0, 1.0);

    assert_eq!(packed.len(), *size);
    assert_eq!(blocks(&packed), *want);
}

/// A data-driven layer's evaluated properties are the spec *defaults*, not zero and not the
/// value at this zoom.
///
/// `heatmap-weight` is `["get", "w"]` and `heatmap-radius` an interpolate over zoom; mbgl
/// writes `constantOr(defaultValue())` for both, so the block carries weight 1 and radius 30
/// while the attributes carry the real values. Only `heatmap-intensity`, which the spec does
/// not let be data-driven, is the style's own 0.7.
#[test]
fn a_data_driven_layers_evaluated_props_carry_the_defaults() {
    let oracle = oracle_buffers();
    let (size, want) = oracle
        .get(&(
            0,
            "heatmap-composite".to_owned(),
            ubo_slots::ID_HEATMAP_EVALUATED_PROPS_UBO,
        ))
        .expect("the oracle writes the composite layer's evaluated props");

    let packed = ubo::pack_heatmap_props(1.0, 30.0, 0.7);

    assert_eq!(packed.len(), *size);
    assert_eq!(blocks(&packed), *want);
}

/// The second pass's block: a screen ortho and the layer's opacity, no view and no tile.
#[test]
fn the_texture_pass_props_match_the_oracle() {
    let oracle = oracle_buffers();
    // The texture pass's group sits at the layer's style position, not at 0.
    for (group, layer, opacity) in [
        (1, "heatmap-constant", 0.9f32),
        (2, "heatmap-composite", 0.6),
    ] {
        let (size, want) = oracle
            .get(&(
                group,
                layer.to_owned(),
                ubo_slots::ID_HEATMAP_TEXTURE_PROPS_UBO,
            ))
            .expect("the oracle writes a texture props block");
        let (size, want) = (*size, want.clone());

        // The *backend* size, not the half-resolution target: the quad is drawn over the frame.
        let packed = ubo::pack_heatmap_texture_props(1024, 768, opacity);

        assert_eq!(packed.len(), size);
        assert_eq!(blocks(&packed), want);
    }
}

/// The drawable buffer: two tiles carry points, so two entries at the block's stride.
#[test]
fn the_heatmap_drawable_buffer_matches_the_oracle() {
    let oracle = oracle_buffers();
    let (size, want) = oracle
        .get(&(
            0,
            "heatmap-constant".to_owned(),
            ubo_slots::ID_HEATMAP_DRAWABLE_UBO,
        ))
        .expect("the oracle writes a heatmap drawable buffer");

    let view = probe();
    let entries: Vec<_> = [(4093u32, 2723u32), (4093, 2724)]
        .into_iter()
        .map(|(x, y)| {
            ubo::HeatmapDrawableEntry::for_tile(
                &view,
                ProjectionMode::Mercator,
                13,
                x,
                y,
                0,
                // Constant properties mix nothing.
                [0.0; 2],
            )
            .expect("an unrotated camera")
        })
        .collect();

    let stride = ubo_layouts::HEATMAP_DRAWABLE_UBO.stride;
    let packed = ubo::pack_heatmap_drawable_buffer(&entries, stride);

    assert_eq!(packed.len(), *size, "two entries");
    assert_eq!(blocks(&packed), *want);
}

/// The extrude scale is tile units per pixel, with no viewport case to choose.
#[test]
fn the_heatmap_extrude_scale_is_the_map_aligned_one() {
    let view = probe();
    assert_eq!(
        ubo::heatmap_extrude_scale(13, &view),
        ubo::circle_extrude_scale(true, 13, &view)[0]
    );
}

/// Each heatmap layer asks for its own half-viewport `HalfFloat` target.
///
/// Both numbers are the renderer's rather than the style's, and both have to hold for the
/// second pass to sample what the first drew: half in each dimension, and a channel type that
/// does not clip a kernel sum at one.
#[test]
fn the_style_needs_one_half_resolution_target_per_layer() {
    let targets: Vec<&str> = DUMP
        .lines()
        .filter_map(|line| line.strip_prefix("rendertarget "))
        .collect();

    assert_eq!(targets, ["512x384 ct=1", "512x384 ct=1"]);
}

/// The bucket is the circle bucket, and the oracle's vertex and index counts say so.
///
/// Two tiles, two and six points. Four vertices and six indices each, which is what the
/// segment lines carry — `vlen=8 ilen=12` and `vlen=24 ilen=36`.
#[test]
fn the_bucket_produces_the_oracles_vertex_and_index_counts() {
    let mut seen: Vec<(u32, u32)> = DUMP
        .lines()
        .filter(|line| line.starts_with("  seg ") && line.contains(".sh0020."))
        .map(|line| {
            let mut vlen = 0;
            let mut ilen = 0;
            for field in line.split_whitespace() {
                if let Some(value) = field.strip_prefix("vlen=") {
                    vlen = value.parse().expect("vlen");
                } else if let Some(value) = field.strip_prefix("ilen=") {
                    ilen = value.parse().expect("ilen");
                }
            }
            (vlen, ilen)
        })
        .collect();
    seen.sort_unstable();
    seen.dedup();

    assert_eq!(seen, [(8, 12), (24, 36)]);

    for (points, vlen, ilen) in [(2usize, 8, 12), (6, 24, 36)] {
        let bucket = tessella_layout::heatmap::build(
            &(0..points)
                .map(|index| [100 + (index as i16) * 10, 200])
                .collect::<Vec<_>>(),
        );
        assert_eq!(bucket.vertices.len(), vlen);
        assert_eq!(bucket.indices.len(), ilen);
        assert_eq!(bucket.segments.len(), 1);
    }
}
