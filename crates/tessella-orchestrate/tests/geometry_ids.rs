//! What a geometry id means, and what it does not.
//!
//! # Why this is asserted rather than left to the doc
//!
//! §5.3 describes geometry as process-scoped and refcounted, and the ABI was written to that —
//! `ViewRelease` and `GeometryRemove` exist for it. The producer does not implement it. Ids are
//! dense from zero in every emission, so the same id names a different tile after a pan.
//!
//! That gap is invisible from inside: every frame renders correctly, every count agrees, and a
//! consumer only discovers it by caching on the id and drawing one tile's geometry under
//! another's matrix. The documentation now says so, and this holds the documentation to the
//! producer — if the lifecycle is built, these assertions fail and the prose comes with them.

use std::collections::BTreeMap;

use tessella_capture_abi::EnvelopeKind;
use tessella_capture_abi::envelope::{GeometryAdd, ViewId, WireRecord as _};
use tessella_capture_abi::ring::Ring;
use tessella_orchestrate::SlabArena;
use tessella_orchestrate::frame::{self, Frame};
use tessella_orchestrate::tile::{TileId, build_mvt_tile, build_sourceless};
use tessella_source::mvt::Tile;
use tessella_style::Style;
use tessella_style::light::Light;
use tessella_tile::camera;
use tessella_tile::cover::{self, ViewTransform};

const REAL_TILE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/real-world-0-0-0.mvt");

const STYLE: &str = r#"{"version": 8, "sources": {"s": {"type": "vector", "tiles": []}},
  "layers": [{"id": "f", "type": "fill", "source": "s", "source-layer": "water",
              "paint": {"fill-color": "red"}}]}"#;

/// Emits one frame at `longitude`, returning each geometry id and the tile it belongs to.
fn emit_at(longitude: f64) -> (Vec<u64>, Vec<String>) {
    let style = Style::parse(STYLE).expect("parses");
    let view = camera::settled(&ViewTransform {
        longitude,
        latitude: 0.0,
        zoom: 3.0,
        width: 512.0,
        height: 512.0,
        bearing: 0.0,
        pitch: 0.0,
    });
    let tiles = cover::cover(&view).expect("covers");
    let decoded = Tile::decode(REAL_TILE).expect("decodes");
    let mut buckets = Vec::new();
    for tile in &tiles {
        let id = TileId::new(tile.z, tile.x, tile.y);
        let mut built = build_mvt_tile(&style, "s", id, &decoded).expect("builds");
        built.extend(build_sourceless(&style, id).expect("background"));
        built.sort_by_key(|bucket| bucket.layer_index);
        buckets.push((id, built));
    }

    let mut arena = SlabArena::new();
    let mut ring = Ring::new(1 << 22);
    let (producer, consumer) = ring.split();
    frame::emit(
        producer,
        &mut arena,
        &Frame {
            style: &style,
            view: &view,
            view_id: ViewId(0),
            tiles: &tiles,
            buckets: &buckets,
            light: &Light::default(),
            fonts: None,
            patterns: None,
        },
    )
    .expect("emits");

    let mut ids = Vec::new();
    while let Some(record) = consumer.peek() {
        if record.kind == EnvelopeKind::GeometryAdd
            && let Some(add) = GeometryAdd::from_bytes(record.record)
        {
            ids.push(add.geometry.0);
        }
        let consumed = record.consumed();
        consumer.advance(consumed);
    }
    let names = tiles
        .iter()
        .map(|tile| format!("{}/{}/{}", tile.z, tile.x, tile.y))
        .collect();
    (ids, names)
}

/// Ids are dense from zero, every emission.
#[test]
fn ids_are_dense_from_zero() {
    let (ids, _) = emit_at(0.0);
    assert!(!ids.is_empty());
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), ids.len(), "no id is announced twice");
    assert_eq!(
        sorted,
        (0..ids.len() as u64).collect::<Vec<_>>(),
        "dense from zero: {ids:?}"
    );
}

/// After a pan the same ids come back, naming a different cover.
///
/// This is the whole of what "not process-wide" means, and it is what a consumer caching on the
/// id would get wrong. The covers overlap — two tiles are in both — and even those two do not
/// keep their ids, because the ids are handed out in cover order and the order shifted.
#[test]
fn a_pan_reuses_the_ids_for_a_different_cover() {
    let (first_ids, first_tiles) = emit_at(0.0);
    let (second_ids, second_tiles) = emit_at(60.0);

    assert_ne!(first_tiles, second_tiles, "the cover has to differ");
    let shared: Vec<&String> = first_tiles
        .iter()
        .filter(|tile| second_tiles.contains(tile))
        .collect();
    assert!(!shared.is_empty(), "the covers should overlap: {shared:?}");

    assert_eq!(
        first_ids, second_ids,
        "the same ids are handed out for a different cover, which is what makes them \
         unusable as a cache key"
    );

    // And the tile at a given position changed, so id N is not the tile it was.
    let position = first_tiles
        .iter()
        .position(|tile| {
            tile != &second_tiles[first_tiles.iter().position(|t| t == tile).unwrap_or(0)]
        })
        .unwrap_or(0);
    assert_ne!(
        first_tiles[position], second_tiles[position],
        "some position holds a different tile, which is the id that moved"
    );
}

/// Nothing is ever released, which is the other half of the same fact.
#[test]
fn nothing_is_released_because_nothing_is_retained() {
    let style = Style::parse(STYLE).expect("parses");
    let view = camera::settled(&ViewTransform {
        longitude: 0.0,
        latitude: 0.0,
        zoom: 3.0,
        width: 512.0,
        height: 512.0,
        bearing: 0.0,
        pitch: 0.0,
    });
    let tiles = cover::cover(&view).expect("covers");
    let decoded = Tile::decode(REAL_TILE).expect("decodes");
    let mut buckets = Vec::new();
    for tile in &tiles {
        let id = TileId::new(tile.z, tile.x, tile.y);
        let mut built = build_mvt_tile(&style, "s", id, &decoded).expect("builds");
        built.extend(build_sourceless(&style, id).expect("background"));
        built.sort_by_key(|bucket| bucket.layer_index);
        buckets.push((id, built));
    }

    let mut arena = SlabArena::new();
    let mut ring = Ring::new(1 << 22);
    let (producer, consumer) = ring.split();
    frame::emit(
        producer,
        &mut arena,
        &Frame {
            style: &style,
            view: &view,
            view_id: ViewId(0),
            tiles: &tiles,
            buckets: &buckets,
            light: &Light::default(),
            fonts: None,
            patterns: None,
        },
    )
    .expect("emits");

    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    while let Some(record) = consumer.peek() {
        *kinds.entry(format!("{:?}", record.kind)).or_default() += 1;
        let consumed = record.consumed();
        consumer.advance(consumed);
    }

    for kind in ["ViewRelease", "GeometryRemove", "ViewUndeclare"] {
        assert_eq!(
            kinds.get(kind).copied().unwrap_or(0),
            0,
            "{kind} is defined in the ABI and never emitted: {kinds:?}"
        );
    }
}
