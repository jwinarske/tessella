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

mod common;

use common::fnv1a;

use std::collections::BTreeMap;

use tessella_capture_abi::ProjectionMode;
use tessella_capture_abi::generated::{ubo_layouts, ubo_slots};
use tessella_orchestrate::ubo;
use tessella_style::ramp::{self, RampParameter};
use tessella_style::{Expression, Value};
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
        ground_below: 0.0,
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

/// `heatmap-color` parses against a color expectation, as mbgl's
/// `Converter<ColorRampPropertyValue>` does with its `ParsingContext(type::Color)`.
fn color_ramp(json: &str) -> Expression {
    let value: Value = serde_json::from_str(json).expect("valid json");
    Expression::parse_for(
        &value,
        &tessella_style::expression::PropertySpec {
            default: None,
            expected: Some(tessella_style::expression::Type::Color),
        },
    )
    .expect("parses")
}

/// Both ramps are byte-identical to the oracle's, which the dump pins by content hash.
///
/// The dump's `texture 256x1` lines are the only two things a heatmap uploads, and they are what
/// the second pass samples: get the ramp wrong and every pixel of the layer is the wrong color
/// while its geometry, its uniforms and its draw order all still match.
///
/// One of the two is the spec's default `heatmap-color`, which the constant layer does not set —
/// so this also pins that the default is the spec's own six-stop ramp rather than an empty one.
#[test]
fn both_color_ramps_match_the_oracle_byte_for_byte() {
    let want: Vec<u64> = DUMP
        .lines()
        .filter_map(|line| line.strip_prefix("texture 256x1 fmt=0 hash="))
        .map(|hash| u64::from_str_radix(hash, 16).expect("a hex hash"))
        .collect();
    assert_eq!(want.len(), 2, "one ramp per heatmap layer");

    // The spec's default, which `heatmap-constant` leaves alone.
    let default = color_ramp(tessella_style::ramp::DEFAULT_HEATMAP_COLOR);
    // `heatmap-composite`'s own.
    let custom = color_ramp(
        r#"["interpolate",["linear"],["heatmap-density"],
           0,"rgba(0, 0, 255, 0)",0.5,"rgb(0, 255, 0)",1,"rgb(255, 0, 0)"]"#,
    );

    let mut got: Vec<u64> = [default, custom]
        .iter()
        .map(|expression| {
            let baked = ramp::bake(expression, RampParameter::HeatmapDensity).expect("bakes");
            assert_eq!(baked.len(), 1024, "256 RGBA texels");
            fnv1a(&baked)
        })
        .collect();
    got.sort_unstable();

    let mut want = want;
    want.sort_unstable();
    assert_eq!(got, want);
}

/// The two passes' blocks reach the ring, in the two different views they belong to.
///
/// #99 checked the packers against the golden's bytes. This checks that a *frame* writes those
/// bytes, and — the part no packer test can reach — that the kernels' blocks go to the layer's
/// offscreen view while the quad's goes to the map's. Swap the two and every block is still
/// byte-correct and the layer is still wrong: the kernel uniforms would configure a pass that
/// is not there, and the ramp pass would read a matrix built for tiles.
#[test]
fn a_frame_writes_each_pass_block_to_its_own_view() {
    use std::sync::Arc;

    use tessella_capture_abi::envelope::{UboUpdate, ViewId, WireRecord};
    use tessella_capture_abi::ring::Ring;
    use tessella_capture_abi::{CameraMode, EnvelopeKind};
    use tessella_orchestrate::SlabArena;
    use tessella_orchestrate::frame::{self, Frame};
    use tessella_orchestrate::tile::{TileId as BuildTile, build_sourceless, build_tile};
    use tessella_orchestrate::view::{self, ViewSession};
    use tessella_style::light::Light;
    use tessella_style::{Source, Style};
    use tessella_tile::cover;

    const HEATMAP: &str = include_str!("../../tessella-style/tests/heatmap_style.json");

    let style = Style::parse(HEATMAP).expect("style parses");
    let Some(Source::Geojson(source)) = style.source("probe") else {
        panic!("one geojson source");
    };
    let features = tessella_source::geojson::read(&source.data).expect("features read");

    let camera = probe();
    let tiles = cover::cover(&camera).expect("covers");
    let mut buckets = Vec::new();
    for tile in &tiles {
        let id = BuildTile::new(tile.z, tile.x, tile.y);
        let mut built = build_tile(
            &style,
            "probe",
            id,
            &features,
            tessella_source::tiling::TilingOptions::default(),
        )
        .expect("tile builds");
        built.extend(build_sourceless(&style, id).expect("background builds"));
        built.sort_by_key(|bucket| bucket.layer_index);
        buckets.push((id, Arc::new(built)));
    }

    let view_id = ViewId(0);
    let mut ring = Ring::new(1 << 22);
    let (producer, consumer) = ring.split();
    let mut arena = SlabArena::new();
    let mut session = ViewSession::new();
    session
        .declare(producer, view_id, CameraMode::Producer)
        .expect("declares");

    frame::emit(
        producer,
        &mut arena,
        &Frame {
            published_projection: None,
            projection: ProjectionMode::Mercator,
            style: &style,
            view: &camera,
            view_id,
            tiles: &tiles,
            buckets: &buckets,
            origins: &[],
            light: &Light::default(),
            fonts: None,
            patterns: None,
        },
    )
    .expect("the frame emits");

    // The ramps go up too, one per layer, and they are the oracle's bytes.
    // (Collected in the same drain below.)

    // `(view, layer, slot) -> bytes`, for the uniform writes only.
    let mut blocks: BTreeMap<(u32, i32, u32), Vec<u8>> = BTreeMap::new();
    let mut ramps: Vec<u64> = Vec::new();
    while let Some(record) = consumer.peek() {
        let consumed = record.consumed();
        if record.kind == EnvelopeKind::TextureUpdate
            && let Some(update) =
                tessella_capture_abi::envelope::TextureUpdate::from_bytes(record.record)
            && update.size.width == 256
            && update.size.height == 1
        {
            let bytes =
                &record.payload[update.pixels.offset as usize..][..update.pixels.count as usize];
            ramps.push(fnv1a(bytes));
        }
        if record.kind == EnvelopeKind::UboUpdate
            && let Some(update) = UboUpdate::from_bytes(record.record)
        {
            let bytes = record.payload[update.data.offset as usize..][..update.data.count as usize]
                .to_vec();
            blocks.insert((update.view.0, update.layer_index, update.slot), bytes);
        }
        consumer.advance(consumed);
    }

    // One ramp per heatmap layer, and both are the oracle's.
    ramps.sort_unstable();
    ramps.dedup();
    let mut want_ramps: Vec<u64> = DUMP
        .lines()
        .filter_map(|line| line.strip_prefix("texture 256x1 fmt=0 hash="))
        .map(|hash| u64::from_str_radix(hash, 16).expect("a hex hash"))
        .collect();
    want_ramps.sort_unstable();
    assert_eq!(ramps, want_ramps, "the frame uploads both color ramps");

    // The style's two heatmap layers are at indices 1 and 2.
    for layer_index in [1i32, 2] {
        #[allow(clippy::cast_sign_loss)]
        let offscreen = view::offscreen_view(view_id, layer_index as u32).expect("encodes");
        assert!(view::is_offscreen(offscreen));

        let kernels = blocks
            .get(&(offscreen.0, layer_index, ubo_slots::ID_HEATMAP_DRAWABLE_UBO))
            .unwrap_or_else(|| panic!("layer {layer_index} writes its kernels' drawable block"));
        assert_eq!(
            kernels.len() % ubo_layouts::HEATMAP_DRAWABLE_UBO.stride as usize,
            0,
            "a whole number of entries"
        );
        assert!(
            blocks.contains_key(&(
                offscreen.0,
                layer_index,
                ubo_slots::ID_HEATMAP_EVALUATED_PROPS_UBO
            )),
            "and its evaluated properties, in the same view"
        );

        // The quad's block is in the map's view, not the offscreen one.
        let texture_props = blocks
            .get(&(
                view_id.0,
                layer_index,
                ubo_slots::ID_HEATMAP_TEXTURE_PROPS_UBO,
            ))
            .unwrap_or_else(|| panic!("layer {layer_index} writes its texture-pass block"));
        assert_eq!(texture_props.len(), 80);
    }

    // And the evaluated properties are the oracle's bytes, layer for layer.
    let oracle = oracle_buffers();
    for (layer_index, name) in [(1i32, "heatmap-constant"), (2, "heatmap-composite")] {
        #[allow(clippy::cast_sign_loss)]
        let offscreen = view::offscreen_view(view_id, layer_index as u32).expect("encodes");
        let got = blocks
            .get(&(
                offscreen.0,
                layer_index,
                ubo_slots::ID_HEATMAP_EVALUATED_PROPS_UBO,
            ))
            .expect("the block was written");
        let (_, want) = oracle
            .get(&(
                0,
                name.to_owned(),
                ubo_slots::ID_HEATMAP_EVALUATED_PROPS_UBO,
            ))
            .expect("the oracle writes it");
        assert_eq!(&blocks_of_bytes(got), want, "{name}");
    }
}

/// The probe's canonicalization, over bytes rather than the dump's hex.
fn blocks_of_bytes(bytes: &[u8]) -> Vec<String> {
    let mut out: Vec<String> = bytes
        .chunks(16)
        .map(|chunk| chunk.iter().map(|byte| format!("{byte:02x}")).collect())
        .collect();
    out.sort();
    out
}

/// The whole layer reaches the stream: two passes, in the right two views, in the right order.
///
/// This is the shape assertion the earlier tests could not make. Each heatmap layer puts its
/// kernels in an offscreen view and one quad in the map's, the quad's geometry is announced
/// before anything binds it, and it carries the two samplers the second pass reads.
#[test]
fn each_heatmap_layer_emits_a_quad_bound_into_the_map() {
    use std::sync::Arc;

    use tessella_capture_abi::envelope::{GeometryAdd, ViewId, ViewUse, WireRecord};
    use tessella_capture_abi::ring::Ring;
    use tessella_capture_abi::{CameraMode, EnvelopeKind};
    use tessella_orchestrate::SlabArena;
    use tessella_orchestrate::frame::{self, Frame};
    use tessella_orchestrate::tile::{TileId as BuildTile, build_sourceless, build_tile};
    use tessella_orchestrate::view::{self, ViewSession};
    use tessella_style::light::Light;
    use tessella_style::{Source, Style};
    use tessella_tile::cover;

    const HEATMAP: &str = include_str!("../../tessella-style/tests/heatmap_style.json");

    let style = Style::parse(HEATMAP).expect("style parses");
    let Some(Source::Geojson(source)) = style.source("probe") else {
        panic!("one geojson source");
    };
    let features = tessella_source::geojson::read(&source.data).expect("features read");

    let camera = probe();
    let tiles = cover::cover(&camera).expect("covers");
    let mut buckets = Vec::new();
    for tile in &tiles {
        let id = BuildTile::new(tile.z, tile.x, tile.y);
        let mut built = build_tile(
            &style,
            "probe",
            id,
            &features,
            tessella_source::tiling::TilingOptions::default(),
        )
        .expect("tile builds");
        built.extend(build_sourceless(&style, id).expect("background builds"));
        built.sort_by_key(|bucket| bucket.layer_index);
        buckets.push((id, Arc::new(built)));
    }

    let view_id = ViewId(0);
    let mut ring = Ring::new(1 << 22);
    let (producer, consumer) = ring.split();
    let mut arena = SlabArena::new();
    let mut session = ViewSession::new();
    session
        .declare(producer, view_id, CameraMode::Producer)
        .expect("declares");

    frame::emit(
        producer,
        &mut arena,
        &Frame {
            published_projection: None,
            projection: ProjectionMode::Mercator,
            style: &style,
            view: &camera,
            view_id,
            tiles: &tiles,
            buckets: &buckets,
            origins: &[],
            light: &Light::default(),
            fonts: None,
            patterns: None,
        },
    )
    .expect("the frame emits");

    let mut announced: Vec<(u64, i32, usize)> = Vec::new();
    let mut uses: Vec<(u64, u32, i32)> = Vec::new();
    while let Some(record) = consumer.peek() {
        let consumed = record.consumed();
        match record.kind {
            EnvelopeKind::GeometryAdd => {
                if let Some(add) = GeometryAdd::from_bytes(record.record) {
                    announced.push((
                        add.geometry.0,
                        add.builtin_shader,
                        add.texture_refs.count as usize,
                    ));
                }
            }
            EnvelopeKind::ViewUse => {
                if let Some(use_record) = ViewUse::from_bytes(record.record) {
                    uses.push((
                        use_record.geometry.0,
                        use_record.view.0,
                        use_record.layer_index,
                    ));
                }
            }
            _ => {}
        }
        consumer.advance(consumed);
    }

    let texture_shader = tessella_capture_abi::BuiltIn::HeatmapTextureShader as i32;
    let kernel_shader = tessella_capture_abi::BuiltIn::HeatmapShader as i32;

    let quads: Vec<_> = announced
        .iter()
        .filter(|(_, shader, _)| *shader == texture_shader)
        .collect();
    assert_eq!(quads.len(), 2, "one quad per heatmap layer");
    for (_, _, samplers) in &quads {
        assert_eq!(*samplers, 2, "the target and the ramp");
    }

    for (geometry, _, _) in &quads {
        let position = announced
            .iter()
            .position(|(id, _, _)| id == geometry)
            .expect("announced");
        let used = uses
            .iter()
            .find(|(id, _, _)| id == geometry)
            .expect("a quad is bound");
        assert_eq!(used.1, view_id.0, "the quad draws in the map's view");
        assert!(position < announced.len(), "announced before it is bound");
    }

    // And the kernels are in the offscreen views, which is the pairing the layer is made of.
    for (geometry, shader, _) in &announced {
        if *shader != kernel_shader {
            continue;
        }
        let used = uses
            .iter()
            .find(|(id, _, _)| id == geometry)
            .expect("a kernel drawable is bound");
        assert!(
            view::is_offscreen(ViewId(used.1)),
            "kernels draw in the layer's own view"
        );
    }
}
