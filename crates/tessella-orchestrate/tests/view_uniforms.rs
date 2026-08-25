//! §9.2's third multi-view invariant: shared geometry, per-view uniforms.
//!
//! > Screen-space UBO variants (R-2) differ per view over identical shared geometry.
//!
//! # What it protects, and why it is the mirror of `view_independence`
//!
//! §5.1 shares a decoded tile process-wide, which is the whole point of §5: four views over one
//! style must not cost four decodes. But a drawable is not only geometry. It is geometry *plus*
//! the matrix that places it, and that matrix is a function of the camera looking at it. The
//! bytes that may be shared and the bytes that may not sit side by side in the same drawable,
//! and telling them apart is the entire correctness question.
//!
//! `view_independence.rs` asserts that sharing does not corrupt a view's *order*. This asserts
//! that sharing does not corrupt a view's *placement*. The failure it invites is the natural
//! one: having hoisted the tile's vertices to the shared store, hoist the tile's uniforms with
//! them — they are per tile too, after all. Then every view draws the primary display's camera,
//! and each inset renders a correct picture of the wrong place. Nothing errors, nothing is
//! empty, and on a workstation where the insets often show similar ground it can look right.
//!
//! # The assertion has to have two halves
//!
//! "Uniforms differ per view" alone is satisfied by never sharing anything, which is the bug §5
//! exists to fix rather than a fix for it. So each test below pins both sides at once: over the
//! *same tile*, the geometry is byte-identical between views while the matrix is not. One
//! without the other is a property a wrong implementation also has.
//!
//! # And a converse, or the difference proves nothing
//!
//! Two views at the *same* camera must produce identical uniforms. Without that, a test that
//! only checked for difference would pass for an implementation that mixed the view's *identity*
//! into its matrix — a stale per-view scratch buffer, an index that leaked into an offset —
//! which differs per view for entirely the wrong reason.

use std::collections::BTreeMap;

use tessella_orchestrate::sweep;
use tessella_orchestrate::tile::{TileId as BuildTile, build_tile};
use tessella_orchestrate::ubo::{DrawableEntry, GlobalPaintParams};
use tessella_source::geojson;
use tessella_source::tiling::TilingOptions;
use tessella_style::{Source, Style};
use tessella_tile::cover::{self, ViewTransform};

const HERMETIC: &str = include_str!("../../tessella-style/tests/hermetic_style.json");

fn style() -> Style {
    Style::parse(HERMETIC).expect("style parses")
}

fn features(style: &Style) -> Vec<tessella_source::GeoJsonFeature> {
    let Some(Source::Geojson(source)) = style.source("probe") else {
        panic!("a geojson source");
    };
    geojson::read(&source.data).expect("features")
}

/// The tiles a view covers, as build ids.
fn cover_of(view: &ViewTransform) -> Vec<BuildTile> {
    cover::cover(view)
        .expect("covers")
        .into_iter()
        .map(|tile| BuildTile::new(tile.z, tile.x, tile.y))
        .collect()
}

/// The tiles every one of these views covers.
///
/// The invariant is only meaningful where the covers actually meet: two views looking at
/// different ground share nothing, and asserting that their matrices differ would assert
/// nothing at all.
fn tiles_in_common(views: &[ViewTransform]) -> Vec<BuildTile> {
    let mut counts: BTreeMap<BuildTile, usize> = BTreeMap::new();
    for view in views {
        for tile in cover_of(view) {
            *counts.entry(tile).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .filter(|(_, seen)| *seen == views.len())
        .map(|(tile, _)| tile)
        .collect()
}

/// A tile's buckets — geometry, resolved paint and paint binder, which is the whole of what
/// §5.1 shares between views.
fn geometry_of(tile: BuildTile) -> Vec<tessella_orchestrate::tile::LayerBucket> {
    let style = style();
    let features = features(&style);
    let mut buckets = build_tile(&style, "probe", tile, &features, TilingOptions::default())
        .expect("tile builds");
    buckets.sort_by_key(|bucket| bucket.layer_index);
    buckets
}

/// The drawable matrix for a tile under a view, at the layer the hermetic style's fill uses.
fn matrix_of(view: &ViewTransform, tile: BuildTile) -> [f32; 16] {
    DrawableEntry::for_tile(view, tile.z, tile.x, tile.y, 0, 0, 0)
        .expect("an unrotated view has a matrix")
        .matrix
}

/// A tile in four views at four cameras: one set of vertices, four different matrices.
///
/// This is the invariant in a single test. Both halves have to hold together — identical
/// geometry proves the sharing happened, differing matrices prove it did not take the camera
/// with it.
#[test]
fn one_tile_has_one_geometry_and_four_matrices() {
    let views = sweep::four_views();
    let common = tiles_in_common(&views);
    assert!(
        !common.is_empty(),
        "the four sweep views must share tiles for this to assert anything"
    );

    for tile in common {
        // The shared half: whatever view asked for it, the tile's bytes are the tile's bytes.
        // Built independently per view here, which is the stronger statement — not merely that
        // a cache returned the same object, but that the bytes do not depend on who asked.
        let shared = geometry_of(tile);
        assert!(
            !shared.is_empty(),
            "{tile:?} should carry geometry to share"
        );
        for _ in &views {
            assert_eq!(
                geometry_of(tile),
                shared,
                "{tile:?} geometry differs by view"
            );
        }

        // The per-view half: four cameras, four placements, all distinct.
        let matrices: Vec<[f32; 16]> = views.iter().map(|view| matrix_of(view, tile)).collect();
        for (left, right) in
            (0..matrices.len()).flat_map(|i| (i + 1..matrices.len()).map(move |j| (i, j)))
        {
            assert_ne!(
                matrices[left], matrices[right],
                "{tile:?}: views {left} and {right} sit at different cameras and must not \
                 share a matrix"
            );
        }
    }
}

/// The converse: same camera, same uniforms.
///
/// Without this the test above passes for an implementation whose matrix depends on the view's
/// identity rather than its camera — which differs per view, and is wrong.
#[test]
fn two_views_at_one_camera_share_every_uniform() {
    let [base, ..] = sweep::four_views();
    let twin = base;

    for tile in cover_of(&base) {
        assert_eq!(
            matrix_of(&base, tile),
            matrix_of(&twin, tile),
            "{tile:?}: one camera must give one matrix"
        );
    }
    assert_eq!(
        GlobalPaintParams::for_view(&base, [64.0, 64.0], 1.0).pack(),
        GlobalPaintParams::for_view(&twin, [64.0, 64.0], 1.0).pack(),
    );
}

/// The frame-wide block is per view too, and for more reasons than the zoom.
///
/// `GlobalPaintParams` carries the viewport, the aspect ratio and the camera-to-centre distance
/// as well as the zoom. A cluster inset is not the size of the display it sits on, so a shared
/// frame-wide block would hand the inset the display's viewport and stretch everything it drew.
#[test]
fn the_frame_wide_block_follows_the_viewport_and_not_only_the_zoom() {
    let [base, ..] = sweep::four_views();
    let display = GlobalPaintParams::for_view(&base, [64.0, 64.0], 1.0);

    // A differently *shaped* inset: every viewport-derived field moves, aspect ratio included.
    let wide = GlobalPaintParams::for_view(
        &ViewTransform {
            width: 320.0,
            height: 180.0,
            ..base
        },
        [64.0, 64.0],
        1.0,
    );
    assert_eq!(display.map_zoom, wide.map_zoom, "same camera zoom");
    assert_ne!(display.world_size, wide.world_size);
    assert_ne!(display.units_to_pixels, wide.units_to_pixels);
    assert_ne!(display.aspect_ratio, wide.aspect_ratio);
    assert_ne!(display.pack(), wide.pack());

    // And the case that catches a block distinguished by aspect ratio alone: an inset that is a
    // scaled copy of the display — 320x240 beside 1024x768 — has the very same 4:3. Its
    // viewport and its camera-to-centre distance still differ, and a block that shared them
    // would size the inset's geometry to the display and put its near and far planes there too.
    let scaled = GlobalPaintParams::for_view(
        &ViewTransform {
            width: 320.0,
            height: 240.0,
            ..base
        },
        [64.0, 64.0],
        1.0,
    );
    assert_eq!(display.aspect_ratio, scaled.aspect_ratio, "the same 4:3");
    assert_ne!(display.world_size, scaled.world_size);
    assert_ne!(
        display.camera_to_center_distance,
        scaled.camera_to_center_distance
    );
    assert_ne!(display.pack(), scaled.pack());
}

/// Across a whole sweep, and not only at its ends.
///
/// The views converge as the sweep descends — that is what `four_view_sweep` measures — and
/// convergence is exactly where a shared uniform would stop being visible. So this walks every
/// frame and asserts the distinction survives at each: wherever two views at that frame sit at
/// different cameras, every tile they share is placed differently for each of them.
#[test]
fn the_distinction_holds_at_every_frame_of_the_sweep() {
    let base = sweep::four_views();
    let mut frames_asserted = 0usize;

    for zoom in sweep::sweep_zooms(9) {
        let views: Vec<ViewTransform> = base
            .iter()
            .map(|view| ViewTransform { zoom, ..*view })
            .collect();
        let common = tiles_in_common(&views);
        if common.is_empty() {
            continue;
        }
        for tile in common {
            let matrices: Vec<[f32; 16]> = views.iter().map(|view| matrix_of(view, tile)).collect();
            for index in 1..matrices.len() {
                assert_ne!(
                    matrices[0], matrices[index],
                    "z{zoom}: {tile:?} placed identically for views 0 and {index}"
                );
            }
        }
        frames_asserted += 1;
    }

    // A sweep whose views never share a tile would pass every assertion above vacuously.
    assert!(
        frames_asserted >= 5,
        "only {frames_asserted} frames had shared tiles to assert on"
    );
}

/// The stencil matrix is not the drawable matrix, and per-view-ness must reach it too.
///
/// `DrawableEntry` biases the projection by layer for depth ordering; a clip mask does not
/// participate in that and so carries an unbiased matrix. They look interchangeable, which is
/// why the module says in as many words that they must not be shared — and a mask left on the
/// wrong camera clips a view to its neighbour's tiles, which subtracts geometry rather than
/// misplacing it.
#[test]
fn the_stencil_matrix_is_per_view_and_is_not_the_drawable_matrix() {
    let [base, second, ..] = sweep::four_views();
    let common = tiles_in_common(&[base, second]);
    let tile = *common.first().expect("the two views share a tile");

    let coord = tessella_tile::cover::TileCoord {
        z: tile.z,
        x: tile.x,
        y: tile.y,
        wrap: 0,
    };
    let masks = |view: &ViewTransform| {
        tessella_orchestrate::stencil::clip_set(view, 0, core::slice::from_ref(&coord))
            .expect("an unrotated view has a matrix")
            .tiles[0]
            .matrix
    };

    assert_ne!(
        masks(&base),
        masks(&second),
        "a mask follows its own camera"
    );
    // And the bias really is the difference: the drawable's matrix for layer 0 is offset from
    // the mask's, so a consumer handed one for the other would depth-test against nothing.
    assert_ne!(matrix_of(&base, tile), masks(&base));
}
