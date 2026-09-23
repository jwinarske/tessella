// SPDX-License-Identifier: BSD-2-Clause
//! Which circle paint properties become vertex attributes, and which do not.
//!
//! # What the oracle gives
//!
//! `tests/golden/circle_style.dump`: twenty points under eight `circle` layers, captured at
//! **pitch 60** so that `circle-pitch-alignment` and `circle-pitch-scale` mean something. The
//! layers vary what a circle can be told: a constant radius, a `["get", …]` radius with a
//! `["match", …]` color and a `["get", …]` stroke width, a zoom `interpolate` radius, a stroke,
//! both pitch enums, a blur, and a viewport-anchored `circle-translate`.
//!
//! # Why this capture exists
//!
//! `circle` appears in two goldens already and neither varies it: across every fixture in the
//! tree only `circle-color` and `circle-radius` were ever set, both constant. So the paint
//! *binder* -- the machinery that decides a property is data-driven, gives it an attribute id,
//! and interleaves it into one buffer at an offset -- had no byte-level test on this family at
//! all, and the binder is shared with every other family.
//!
//! The distinction it pins is the one a reader gets wrong: **a property that varies per feature
//! becomes a vertex attribute; a property that varies per zoom does not.** `circle-zoomed`'s
//! radius sweeps 2 to 24 across five zoom levels and binds nothing, because a zoom curve is
//! evaluated once per frame into the layer's uniforms. `circle-data-driven`'s radius binds,
//! because it cannot be. Getting that backwards costs an attribute per vertex on every circle in
//! a scene, or drops one that was needed -- and neither shows up as a wrong pixel until the data
//! happens to vary.
//!
//! # What is not compared here
//!
//! The pitch enums and the translate reach the shader through uniforms rather than geometry, so
//! what this asserts about them is that they leave the vertices alone -- see
//! [`the_uniform_only_properties_leave_the_geometry_alone`]. Their arithmetic is
//! `ubo::circle_extrude_scale`'s and is tested there.
//!
//! The circle *edge* is a known rasterization floor -- identical shader math, one antialiased
//! pixel per circle, converging as the radius grows -- which is exactly why this family is worth
//! a dump rather than more pixels.

use std::collections::{BTreeMap, BTreeSet};

use tessella_capture_abi::{BuiltIn, declared_for};
use tessella_orchestrate::Content;
use tessella_orchestrate::binder::{CIRCLE_FAMILY, attribute_ids, layout};
use tessella_orchestrate::tile::{TileId, build_tile};
use tessella_source::tiling::TilingOptions;
use tessella_style::Style;

const DUMP: &str = include_str!("../../../tests/golden/circle_style.dump");
const STYLE: &str = include_str!("../../tessella-style/tests/circle_style.json");

/// The tiles the fixture's points fall in.
///
/// The capture's cover is larger -- pitch 60 at z13 reaches twenty-four tiles, which is what the
/// background draws -- but the points are placed inside these six, so these are the tiles a
/// circle bucket exists for.
const TILES: [(u32, u32); 6] = [
    (4092, 2723),
    (4092, 2724),
    (4093, 2723),
    (4093, 2724),
    (4094, 2723),
    (4094, 2724),
];

/// `idCirclePosVertexAttribute`, the one attribute that is not a paint property.
///
/// It is the quad's own corner and every layer carries it, so it is filtered out before the two
/// sides are compared: this build's paint binder describes paint, and position reaches the wire
/// from the bucket.
const POSITION_ATTRIBUTE: u32 = 0;

/// An attribute as the capture describes it: its id, its offset into the interleaved buffer, and
/// that buffer's stride.
type Attribute = (u32, u32, u32);

/// Per layer index, the data-driven attributes the oracle bound.
fn oracle_attributes() -> BTreeMap<usize, BTreeSet<Attribute>> {
    let mut out: BTreeMap<usize, BTreeSet<Attribute>> = BTreeMap::new();
    for line in DUMP.lines() {
        let Some(rest) = line.trim_start().strip_prefix("attr L") else {
            continue;
        };
        let layer: usize = rest[..5].parse().expect("a five-digit layer index");
        let field = |name: &str| -> u32 {
            rest.split(&format!(" {name}="))
                .nth(1)
                .and_then(|t| t.split_whitespace().next())
                .unwrap_or_else(|| panic!("an {name} field in {rest}"))
                .parse()
                .expect("digits")
        };
        let id = field("id");
        if id == POSITION_ATTRIBUTE {
            continue;
        }
        out.entry(layer)
            .or_default()
            .insert((id, field("off"), field("stride")));
    }
    out
}

/// The same, from this build's own paint binder.
fn our_attributes() -> BTreeMap<usize, BTreeSet<Attribute>> {
    let style = Style::parse(STYLE).expect("the style parses");
    let ids = attribute_ids(CIRCLE_FAMILY);
    let mut out: BTreeMap<usize, BTreeSet<Attribute>> = BTreeMap::new();

    for (index, layer) in style.layers.iter().enumerate() {
        if !matches!(layer.kind, tessella_style::LayerKind::Circle) {
            continue;
        }
        let paint = tessella_style::property::resolve_paint(layer).expect("the paint resolves");
        let specs = tessella_style::property::paint_specs(&layer.kind).unwrap_or(&[]);
        // The zoom the capture was taken at -- a binder's slots depend on it, because a property
        // that is constant at one zoom can be data-driven at another.
        let binder = tessella_layout::paint::PaintBinder::new(specs, &paint, 13.0);
        let vertex_layout = layout(&binder, &ids, |attr_id| {
            declared_for(BuiltIn::CircleShader, attr_id).map(|a| (a.binding, a.declared))
        });
        out.insert(
            index,
            vertex_layout
                .attributes
                .iter()
                .map(|a| (a.attr_id, a.offset, vertex_layout.stride))
                .collect(),
        );
    }
    out
}

/// Vertex and index counts per `(layer, tile)`, from this build's buckets.
fn our_geometry() -> BTreeMap<(usize, (u32, u32)), (usize, usize)> {
    let style = Style::parse(STYLE).expect("the style parses");
    let tessella_style::Source::Geojson(source) = style.source("probe").expect("a source") else {
        panic!("the fixture has one geojson source")
    };
    let features = tessella_source::geojson::read(&source.data).expect("features read");

    let mut out = BTreeMap::new();
    for (x, y) in TILES {
        let buckets = build_tile(
            &style,
            "probe",
            TileId::new(13, x, y),
            &features,
            TilingOptions::default(),
        )
        .expect("the tile builds");
        for bucket in &buckets {
            let Content::Circle(circle) = &bucket.content else {
                continue;
            };
            if circle.vertices.is_empty() {
                continue;
            }
            out.insert(
                (bucket.layer_index, (x, y)),
                (circle.vertices.len(), circle.indices.len()),
            );
        }
    }
    out
}

/// Every layer binds exactly the attributes the oracle bound, at the oracle's offsets.
///
/// Offsets and stride as well as ids, because the three share one interleaved buffer and an
/// offset that is off by a field reads a color as a radius -- which draws, and draws wrongly.
#[test]
fn every_layer_binds_the_oracles_attributes() {
    let oracle = oracle_attributes();
    let ours = our_attributes();
    let style = Style::parse(STYLE).expect("the style parses");
    let name = |index: usize| {
        style
            .layers
            .get(index)
            .map_or("?", |layer| layer.id.as_str())
    };

    assert!(!ours.is_empty(), "the fixture has no circle layers");
    for (&layer, bound) in &ours {
        let want = oracle.get(&layer).cloned().unwrap_or_default();
        assert_eq!(
            bound,
            &want,
            "layer {layer} ({}): bound {bound:?} against the oracle's {want:?}",
            name(layer)
        );
    }
}

/// A property that varies per feature binds; one that varies per zoom does not.
///
/// The contrast is the point of the fixture, so it is asserted directly rather than left implied
/// by the comparison above -- which would still pass if both layers bound nothing.
#[test]
fn a_zoom_curve_binds_nothing_and_a_feature_curve_binds() {
    let ours = our_attributes();
    let style = Style::parse(STYLE).expect("the style parses");
    let index = |want: &str| {
        style
            .layers
            .iter()
            .position(|layer| layer.id == want)
            .unwrap_or_else(|| panic!("no layer {want}"))
    };

    let driven = ours
        .get(&index("circle-data-driven"))
        .expect("its bindings");
    let zoomed = ours.get(&index("circle-zoomed")).expect("its bindings");

    // `circle-color`, `circle-radius`, `circle-stroke-width` -- ids 1, 2 and 6 in
    // `shader_defines.hpp`, packed into sixteen bytes as a color pair and two floats.
    let ids: BTreeSet<u32> = driven.iter().map(|&(id, _, _)| id).collect();
    assert_eq!(
        ids,
        BTreeSet::from([1, 2, 6]),
        "the data-driven layer should bind color, radius and stroke width: {driven:?}"
    );
    assert!(
        driven.iter().all(|&(_, _, stride)| stride == 16),
        "the three share one sixteen-byte vertex: {driven:?}"
    );
    assert!(
        zoomed.is_empty(),
        "an interpolate over zoom is a uniform, not an attribute: {zoomed:?}"
    );
}

/// Every layer draws a quad per point, and the oracle drew the same ones.
///
/// Four vertices and six indices each, so the counts are the fixture's point distribution across
/// the cover -- which is what catches a point dropped at a tile edge or one counted twice.
#[test]
fn every_layer_draws_the_oracles_quads() {
    let mut oracle: BTreeMap<(usize, (u32, u32)), (usize, usize)> = BTreeMap::new();
    for line in DUMP.lines() {
        let Some(rest) = line.trim_start().strip_prefix("seg L") else {
            continue;
        };
        let layer: usize = rest[..5].parse().expect("a five-digit layer index");
        let Some(tile) = rest
            .split(".t13_")
            .nth(1)
            .and_then(|t| t.split("_o13").next())
            .and_then(|t| t.split_once('_'))
            .map(|(x, y)| (x.parse().expect("a tile x"), y.parse().expect("a tile y")))
        else {
            continue;
        };
        let count = |name: &str| -> usize {
            rest.split(&format!("{name}="))
                .nth(1)
                .and_then(|t| t.split_whitespace().next())
                .unwrap_or_else(|| panic!("a {name} field"))
                .parse()
                .expect("digits")
        };
        oracle.insert((layer, tile), (count("vlen"), count("ilen")));
    }

    let ours = our_geometry();
    assert!(!ours.is_empty(), "the fixture built no circle buckets");
    for (&(layer, tile), &(verts, indices)) in &ours {
        assert_eq!(
            verts % 4,
            0,
            "a circle is a quad: {verts} vertices at layer {layer}, {tile:?}"
        );
        assert_eq!(
            verts / 4 * 6,
            indices,
            "six indices to the quad at layer {layer}, {tile:?}"
        );
        let &want = oracle
            .get(&(layer, tile))
            .unwrap_or_else(|| panic!("the oracle drew nothing for layer {layer} at {tile:?}"));
        assert_eq!(
            (verts, indices),
            want,
            "layer {layer} at {tile:?}: {verts} vertices and {indices} indices against the \
             oracle's {want:?}"
        );
    }
}

/// The pitch enums, the blur and the translate change uniforms, not vertices.
///
/// Captured at pitch 60, where a map-aligned circle and a viewport-aligned one genuinely differ
/// on screen -- and still draw the same quads. Worth pinning because the opposite is a plausible
/// implementation: extruding a map-aligned circle in tile units at build time would put the
/// pitch into the geometry and rebuild every bucket on a camera move.
#[test]
fn the_uniform_only_properties_leave_the_geometry_alone() {
    let ours = our_geometry();
    let style = Style::parse(STYLE).expect("the style parses");
    let index = |want: &str| {
        style
            .layers
            .iter()
            .position(|layer| layer.id == want)
            .unwrap_or_else(|| panic!("no layer {want}"))
    };

    let plain = index("circle-plain");
    for other in [
        "circle-pitch-map",
        "circle-pitch-viewport",
        "circle-blurred",
        "circle-translated",
        "circle-stroked",
    ] {
        for (x, y) in TILES {
            assert_eq!(
                ours.get(&(index(other), (x, y))),
                ours.get(&(plain, (x, y))),
                "{other} should draw the same quads as circle-plain at {x}/{y}"
            );
        }
    }
}
