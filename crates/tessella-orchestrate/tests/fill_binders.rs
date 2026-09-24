// SPDX-License-Identifier: BSD-2-Clause
//! A fill's paint binder across two shaders, and which outline each layer draws.
//!
//! # What the oracle gives
//!
//! `tests/golden/fill_style.dump`: five squares under five `fill` layers that vary what a fill
//! can be told -- a plain color, a constant `fill-outline-color`, a data-driven color *and*
//! outline color *and* opacity, `fill-antialias: false`, and a viewport-anchored `fill-translate`.
//!
//! # Why this capture exists
//!
//! Across every fixture in the tree only `fill-color`, `fill-opacity` and `fill-pattern` were ever
//! set. So the case `binder.rs` documents as the reason `attribute_ids` unions a shader *family*
//! had no byte-level test:
//!
//! > The plain fill shader does not declare `fill-outline-color` and the plain line shader does
//! > not declare `line-floorwidth` -- that is exactly why those bind at `-1`.
//!
//! One interleaved buffer serves both shaders, and each binds what it declares and `-1`s what it
//! does not. The oracle and this build agree on all of it, offsets included:
//!
//! | id | property | `FillShader` | `FillOutlineShader` |
//! |---|---|---|---|
//! | 0 | position | `bind=0` | `bind=0` |
//! | 1 | `fill-color` | `bind=1` off 0 | **`bind=-1`** off 0 |
//! | 2 | `fill-opacity` | `bind=2` off 8 | `bind=2` off 8 |
//! | 3 | `fill-outline-color` | **`bind=-1`** off 12 | `bind=1` off 12 |
//!
//! Get an offset wrong and one shader reads the other's color; drop the `-1` entries and the
//! buffer's stride stops describing what is in it.
//!
//! # Which outline, and the floor under the question
//!
//! `fill-antialias: false` draws no outline at all, and the oracle emits no `FillOutlineShader`
//! drawable for that layer. Where an outline *is* drawn, mbgl picks between line primitives over
//! the fill's own vertices and a triangulated polyline, and on this Vulkan oracle the plain branch
//! takes the hardware line -- which is the rasterization floor
//! `MLN_TRIANGULATE_FILL_OUTLINES` describes, and not comparable vertex for vertex.
//!
//! The data-driven layer is where that stops mattering. `FillOutlineTriangulatedShader` declares
//! no paint of its own, so a per-feature outline color has nowhere to travel and **both sides fall
//! back to line primitives** -- where they agree exactly, fifty indices over twenty-five vertices.
//! That is the one layer here whose outline can be compared, and it is also what checks
//! `ubo::fill_outline_triangulates` against the oracle rather than against its own reasoning.

use std::collections::BTreeMap;

use tessella_capture_abi::{BuiltIn, declared_for};
use tessella_orchestrate::Content;
use tessella_orchestrate::binder::{FILL_FAMILY, attribute_ids, layout};
use tessella_orchestrate::tile::{TileId, build_tile};
use tessella_source::tiling::TilingOptions;
use tessella_style::Style;

const DUMP: &str = include_str!("../../../tests/golden/fill_style.dump");
const STYLE: &str = include_str!("../../tessella-style/tests/fill_style.json");

/// The tile every square in the fixture falls in.
const TILE: (u32, u32) = (4093, 2723);

/// `FillShader` and `FillOutlineShader`, counting from `BuiltIn::None`.
const FILL: &str = "sh0011";
const OUTLINE: &str = "sh0012";

/// An attribute as either side describes it: id, shader binding, offset, stride.
type Attribute = (u32, i32, u32, u32);

/// Per `(layer, shader tag)`, the attributes the oracle bound on the fixture's tile.
fn oracle_attributes() -> BTreeMap<(usize, &'static str), Vec<Attribute>> {
    let mut out: BTreeMap<(usize, &'static str), Vec<Attribute>> = BTreeMap::new();
    for line in DUMP.lines() {
        let Some(rest) = line.trim_start().strip_prefix("attr L") else {
            continue;
        };
        if !rest.contains("t13_00004093_00002723") {
            continue;
        }
        let tag = if rest.contains(FILL) {
            FILL
        } else if rest.contains(OUTLINE) {
            OUTLINE
        } else {
            continue;
        };
        let layer: usize = rest[..5].parse().expect("a five-digit layer index");
        let field = |name: &str| -> i64 {
            rest.split(&format!(" {name}="))
                .nth(1)
                .and_then(|t| t.split_whitespace().next())
                .unwrap_or_else(|| panic!("an {name} field in {rest}"))
                .parse()
                .expect("digits")
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let entry = (
            field("id") as u32,
            field("bind") as i32,
            field("off") as u32,
            field("stride") as u32,
        );
        let slot = out.entry((layer, tag)).or_default();
        if !slot.contains(&entry) {
            slot.push(entry);
        }
    }
    for attributes in out.values_mut() {
        attributes.sort_unstable();
    }
    out
}

/// The same, from this build's own binder. Position is excluded: it is not paint.
fn our_attributes() -> BTreeMap<(usize, &'static str), Vec<Attribute>> {
    let style = Style::parse(STYLE).expect("the style parses");
    let ids = attribute_ids(FILL_FAMILY);
    let mut out = BTreeMap::new();

    for (index, layer) in style.layers.iter().enumerate() {
        if !matches!(layer.kind, tessella_style::LayerKind::Fill) {
            continue;
        }
        let paint = tessella_style::property::resolve_paint(layer).expect("the paint resolves");
        let specs = tessella_style::property::paint_specs(&layer.kind).unwrap_or(&[]);
        let binder = tessella_layout::paint::PaintBinder::new(specs, &paint, 13.0);
        for (tag, shader) in [
            (FILL, BuiltIn::FillShader),
            (OUTLINE, BuiltIn::FillOutlineShader),
        ] {
            let vertex_layout = layout(&binder, &ids, |attr_id| {
                declared_for(shader, attr_id).map(|a| (a.binding, a.declared))
            });
            let mut attributes: Vec<Attribute> = vertex_layout
                .attributes
                .iter()
                .map(|a| (a.attr_id, a.binding, a.offset, vertex_layout.stride))
                .collect();
            attributes.sort_unstable();
            out.insert((index, tag), attributes);
        }
    }
    out
}

/// This build's fill buckets on the fixture's tile, by layer index.
fn our_geometry() -> BTreeMap<usize, (usize, usize, usize)> {
    let style = Style::parse(STYLE).expect("the style parses");
    let tessella_style::Source::Geojson(source) = style.source("probe").expect("a source") else {
        panic!("the fixture has one geojson source")
    };
    let features = tessella_source::geojson::read(&source.data).expect("features read");
    let (x, y) = TILE;
    build_tile(
        &style,
        "probe",
        TileId::new(13, x, y),
        &features,
        TilingOptions::default(),
    )
    .expect("the tile builds")
    .iter()
    .filter_map(|bucket| match &bucket.content {
        Content::Fill(fill) => Some((
            bucket.layer_index,
            (
                fill.vertices.len(),
                fill.indices.len(),
                fill.line_indices.len(),
            ),
        )),
        _ => None,
    })
    .collect()
}

/// Every data-driven fill property binds where the oracle binds it, on both shaders.
///
/// The oracle carries position as well, which this build's paint binder does not describe, so the
/// comparison is over the paint attributes and position is checked separately below.
#[test]
fn the_binder_matches_the_oracle_on_both_shaders() {
    let oracle = oracle_attributes();
    let ours = our_attributes();
    let style = Style::parse(STYLE).expect("the style parses");

    assert!(!ours.is_empty(), "the fixture has no fill layers");
    for (&(layer, tag), bound) in &ours {
        let name = style.layers.get(layer).map_or("?", |l| l.id.as_str());
        let want: Vec<Attribute> = oracle
            .get(&(layer, tag))
            .cloned()
            .unwrap_or_default()
            .into_iter()
            // Position is the one attribute that is not paint; the bucket carries it.
            .filter(|&(id, _, _, _)| id != 0)
            .collect();
        assert_eq!(
            bound, &want,
            "layer {layer} ({name}) on {tag}: {bound:?} against the oracle's {want:?}"
        );
    }
}

/// The complementary `-1`s are the point: each shader declines the other's color.
///
/// Asserted on its own because the comparison above would also pass if both sides had dropped the
/// `-1` entries together, and then the stride would describe a buffer neither writes.
#[test]
fn each_shader_declines_the_others_color() {
    let ours = our_attributes();
    let style = Style::parse(STYLE).expect("the style parses");
    let driven = style
        .layers
        .iter()
        .position(|layer| layer.id == "fill-outline-driven")
        .expect("the data-driven layer");

    let binding_of = |tag: &'static str, id: u32| -> i32 {
        ours.get(&(driven, tag))
            .expect("its bindings")
            .iter()
            .find(|&&(attr, _, _, _)| attr == id)
            .map(|&(_, binding, _, _)| binding)
            .unwrap_or_else(|| panic!("no attribute {id} on {tag}"))
    };

    // 1 is `fill-color` and 3 is `fill-outline-color`, from `shader_defines.hpp`.
    assert_eq!(binding_of(FILL, 1), 1, "the fill shader takes fill-color");
    assert_eq!(
        binding_of(OUTLINE, 1),
        -1,
        "the outline shader does not declare fill-color"
    );
    assert_eq!(
        binding_of(FILL, 3),
        -1,
        "the fill shader does not declare fill-outline-color"
    );
    assert_eq!(
        binding_of(OUTLINE, 3),
        1,
        "the outline shader takes fill-outline-color"
    );

    // And all of it over one buffer, which is what the shared stride says.
    for tag in [FILL, OUTLINE] {
        let strides: Vec<u32> = ours
            .get(&(driven, tag))
            .expect("its bindings")
            .iter()
            .map(|&(_, _, _, stride)| stride)
            .collect();
        assert!(
            strides.iter().all(|&stride| stride == 20),
            "one twenty-byte vertex serves {tag}: {strides:?}"
        );
    }
}

/// `fill-antialias: false` draws no outline, and the oracle emits no outline drawable for it.
#[test]
fn antialias_off_draws_no_outline() {
    let style = Style::parse(STYLE).expect("the style parses");
    let index = |want: &str| {
        style
            .layers
            .iter()
            .position(|layer| layer.id == want)
            .unwrap_or_else(|| panic!("no layer {want}"))
    };
    let outline_drawables = |layer: usize| {
        DUMP.lines()
            .filter(|line| line.starts_with("drawable L"))
            .filter(|line| line.contains(&format!("L{layer:05}.")) && line.contains(OUTLINE))
            .count()
    };

    assert_eq!(
        outline_drawables(index("fill-no-aa")),
        0,
        "the oracle should draw no outline for an antialias-off layer"
    );
    for named in ["fill-plain", "fill-outlined", "fill-outline-driven"] {
        assert!(
            outline_drawables(index(named)) > 0,
            "{named} should have an outline: antialias defaults true and mbgl outlines in the \
             fill's own color when none is named"
        );
    }

    // And this build agrees about the one that draws none: no line indices and no polyline.
    let ours = our_geometry();
    let (_, _, lines) = ours[&index("fill-no-aa")];
    assert_eq!(lines, 0, "no outline means no line indices either");
}

/// The fills themselves agree exactly, and so does the one outline both sides draw the same way.
///
/// A data-driven outline color cannot travel through `FillOutlineTriangulatedShader`, which
/// declares no paint, so both fall back to line primitives there -- fifty indices over
/// twenty-five vertices, five edges to each of five squares. That is what checks
/// `ubo::fill_outline_triangulates` against the oracle instead of against its own reasoning.
#[test]
fn the_fill_and_the_comparable_outline_match() {
    let style = Style::parse(STYLE).expect("the style parses");
    let ours = our_geometry();
    let (x, y) = TILE;

    let oracle_seg = |layer: usize, tag: &str| -> Option<(usize, usize)> {
        let count = |rest: &str, name: &str| -> usize {
            rest.split(&format!("{name}="))
                .nth(1)
                .and_then(|tail| tail.split_whitespace().next())
                .and_then(|digits| digits.parse().ok())
                .unwrap_or_default()
        };
        DUMP.lines()
            .filter_map(|line| line.trim_start().strip_prefix("seg L"))
            .filter(|rest| rest.starts_with(&format!("{layer:05}.")))
            .filter(|rest| rest.contains(tag) && rest.contains(&format!("t13_{x:08}_{y:08}")))
            .map(|rest| (count(rest, "vlen"), count(rest, "ilen")))
            .next()
    };

    for (&layer, &(verts, indices, lines)) in &ours {
        let name = style.layers.get(layer).map_or("?", |l| l.id.as_str());
        let want = oracle_seg(layer, FILL).unwrap_or_else(|| panic!("no oracle fill for {name}"));
        assert_eq!(
            (verts, indices),
            want,
            "layer {layer} ({name}) fill: {verts}/{indices} against the oracle's {want:?}"
        );

        // Line indices only where the outline could not be triangulated. Where they exist they
        // are the oracle's own primitive and compare directly.
        if lines > 0 {
            let want = oracle_seg(layer, OUTLINE)
                .unwrap_or_else(|| panic!("no oracle outline for {name}"));
            assert_eq!(
                lines, want.1,
                "layer {layer} ({name}) draws the oracle's line primitives, so the index counts \
                 must agree: {lines} against {}",
                want.1
            );
        }
    }

    // The data-driven layer is the one that falls back, and it is not vacuous.
    let driven = style
        .layers
        .iter()
        .position(|layer| layer.id == "fill-outline-driven")
        .expect("the data-driven layer");
    assert_eq!(
        ours[&driven].2, 50,
        "a data-driven outline color forces the line-primitive path"
    );
    let constant = style
        .layers
        .iter()
        .position(|layer| layer.id == "fill-outlined")
        .expect("the constant-outline layer");
    assert_eq!(
        ours[&constant].2, 0,
        "a constant outline triangulates instead, which is the MLN_TRIANGULATE floor"
    );
}
