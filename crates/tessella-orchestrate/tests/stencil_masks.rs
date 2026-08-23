//! Clip sets, checked against the golden dump (§2.2, §9.1).
//!
//! The dump records each clip set as a layer index, a tile, and an FNV-1a hash of the tile's
//! matrix as sixteen little-endian `f32`. So the matrices can be diffed without the golden file
//! carrying ninety-six floats, and the comparison is still exact: a hash match over sixty-four
//! bytes is not a tolerance.

use std::collections::{BTreeMap, BTreeSet};

use tessella_orchestrate::stencil;
use tessella_tile::cover::{self, ViewTransform};

const DUMP: &str = include_str!("../../../tests/golden/hermetic_style.dump");

/// The oracle's probe, settled the way a map settles a camera it is given.
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

/// FNV-1a, as the probe hashes.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// `layer -> (x, y) -> matrix hash`, from the dump's stencil section.
fn oracle_sets() -> BTreeMap<i32, BTreeMap<(u32, u32), u64>> {
    let mut sets: BTreeMap<i32, BTreeMap<(u32, u32), u64>> = BTreeMap::new();
    for line in DUMP.lines() {
        let Some(rest) = line.trim_start().strip_prefix("stencil-tile layer=") else {
            continue;
        };
        let mut fields = rest.split(' ');
        let layer: i32 = fields.next().expect("layer").parse().expect("layer number");
        let tile = fields.next().expect("tile");
        let hash = u64::from_str_radix(
            fields
                .next()
                .and_then(|f| f.strip_prefix("m="))
                .expect("a matrix hash"),
            16,
        )
        .expect("hex");

        let mut parts = tile.strip_prefix('t').expect("a tile field").split('_');
        let _z = parts.next();
        let x: u32 = parts.next().expect("x").parse().expect("x number");
        let y: u32 = parts.next().expect("y").parse().expect("y number");
        sets.entry(layer).or_default().insert((x, y), hash);
    }
    sets
}

/// This crate's clip set for a layer, hashed the same way.
fn my_set(layer: i32) -> BTreeMap<(u32, u32), u64> {
    let view = probe();
    let tiles = cover::cover(&view).expect("covers");
    let set = stencil::clip_set(&view, layer, &tiles).expect("an unrotated camera");
    set.tiles
        .iter()
        .map(|tile| {
            let mut bytes = Vec::with_capacity(64);
            for value in tile.matrix {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
            ((tile.tile.x, tile.tile.y), fnv1a(&bytes))
        })
        .collect()
}

/// The tile matrices reproduce the oracle's, hash for hash.
///
/// Checked on both of R0's tiled layers. The oracle also emits a set for its line layer, which
/// R0 does not implement — and its matrices are identical to these, which is the next test.
#[test]
fn the_clip_matrices_match_the_oracle() {
    let oracle = oracle_sets();
    for layer in [1, 2] {
        let want = oracle.get(&layer).expect("the oracle clips this layer");
        let got = my_set(layer);
        assert_eq!(got.len(), 6, "six tiles at layer {layer}");
        assert_eq!(&got, want, "clip matrices diverge at layer {layer}");
    }
}

/// The matrix is a function of the tile and the camera, not of the layer.
///
/// The oracle's three clip sets carry identical hashes for that reason. A set that varied by
/// layer would mean tile placement had picked up a dependency it must not have — and would make
/// the shared tile store unsound, because the placement would no longer be something two layers
/// could agree on.
#[test]
fn the_matrix_does_not_depend_on_the_layer() {
    let oracle = oracle_sets();
    let sets: Vec<&BTreeMap<(u32, u32), u64>> = oracle.values().collect();
    assert!(sets.len() >= 2, "more than one clip set to compare");
    for set in &sets[1..] {
        assert_eq!(*set, sets[0], "the oracle's sets differ by layer");
    }
    assert_eq!(my_set(1), my_set(2), "and so do this crate's");
}

/// Only tiled layers are clipped. The background is not, and neither is anything drawn once.
///
/// A background clipped to a tile is a background with seams, and this crate's
/// `background_flags` carries no stencil bit — so the two halves have to agree, and this is
/// where that is checked rather than assumed.
#[test]
fn untiled_layers_have_no_clip_set() {
    let clipped: BTreeSet<i32> = oracle_sets().keys().copied().collect();
    assert!(!clipped.contains(&0), "the background is not clipped");
    assert!(!clipped.contains(&4), "nor is the circle layer");
    assert_eq!(clipped, BTreeSet::from([1, 2, 3]), "the three tiled layers");

    let background = tessella_orchestrate::view::background_flags();
    let tiled = tessella_orchestrate::view::tiled_flags();
    let stencil_bit = tessella_capture_abi::envelope::DrawFlags::ENABLE_STENCIL;
    assert!(
        !background.contains(stencil_bit),
        "background flags carry no stencil bit"
    );
    assert!(tiled.contains(stencil_bit), "and tiled flags do");
}

/// An unchanged clip set is not re-sent, which is DR-8 on this channel.
#[test]
fn an_unchanged_clip_set_writes_no_bytes() {
    use tessella_capture_abi::envelope::ViewId;
    use tessella_capture_abi::ring::Ring;

    let view = probe();
    let tiles = cover::cover(&view).expect("covers");
    let set = stencil::clip_set(&view, 1, &tiles).expect("an unrotated camera");

    let mut ring = Ring::new(1 << 16);
    let (producer, _consumer) = ring.split();
    let mut sets = stencil::ClipSets::new();

    assert!(
        sets.emit(producer, ViewId(0), &set).expect("emits"),
        "the first one is written"
    );
    let after_first = producer.head();

    for _frame in 0..1000 {
        assert!(!sets.emit(producer, ViewId(0), &set).expect("emits"));
    }
    assert_eq!(producer.head(), after_first, "and nothing after it");
}

/// A moved camera is a different clip set, so the silence above is not simply a stuck cache.
#[test]
fn a_moved_camera_changes_the_clip_set() {
    use tessella_capture_abi::envelope::ViewId;
    use tessella_capture_abi::ring::Ring;

    let mut ring = Ring::new(1 << 16);
    let (producer, _consumer) = ring.split();
    let mut sets = stencil::ClipSets::new();

    let view = probe();
    let first = stencil::clip_set(&view, 1, &cover::cover(&view).expect("covers"))
        .expect("an unrotated camera");
    assert!(sets.emit(producer, ViewId(0), &first).expect("emits"));

    let moved = ViewTransform {
        longitude: view.longitude + 0.02,
        ..view
    };
    let second = stencil::clip_set(&moved, 1, &cover::cover(&moved).expect("covers"))
        .expect("an unrotated camera");
    assert_ne!(first.tiles, second.tiles, "the placement moved");
    assert!(
        sets.emit(producer, ViewId(0), &second).expect("emits"),
        "so it is re-sent"
    );
}
