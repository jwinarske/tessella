//! Painter order, checked against the golden dump (§6.3, §9.1).
//!
//! The dump is a draw order: mbgl emits its drawables in the order it draws them, and the key on
//! each line carries the layer, the sublayer and the tile. So the order this crate resolves can
//! be compared against the oracle's element for element, rather than asserted as a rule and
//! hoped for.
//!
//! # What the comparison covers
//!
//! The dump has five layers; R0 implements the first three (a background and two fills). Those
//! are the dump's first thirty drawables, and they are contiguous — a layer that draws nothing
//! still holds its index, so no renumbering separates them from the line and circle layers that
//! follow. The comparison takes the dump's own prefix rather than a filtered subset, which means
//! a defect that reordered across layers would show up here rather than being filtered away.

use std::collections::{BTreeMap, BTreeSet};

use tessella_capture_abi::envelope::ViewId;
use tessella_orchestrate::order::{self, DrawOrder};
use tessella_orchestrate::tile::{TileId as BuildTile, build_tile};
use tessella_source::geojson;
use tessella_source::tiling::TilingOptions;
use tessella_style::{Source, Style};
use tessella_tile::cover::{self, ViewTransform};

const HERMETIC: &str = include_str!("../../tessella-style/tests/hermetic_style.json");
const DUMP: &str = include_str!("../../../tests/golden/hermetic_style.dump");

/// The oracle's probe: the transform the golden dump was captured at.
fn probe() -> ViewTransform {
    ViewTransform {
        longitude: -0.11,
        latitude: 51.505,
        zoom: 13.0,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
    }
}

/// `(layer, sublayer, x, y)` for one drawable.
type Slot = (u32, i32, u32, u32);

/// The order the oracle drew in, parsed from the dump's drawable lines.
fn oracle_order() -> Vec<Slot> {
    DUMP.lines()
        .filter_map(|line| line.strip_prefix("drawable L"))
        .map(|rest| {
            let key = rest.split(' ').next().expect("a drawable key");
            let mut parts = key.split('.');
            let layer: u32 = parts.next().expect("layer").parse().expect("layer number");
            let sub: i32 = parts
                .next()
                .and_then(|s| s.strip_prefix('S'))
                .expect("sublayer")
                .parse()
                .expect("sublayer number");
            let tile = parts.next().expect("tile");
            let mut fields = tile.strip_prefix('t').expect("a tile field").split('_');
            let _z = fields.next();
            let x: u32 = fields.next().expect("x").parse().expect("x number");
            let y: u32 = fields.next().expect("y").parse().expect("y number");
            (layer, sub, x, y)
        })
        .collect()
}

/// The order this crate resolves for the same view.
///
/// `OrderEntry` carries no tile — the tile rides on `ViewUse`, because geometry is shared across
/// views while a use is per view — so the tile is recovered through the geometry id the binding
/// was given. That indirection is the ABI's, not the test's.
fn resolved_order() -> Vec<Slot> {
    let style = Style::parse(HERMETIC).expect("style parses");
    let Some(Source::Geojson(source)) = style.source("probe") else {
        panic!("a geojson source");
    };
    let features = geojson::read(&source.data).expect("features");

    let view = ViewId(0);
    let mut order = DrawOrder::new();
    let mut next_id = 0;
    let mut tile_of_geometry = BTreeMap::new();

    // Bound in cover order, which is not draw order. Sorting is the module's job, and feeding it
    // pre-sorted input would test nothing.
    for tile in cover::cover(&probe()).expect("covers") {
        let buckets = build_tile(
            &style,
            BuildTile::new(tile.z, tile.x, tile.y),
            &features,
            TilingOptions::default(),
        )
        .expect("tile builds");
        for binding in order::bindings_for(
            view,
            order::tile_of(tile.z, tile.x, tile.y),
            &buckets,
            &mut next_id,
        ) {
            tile_of_geometry.insert(binding.geometry.0, (tile.x, tile.y));
            order.bind(binding);
        }
    }

    order
        .resolve()
        .iter()
        .map(|entry| {
            let (x, y) = tile_of_geometry[&entry.geometry.0];
            (entry.layer_index, entry.sub_layer_index, x, y)
        })
        .collect()
}

/// The resolved order matches the oracle's, drawable for drawable.
#[test]
fn painter_order_matches_the_oracle() {
    let oracle = oracle_order();
    let mine = resolved_order();

    // Asserted before the comparison, because comparing a prefix of length `mine.len()` passes
    // trivially when `mine` is empty. Thirty is the background's six plus two fills at two
    // drawables per tile.
    assert_eq!(mine.len(), 30, "six tiles, one background and two fills");
    assert_eq!(oracle.len(), 37, "the dump's five layers");

    assert_eq!(
        &oracle[..mine.len()],
        &mine[..],
        "draw order diverges from the oracle"
    );
}

/// And the comparison can fail: a swapped sublayer is caught.
///
/// The test above passed on its first run, which is the moment to check that it was capable of
/// not passing. Drawing a fill's outline before its triangles is a real defect — the outline
/// would be painted over — and it must not compare equal to the oracle.
#[test]
fn a_swapped_sublayer_would_not_match() {
    let oracle = oracle_order();
    let mut swapped = resolved_order();
    for slot in &mut swapped {
        slot.1 = match slot.1 {
            1 => 2,
            2 => 1,
            other => other,
        };
    }
    assert_ne!(&oracle[..swapped.len()], &swapped[..]);
}

/// The dump's R0 prefix is contiguous, which is what makes comparing a prefix legitimate.
///
/// If the line and circle layers were interleaved among the fills, taking the first thirty
/// drawables would be taking an arbitrary window rather than the part R0 implements.
#[test]
fn the_implemented_layers_are_the_dumps_prefix() {
    let oracle = oracle_order();
    let r0: BTreeSet<u32> = oracle.iter().take(30).map(|(layer, ..)| *layer).collect();
    assert_eq!(r0, BTreeSet::from([0, 1, 2]), "background and two fills");
    assert!(
        oracle[30..].iter().all(|(layer, ..)| *layer >= 3),
        "and nothing after the prefix belongs to them"
    );
}
