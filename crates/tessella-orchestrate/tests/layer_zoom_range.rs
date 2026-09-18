// SPDX-License-Identifier: BSD-2-Clause
//! A layer's `minzoom` and `maxzoom` between two whole zooms.
//!
//! A tile's zoom is a whole number and the view's is not, so mbgl asks two questions. A tile
//! builds a layer when its zoom is inside the bounds rounded outward (`GeometryTile`), and a frame
//! draws and places it when the view's zoom is inside them exactly (`RenderLayer::supportsZoom`).
//! tessella asked only the first, and asked it of the bounds as written: a layer with
//! `minzoom: 15.5` was never built in the z15 tile a view at 15.5 draws from. bright's path names
//! are that layer, and display-buildings-in-3d, at 15.5, drew none of them.

use std::collections::BTreeSet;
use std::sync::Arc;

use tessella_capture_abi::EnvelopeKind;
use tessella_capture_abi::ProjectionMode;
use tessella_capture_abi::envelope::{OrderEntry, OrderUpdate, ViewId, WireRecord};
use tessella_capture_abi::ring::Ring;
use tessella_orchestrate::SlabArena;
use tessella_orchestrate::frame::{self, Frame};
use tessella_orchestrate::tile::{TileId, build_tile};
use tessella_source::geojson::{self, GeoJsonFeature};
use tessella_source::tiling::TilingOptions;
use tessella_style::light::Light;
use tessella_style::{Source, Style};
use tessella_tile::camera;
use tessella_tile::cover::{self, ViewTransform};

/// A square around the origin, larger than any view below, and three layers over it.
const STYLE: &str = r#"{
  "version": 8,
  "sources": {"data": {"type": "geojson", "data": {"type": "Feature", "properties": {},
    "geometry": {"type": "Polygon",
      "coordinates": [[[-1, -1], [1, -1], [1, 1], [-1, 1], [-1, -1]]]}}}},
  "layers": [
    {"id": "always", "type": "fill", "source": "data"},
    {"id": "from", "type": "fill", "source": "data", "minzoom": 15.5},
    {"id": "until", "type": "fill", "source": "data", "maxzoom": 15.5}
  ]
}"#;

const ALWAYS: u32 = 0;
const FROM: u32 = 1;
const UNTIL: u32 = 2;

fn style_and_features() -> (Style, Vec<GeoJsonFeature>) {
    let style = Style::parse(STYLE).expect("the style parses");
    let Some(Source::Geojson(source)) = style.source("data") else {
        panic!("a geojson source");
    };
    let features = geojson::read(&source.data).expect("features");
    (style, features)
}

/// The layers a tile at `z` builds, by style index.
fn built_at(z: u8) -> BTreeSet<u32> {
    let (style, features) = style_and_features();
    // The tile holding the origin's north-east corner, which the square covers.
    let middle = 1u32 << (z - 1);
    build_tile(
        &style,
        "data",
        TileId::new(z, middle, middle - 1),
        &features,
        TilingOptions::default(),
    )
    .expect("the tile builds")
    .iter()
    .map(|bucket| u32::try_from(bucket.layer_index).expect("a style index"))
    .collect()
}

/// The layers one frame draws at view zoom `zoom`, by style index.
fn drawn_at(zoom: f64) -> BTreeSet<u32> {
    let (style, features) = style_and_features();
    let view = camera::settled(&ViewTransform {
        longitude: 0.0,
        latitude: 0.0,
        zoom,
        width: 512.0,
        height: 512.0,
        bearing: 0.0,
        pitch: 0.0,
        ground_below: 0.0,
    });
    let tiles = cover::cover(&view).expect("covers");
    let buckets: Vec<_> = tiles
        .iter()
        .map(|tile| {
            let id = TileId::new(tile.z, tile.x, tile.y);
            let built = build_tile(&style, "data", id, &features, TilingOptions::default())
                .expect("the tile builds");
            (id, Arc::new(built))
        })
        .collect();

    let mut ring = Ring::new(1 << 24);
    let (producer, consumer) = ring.split();
    let mut arena = SlabArena::new();
    frame::emit(
        producer,
        &mut arena,
        &Frame {
            projection: ProjectionMode::Mercator,
            style: &style,
            view: &view,
            view_id: ViewId(0),
            tiles: &tiles,
            buckets: &buckets,
            origins: &[],
            light: &Light::default(),
            fonts: None,
            patterns: None,
        },
    )
    .expect("the frame emits");

    let mut order: Vec<OrderEntry> = Vec::new();
    while let Some(record) = consumer.peek() {
        if record.kind == EnvelopeKind::OrderUpdate
            && let Some(update) = OrderUpdate::from_bytes(record.record)
        {
            let size = size_of::<OrderEntry>();
            let start = update.entries.offset as usize;
            order = (0..update.entries.count as usize)
                .filter_map(|index| {
                    record
                        .payload
                        .get(start + index * size..)
                        .and_then(OrderEntry::from_bytes)
                })
                .collect();
        }
        let consumed = record.consumed();
        consumer.advance(consumed);
    }
    assert!(!order.is_empty(), "the frame emitted an order");
    order.iter().map(|entry| entry.layer_index).collect()
}

/// A z15 tile serves views from 15 up to 16, so it builds both layers whose bound falls inside
/// that range; a z14 tile is below `minzoom: 15.5` entirely, and a z16 tile above `maxzoom: 15.5`.
#[test]
fn a_tile_builds_a_layer_whose_bounds_fall_inside_its_zoom() {
    assert_eq!(built_at(15), BTreeSet::from([ALWAYS, FROM, UNTIL]));
    assert_eq!(built_at(14), BTreeSet::from([ALWAYS, UNTIL]));
    assert_eq!(built_at(16), BTreeSet::from([ALWAYS, FROM]));
}

/// The view decides between them, inclusive at both ends as mbgl's `supportsZoom` is.
#[test]
fn a_frame_draws_a_layer_only_inside_its_bounds() {
    assert_eq!(drawn_at(15.2), BTreeSet::from([ALWAYS, UNTIL]));
    assert_eq!(drawn_at(15.5), BTreeSet::from([ALWAYS, FROM, UNTIL]));
    assert_eq!(drawn_at(15.7), BTreeSet::from([ALWAYS, FROM]));
}
