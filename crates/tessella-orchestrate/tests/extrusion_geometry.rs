// SPDX-License-Identifier: BSD-2-Clause
//! A fill-extrusion's roof, its pass count and its clipped rings, against the oracle.
//!
//! # What the oracle gives
//!
//! `tests/golden/extrusion_style.dump`: six polygons -- five plain rectangles and one with an
//! interior ring -- under four `fill-extrusion` layers that vary how height, base, color and
//! opacity are arrived at: a constant height, `["get", …]` height and base, a
//! `vertical-gradient: false` layer, and a zoom `interpolate` height with a `match` color.
//!
//! Each drawable's name carries its vertex count and its `idx=` field the index count, so per
//! layer and tile the capture is mbgl's own answer for how much roof a polygon produces.
//!
//! # Why this capture exists
//!
//! The extrusion family had no dump coverage. Its geometry is the one place a polygon's *rings*
//! reach the wire -- earcut over a ring set, with the walls raised by the shader from the same
//! vertices -- and a ring misclassified as an exterior fills in a courtyard without changing a
//! pixel anywhere else on the frame. Counting rings is what names that; a gross-pixel sweep over
//! a city does not.
//!
//! # Two divergences this pinned, neither of them a defect
//!
//! **mbgl emits the geometry twice on this backend.** `MLN_USE_FILL_EXTRUSION_INSTANCING` is
//! `(MLN_RENDER_BACKEND_METAL || MLN_RENDER_BACKEND_VULKAN)` in `include/mbgl/shaders/layer_ubo.hpp`,
//! and the oracle is a Vulkan build. `render_fill_extrusion_layer.cpp` builds its `depth` and
//! `color` drawables unconditionally and then, under that flag, an instanced pair as well -- so
//! every layer appears against both `FillExtrusionShader` (`sh0016`) and
//! `FillExtrusionInstancedShader` (`sh0017`), the second drawing a unit quad instanced per
//! extrusion vertex. Only the first is geometry to compare against; see [`ORACLE_SHADER`].
//!
//! **A hole that reaches the tile's buffer edge stops being a hole.** In `4092/2723` the fixture's
//! interior ring runs out past the tile's buffer, so mbgl's clipper merges it into the outer
//! boundary and emits one U-shaped ring -- eight corners and a closing point, nine vertices. This
//! build clips the two rings separately and keeps both, four corners and a closing point each, ten
//! vertices, and earcut bridges them. **The triangles are the same either way** -- six, and the
//! same six -- and the only edge the extra vertex adds lies at x=10240, inside the buffer the
//! stencil mask discards. So the roof and the picture agree and the representation does not; see
//! [`MERGED_AT_THE_BUFFER_EDGE`].
//!
//! In the tile where the same polygon is *not* clipped, `4093/2723`, the two agree exactly: ten
//! vertices and twenty-four indices, a ring and its hole.
//!
//! # The capture is canonicalized
//!
//! Those two render-state sets are emitted in whichever order mbgl visited them, so a raw capture
//! of this style differs from the last on 160 `flags=` fields and nothing else.
//! `canonicalize_drawable_index.py` renumbers the `#NN` by what each drawable *is*, after which
//! two fresh captures and the committed file are byte-identical. It is part of the regeneration
//! recipe; this test reads only layer, tile, vertex count and `idx=`, so it passed either way, and
//! that is why the missing step went unnoticed until the file was diffed.

use std::collections::BTreeMap;

use tessella_orchestrate::Content;
use tessella_orchestrate::tile::{TileId, build_tile};
use tessella_source::tiling::TilingOptions;
use tessella_style::Style;

const DUMP: &str = include_str!("../../../tests/golden/extrusion_style.dump");
const STYLE: &str = include_str!("../../tessella-style/tests/extrusion_style.json");

/// The tiles the capture covers, which is the z13 cover of its camera.
const TILES: [(u32, u32); 6] = [
    (4092, 2723),
    (4092, 2724),
    (4093, 2723),
    (4093, 2724),
    (4094, 2723),
    (4094, 2724),
];

/// `FillExtrusionShader`, counting from `BuiltIn::None` in `include/mbgl/shaders/shader_source.hpp`.
///
/// The instanced companion is `sh0017` and carries no geometry of its own -- its vertices are a
/// unit quad and its instance attributes point back at this shader's buffer -- so it is filtered
/// out rather than summed.
const ORACLE_SHADER: &str = "sh0016";

/// The one tile where the fixture's hole reaches the buffer edge, and the two sides differ.
///
/// mbgl merges the ring into the outer boundary there and this build does not. Pinned rather than
/// fixed: the triangles agree and the extra edge falls in the masked buffer.
const MERGED_AT_THE_BUFFER_EDGE: (u32, u32) = (4092, 2723);

/// Vertex and index counts per `(layer, tile)`, which is what both sides are compared on.
type Geometry = BTreeMap<(usize, (u32, u32)), (usize, usize)>;

/// Vertex and index counts per `(layer, tile)`, read out of the oracle's drawable names.
///
/// A translucent layer draws its geometry twice -- a depth pass in front of a color pass -- so the
/// same `(layer, tile)` appears more than once with identical counts. They are asserted equal and
/// collapsed, because this build carries one bucket and reports the pass count separately.
fn oracle_geometry() -> Geometry {
    let mut out = BTreeMap::new();
    for line in DUMP.lines() {
        let Some(rest) = line.strip_prefix("drawable L") else {
            continue;
        };
        if !rest.contains(ORACLE_SHADER) {
            continue;
        }
        let layer: usize = rest[..5].parse().expect("a five-digit layer index");
        // `…t13_00004092_00002723_o13_w+000…` -- the tile this drawable belongs to.
        let tile = rest
            .split(".t13_")
            .nth(1)
            .and_then(|t| t.split("_o13").next())
            .and_then(|t| t.split_once('_'))
            .map(|(x, y)| (x.parse().expect("a tile x"), y.parse().expect("a tile y")))
            .expect("a tile id");
        // `…v00000010#00 … idx=24:<hash>` -- the vertex count is in the name, the index count is
        // the first half of `idx=`.
        let verts: usize = rest
            .split(".v")
            .nth(1)
            .and_then(|t| t.split('#').next())
            .expect("a vertex count")
            .parse()
            .expect("digits");
        let indices: usize = line
            .split("idx=")
            .nth(1)
            .and_then(|t| t.split(':').next())
            .expect("an index count")
            .parse()
            .expect("digits");

        if let Some(&seen) = out.get(&(layer, tile)) {
            assert_eq!(
                seen,
                (verts, indices),
                "layer {layer}'s depth and color passes should draw the same geometry at {tile:?}"
            );
        }
        out.insert((layer, tile), (verts, indices));
    }
    out
}

/// This build's own buckets: the geometry, whether each layer wants a depth pass, and the
/// layer names to report failures by.
struct Ours {
    geometry: Geometry,
    depth: BTreeMap<usize, bool>,
    names: Vec<String>,
}

/// The same counts from this build's own buckets, with each layer's pass count alongside.
fn our_geometry() -> Ours {
    let style = Style::parse(STYLE).expect("the style parses");
    let tessella_style::Source::Geojson(source) = style.source("probe").expect("a source") else {
        panic!("the fixture has one geojson source")
    };
    let features = tessella_source::geojson::read(&source.data).expect("features read");

    let mut geometry = BTreeMap::new();
    let mut depth = BTreeMap::new();
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
            let Content::Fill3d(extrusion) = &bucket.content else {
                continue;
            };
            // An empty bucket is not a drawable, so it must not be counted against an oracle that
            // only records what it drew.
            if extrusion.vertices.is_empty() {
                continue;
            }
            geometry.insert(
                (bucket.layer_index, (x, y)),
                (extrusion.vertices.len(), extrusion.indices.len()),
            );
            depth.insert(bucket.layer_index, extrusion.needs_depth_pass());
        }
    }
    Ours {
        geometry,
        depth,
        names: style.layers.iter().map(|l| l.id.clone()).collect(),
    }
}

/// Every extrusion layer roofs every tile with the oracle's triangles.
///
/// Index counts, on every tile including the clipped one -- the divergence below costs a vertex
/// and no triangle, so this arm holds everywhere and is the one that would catch a ring read as an
/// exterior instead of a hole.
#[test]
fn every_roof_has_the_oracles_triangles() {
    let oracle = oracle_geometry();
    let Ours {
        geometry, names, ..
    } = our_geometry();

    assert!(
        !geometry.is_empty(),
        "the fixture built no extrusion buckets"
    );
    for (&(layer, tile), &(_, indices)) in &geometry {
        let name = names.get(layer).map_or("?", String::as_str);
        let &(_, want) = oracle.get(&(layer, tile)).unwrap_or_else(|| {
            panic!("the oracle drew nothing for layer {layer} ({name}) at {tile:?}")
        });
        assert_eq!(
            indices, want,
            "layer {layer} ({name}) at {tile:?}: {indices} indices against the oracle's {want}"
        );
    }
}

/// And the same vertices, except where a hole runs off the tile's buffer.
///
/// The exception is pinned rather than skipped: if the clipper ever merges the way mbgl's does,
/// this fails and says so.
#[test]
fn every_ring_has_the_oracles_vertices_except_at_the_buffer_edge() {
    let oracle = oracle_geometry();
    let Ours {
        geometry, names, ..
    } = our_geometry();

    for (&(layer, tile), &(verts, _)) in &geometry {
        let name = names.get(layer).map_or("?", String::as_str);
        let &(want, _) = oracle.get(&(layer, tile)).unwrap_or_else(|| {
            panic!("the oracle drew nothing for layer {layer} ({name}) at {tile:?}")
        });
        if tile == MERGED_AT_THE_BUFFER_EDGE {
            assert_eq!(
                (verts, want),
                (20, 19),
                "the merged-ring divergence at {tile:?} is twenty vertices against nineteen; \
                 layer {layer} ({name}) now reads {verts} against {want}"
            );
            continue;
        }
        assert_eq!(
            verts, want,
            "layer {layer} ({name}) at {tile:?}: {verts} vertices against the oracle's {want}"
        );
    }
}

/// A hole is a hole: the unclipped tile carries the ring and its interior, not two exteriors.
///
/// Two rings of four corners and a closing point each, earcut into eight triangles. Read as two
/// exteriors instead it would be four triangles and the courtyard would fill in, which is the
/// failure this fixture exists to catch.
#[test]
fn an_interior_ring_stays_a_hole() {
    let oracle = oracle_geometry();
    let ours = our_geometry().geometry;
    let unclipped = (4093, 2723);

    for layer in 1..=4 {
        assert_eq!(
            ours.get(&(layer, unclipped)),
            Some(&(30, 48)),
            "layer {layer} at {unclipped:?} should roof the holed polygon and its four neighbors"
        );
        assert_eq!(
            oracle.get(&(layer, unclipped)),
            ours.get(&(layer, unclipped)),
            "and agree with the oracle at {unclipped:?}"
        );
    }
}

/// An opaque extrusion draws once; a translucent one draws a depth pass in front of its color.
///
/// mbgl's `doDepthPass = (!opaque || hasPattern)`, which is why `ext-opaque` shows six drawables
/// across six tiles where the other three show twelve.
#[test]
fn the_depth_pass_follows_opacity() {
    let Ours { depth, names, .. } = our_geometry();

    let passes: BTreeMap<usize, usize> = DUMP
        .lines()
        .filter_map(|line| line.strip_prefix("drawable L"))
        .filter(|rest| rest.contains(ORACLE_SHADER))
        .map(|rest| rest[..5].parse::<usize>().expect("a layer index"))
        .fold(BTreeMap::new(), |mut acc, layer| {
            *acc.entry(layer).or_default() += 1;
            acc
        });

    for (&layer, &wants_depth) in &depth {
        let name = names.get(layer).map_or("?", String::as_str);
        let drawn = passes.get(&layer).copied().unwrap_or_default();
        let tiles = TILES.len();
        let want = if wants_depth { 2 * tiles } else { tiles };
        assert_eq!(
            drawn, want,
            "layer {layer} ({name}) wants a depth pass: {wants_depth}, so the oracle should have \
             drawn {want} across {tiles} tiles rather than {drawn}"
        );
    }
}

/// mbgl draws this family twice on Metal and Vulkan; this build draws it once.
///
/// Recorded rather than asserted against this build, because there is nothing here to compare it
/// to. If the instanced path is ever adopted, this is the line that says what mbgl's costs.
#[test]
fn the_oracle_also_emits_an_instanced_copy() {
    let instanced = DUMP
        .lines()
        .filter(|line| line.starts_with("drawable L"))
        .filter(|line| line.contains("sh0017"))
        .count();
    let plain = DUMP
        .lines()
        .filter(|line| line.starts_with("drawable L"))
        .filter(|line| line.contains(ORACLE_SHADER))
        .count();
    assert_eq!(
        instanced, plain,
        "the instanced path shadows the plain one drawable for drawable: {instanced} against \
         {plain}"
    );
}

/// `fill-extrusion-rounded-corner-distance` rounds the corners, and the oracle says by how much.
///
/// The property is mbgl's and was read nowhere here, so a style asking for rounded buildings got
/// square ones. `roundPolygonCorners` runs between `limitHoles` and the vertex count, turning each
/// corner into five points -- a start, three arc points and an end.
///
/// Counts captured from `mbgl-capture-probe` over this fixture with the property set to 5. They do
/// not depend on the distance: 5 and 20 give the same counts, because what a corner costs is fixed
/// and only where the arc sits moves.
///
/// | tile | oracle | this build |
/// |---|---|---|
/// | 4092/2723 | 83 / 222 | **84** / 222 |
/// | 4092/2724 | 42 / 108 | 42 / 108 |
/// | 4093/2723 | 126 / 336 | 126 / 336 |
/// | 4093/2724 | 84 / 216 | 84 / 216 |
/// | 4094/2723 | 63 / 162 | 63 / 162 |
/// | 4094/2724 | 63 / 162 | 63 / 162 |
///
/// The one mismatch is the tile [`MERGED_AT_THE_BUFFER_EDGE`] already names, and it is that
/// divergence rather than a rounding one: unrounded it is 20 against 19. Two rings of four corners
/// round to `2 * (4 * 5 + 1)` = 42 where one merged ring of eight rounds to `8 * 5 + 1` = 41, so
/// the difference stays the second ring's closing point -- one vertex, and no triangle. Every
/// index count agrees, that tile included.
#[test]
fn rounding_the_corners_matches_the_oracles_counts() {
    /// `(tile, vertices, indices)` as the oracle captured them at distance 5.
    const ORACLE: [((u32, u32), usize, usize); 6] = [
        ((4092, 2723), 83, 222),
        ((4092, 2724), 42, 108),
        ((4093, 2723), 126, 336),
        ((4093, 2724), 84, 216),
        ((4094, 2723), 63, 162),
        ((4094, 2724), 63, 162),
    ];

    let mut doc: serde_json::Value = serde_json::from_str(STYLE).expect("the fixture parses");
    for layer in doc["layers"]
        .as_array_mut()
        .expect("the style has layers")
        .iter_mut()
    {
        if layer["type"] == "fill-extrusion" {
            layer["layout"] = serde_json::json!({
                "fill-extrusion-rounded-corner-distance": 5
            });
        }
    }
    let style = Style::parse(&doc.to_string()).expect("the rounded style parses");
    let tessella_style::Source::Geojson(source) = style.source("probe").expect("a source") else {
        panic!("the fixture has one geojson source")
    };
    let features = tessella_source::geojson::read(&source.data).expect("features read");

    for (tile, want_verts, want_indices) in ORACLE {
        let (x, y) = tile;
        let buckets = build_tile(
            &style,
            "probe",
            TileId::new(13, x, y),
            &features,
            TilingOptions::default(),
        )
        .expect("the tile builds");
        let extrusion = buckets
            .iter()
            .find_map(|bucket| match &bucket.content {
                Content::Fill3d(e) if bucket.layer_index == 1 => Some(e),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no extrusion bucket at {tile:?}"));

        assert_eq!(
            extrusion.indices.len(),
            want_indices,
            "rounded triangles at {tile:?}"
        );
        let allowance = usize::from(tile == MERGED_AT_THE_BUFFER_EDGE);
        assert_eq!(
            extrusion.vertices.len(),
            want_verts + allowance,
            "rounded vertices at {tile:?}: the oracle's {want_verts} plus {allowance} for the \
             merged ring"
        );
    }
}

/// Zero is the default and it rounds nothing, which is the whole of the common path.
#[test]
fn no_distance_leaves_the_corners_alone() {
    let style = Style::parse(STYLE).expect("the style parses");
    let tessella_style::Source::Geojson(source) = style.source("probe").expect("a source") else {
        panic!("the fixture has one geojson source")
    };
    let features = tessella_source::geojson::read(&source.data).expect("features read");
    let buckets = build_tile(
        &style,
        "probe",
        TileId::new(13, 4093, 2724),
        &features,
        TilingOptions::default(),
    )
    .expect("the tile builds");
    let extrusion = buckets
        .iter()
        .find_map(|bucket| match &bucket.content {
            Content::Fill3d(e) if bucket.layer_index == 1 => Some(e),
            _ => None,
        })
        .expect("an extrusion bucket");
    assert_eq!(
        (extrusion.vertices.len(), extrusion.indices.len()),
        (20, 24),
        "the fixture sets no distance, so this is the unrounded count the golden already pins"
    );
}
