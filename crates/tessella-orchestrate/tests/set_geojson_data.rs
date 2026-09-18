// SPDX-License-Identifier: BSD-2-Clause
//! Replacing a GeoJSON source's data on a running map.
//!
//! GL JS's `map.getSource(id).setData(...)`, and mbgl's `GeoJSONSource::setGeoJSONData`: nine of
//! the documentation examples are a source whose data is replaced, from a point moving along a
//! route to live data arriving over a socket, and none of them can be asked of this build without
//! it.
//!
//! # What has to be true
//!
//! That the map afterwards is the map that style would have drawn -- the replacement is data, not
//! a second code path, so a map handed new data must agree with one created with it. And that the
//! replacement costs what it should: the source's own tiles are built again and nobody else's
//! are, because the animation examples call this every frame over a basemap that has not changed.

#![cfg(feature = "std")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use tessella_orchestrate::boot::BootError;
use tessella_orchestrate::cache::TileCache;
use tessella_orchestrate::deferred::TileTransport;
use tessella_orchestrate::map::Tiles;
use tessella_orchestrate::pool::Pool;
use tessella_orchestrate::source::{Readiness, SetDataError, TileSource};
use tessella_orchestrate::tile::{Content, LayerBucket, TileId};
use tessella_storage::http::HttpFileSource;
use tessella_storage::source::Coalescing;
use tessella_tile::cover::{TileCoord, ViewTransform};
use tessella_tile::store::Surface;

/// A document of `count` points, spaced so they land in the zoom-zero tile.
fn points(count: usize) -> String {
    let features: Vec<String> = (0..count)
        .map(|index| {
            #[allow(clippy::cast_precision_loss)]
            let offset = index as f64 * 0.5;
            format!(
                r#"{{"type":"Feature","properties":{{}},
                   "geometry":{{"type":"Point","coordinates":[{},{}]}}}}"#,
                0.1 + offset,
                51.5 - offset
            )
        })
        .collect();
    format!(
        r#"{{"type":"FeatureCollection","features":[{}]}}"#,
        features.join(",")
    )
}

/// A style with one GeoJSON source over one vector source, so the cost of a replacement is
/// visible: the vector source's tiles must survive it.
fn style(document: &str) -> String {
    format!(
        r#"{{"version": 8,
             "sources": {{
               "g": {{"type": "geojson", "data": {document}}},
               "v": {{"type": "vector", "tiles": ["http://127.0.0.1:1/{{z}}/{{x}}/{{y}}.pbf"]}}
             }},
             "layers": [
               {{"id": "dots", "type": "circle", "source": "g", "paint": {{"circle-radius": 4}}}},
               {{"id": "sea", "type": "fill", "source": "v", "source-layer": "water"}}
             ]}}"#
    )
}

fn view() -> ViewTransform {
    tessella_tile::camera::settled(&ViewTransform {
        longitude: 0.0,
        latitude: 0.0,
        zoom: 0.0,
        width: 512.0,
        height: 512.0,
        bearing: 0.0,
        pitch: 0.0,
    })
}

type Source = Arc<tessella_orchestrate::source::Pooled<HttpFileSource>>;

/// A source over the shared pool, and the cache it builds into.
fn source_of(document: &str) -> (Source, Arc<TileCache<BootError>>) {
    let files = Arc::new(Coalescing::new(HttpFileSource::new(Duration::from_secs(
        30,
    ))));
    let cache: Arc<TileCache<BootError>> = Arc::new(TileCache::new(64));
    let source = TileSource::new(
        style(document),
        Arc::clone(&files),
        Arc::clone(&cache),
        Pool::shared(),
        1,
    );
    (source, cache)
}

fn settle<D: TileTransport + 'static>(
    source: &Arc<TileSource<D>>,
    mut done: impl FnMut() -> bool,
) -> bool {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        source.drain();
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

/// Asks for the zoom-zero tile until its buckets satisfy `wanted`.
///
/// The predicate is the point: after a replacement the old tile is still there to draw -- that is
/// the behavior `the_tiles_built_before_a_replacement_are_not_dropped` asserts -- so a wait that
/// stopped at "some buckets" would read the tile the replacement was meant to supersede.
fn draw_until<D: TileTransport + 'static>(
    source: &Arc<TileSource<D>>,
    wanted: impl Fn(&[LayerBucket]) -> bool,
) -> Arc<Vec<LayerBucket>> {
    let cover = [TileCoord {
        z: 0,
        x: 0,
        y: 0,
        wrap: 0,
    }];
    assert!(
        settle(source, || {
            source.want(&view(), &cover, &[], Surface::Plane);
            source.readiness() == Readiness::Ready
                && source
                    .buckets(TileId::new(0, 0, 0))
                    .is_some_and(|buckets| wanted(&buckets))
        }),
        "the tile the test was waiting for never landed"
    );
    source.buckets(TileId::new(0, 0, 0)).expect("buckets")
}

/// The same, for a caller that wants whatever is there.
fn draw<D: TileTransport + 'static>(source: &Arc<TileSource<D>>) -> Arc<Vec<LayerBucket>> {
    draw_until(source, |buckets| !buckets.is_empty())
}

/// How many circle vertices the tile carries, which is four per point the document held.
fn dots(buckets: &[LayerBucket]) -> usize {
    buckets
        .iter()
        .filter_map(|bucket| match &bucket.content {
            Content::Circle(circle) => Some(circle.vertices.len()),
            _ => None,
        })
        .sum()
}

/// The map after a replacement is the map that document would have drawn.
///
/// The claim that makes this data rather than a second code path.
#[test]
fn replaced_data_draws_as_though_the_style_carried_it() {
    let (replaced, _) = source_of(&points(3));
    let first = dots(&draw(&replaced));

    let document: tessella_style::Value =
        serde_json::from_str(&points(7)).expect("the document parses");
    replaced
        .set_geojson_data("g", &document)
        .expect("the source takes it");

    let after = dots(&draw_until(&replaced, |buckets| dots(buckets) != first));
    assert_ne!(after, first, "the replacement changed nothing");

    // The same document, in the style from the start.
    let (fresh, _) = source_of(&points(7));
    assert_eq!(
        after,
        dots(&draw(&fresh)),
        "a replaced source draws differently from one created with the same data"
    );
}

/// Only the replaced source is built again.
///
/// The animation examples call this every frame over a basemap that has not changed, so a
/// replacement that invalidated the whole cache would be unusable at the rate they need. The
/// vector source in this style never answers -- its tiles fail to fetch -- so the counter this
/// reads is the GeoJSON source's own builds, and what matters is that it moves by one tile's
/// worth rather than by the cache.
#[test]
fn a_replacement_rebuilds_its_own_source_and_no_other() {
    let (source, cache) = source_of(&points(3));
    draw(&source);
    let before = cache.builds();

    let document: tessella_style::Value =
        serde_json::from_str(&points(5)).expect("the document parses");
    source
        .set_geojson_data("g", &document)
        .expect("the source takes it");
    draw_until(&source, |buckets| dots(buckets) == 5 * 4);

    assert_eq!(
        cache.builds() - before,
        1,
        "a replacement built more than the one tile of the one source it replaced"
    );
}

/// The old tiles are still there to draw while the new ones are built.
///
/// mbgl keeps what is drawn until the replacement lands, which is what stops a map blinking at
/// every frame of an animation. Here that shows as the cache keeping both: the old key is not
/// evicted by the new one arriving.
#[test]
fn the_tiles_built_before_a_replacement_are_not_dropped() {
    let (source, cache) = source_of(&points(3));
    draw(&source);
    let held = cache.len();

    let document: tessella_style::Value =
        serde_json::from_str(&points(5)).expect("the document parses");
    source
        .set_geojson_data("g", &document)
        .expect("the source takes it");
    draw_until(&source, |buckets| dots(buckets) == 5 * 4);

    assert_eq!(
        cache.len(),
        held + 1,
        "the replacement's tile did not join the one it replaced"
    );
}

/// A source the style does not have, and a document that is not GeoJSON, are the caller's
/// mistakes and are reported as such.
#[test]
fn what_cannot_be_replaced_says_so() {
    let (source, _) = source_of(&points(3));
    draw(&source);

    let document: tessella_style::Value =
        serde_json::from_str(&points(1)).expect("the document parses");
    assert_eq!(
        source.set_geojson_data("nowhere", &document),
        Err(SetDataError::NoSuchSource),
        "a source the style does not have"
    );
    assert_eq!(
        source.set_geojson_data("v", &document),
        Err(SetDataError::NoSuchSource),
        "a source that is not GeoJSON"
    );
    let not_geojson: tessella_style::Value =
        serde_json::from_str(r#"{"type": "Nonsense"}"#).expect("it parses as JSON");
    assert!(
        matches!(
            source.set_geojson_data("g", &not_geojson),
            Err(SetDataError::BadDocument(_))
        ),
        "a document that is not GeoJSON"
    );
}

/// Before the style resolves there is no source list to name.
#[test]
fn a_map_that_has_not_read_its_style_has_nothing_to_replace() {
    let (source, _) = source_of(&points(3));
    let document: tessella_style::Value =
        serde_json::from_str(&points(1)).expect("the document parses");
    assert_eq!(
        source.set_geojson_data("g", &document),
        Err(SetDataError::NotResolved),
        "a source that has not resolved took data for a source it has not read"
    );
}

/// A clustered source is re-clustered from the data it is handed.
///
/// The index a clustered source's tiles are cut from is built once at resolution, from the
/// document the style carried. Handing over new data has to build a new one, or the map draws the
/// old document's clusters over the new document's points -- which is the failure a replacement
/// that only swapped the feature list would have.
mod clustered {
    use super::{Readiness, TileCoord, TileId, TileSource, Tiles, settle, view};
    use std::sync::Arc;
    use std::time::Duration;
    use tessella_orchestrate::boot::BootError;
    use tessella_orchestrate::cache::TileCache;
    use tessella_orchestrate::pool::Pool;
    use tessella_orchestrate::tile::{Content, LayerBucket};
    use tessella_storage::http::HttpFileSource;
    use tessella_storage::source::Coalescing;
    use tessella_tile::store::Surface;

    /// Points a tenth of a degree apart, which zoom zero clusters into one.
    fn tight(count: usize, at: (f64, f64)) -> String {
        let features: Vec<String> = (0..count)
            .map(|index| {
                #[allow(clippy::cast_precision_loss)]
                let offset = index as f64 * 0.01;
                format!(
                    r#"{{"type":"Feature","properties":{{}},
                       "geometry":{{"type":"Point","coordinates":[{},{}]}}}}"#,
                    at.0 + offset,
                    at.1 + offset
                )
            })
            .collect();
        features.join(",")
    }

    fn style(document: &str) -> String {
        format!(
            r#"{{"version": 8,
                 "sources": {{"g": {{"type": "geojson", "cluster": true,
                                     "data": {{"type": "FeatureCollection",
                                               "features": [{document}]}}}}}},
                 "layers": [{{"id": "dots", "type": "circle", "source": "g",
                              "paint": {{"circle-radius": 4}}}}]}}"#
        )
    }

    /// Circle vertices in the tile, which is four per cluster or lone point drawn.
    fn dots(buckets: &[LayerBucket]) -> usize {
        buckets
            .iter()
            .filter_map(|bucket| match &bucket.content {
                Content::Circle(circle) => Some(circle.vertices.len()),
                _ => None,
            })
            .sum()
    }

    #[test]
    fn replaced_data_is_clustered_again() {
        let files = Arc::new(Coalescing::new(HttpFileSource::new(Duration::from_secs(
            30,
        ))));
        let cache: Arc<TileCache<BootError>> = Arc::new(TileCache::new(64));
        // One tight group, which is one cluster at zoom zero.
        let source = TileSource::new(
            style(&tight(20, (0.1, 51.5))),
            files,
            cache,
            Pool::shared(),
            1,
        );
        let cover = [TileCoord {
            z: 0,
            x: 0,
            y: 0,
            wrap: 0,
        }];
        let landed = |wanted: usize| {
            settle(&source, || {
                source.want(&view(), &cover, &[], Surface::Plane);
                source.readiness() == Readiness::Ready
                    && source
                        .buckets(TileId::new(0, 0, 0))
                        .is_some_and(|buckets| dots(&buckets) == wanted)
            })
        };
        assert!(landed(4), "one group is one cluster");

        // The same group and a second one far enough away to cluster on its own.
        let document: tessella_style::Value = serde_json::from_str(&format!(
            r#"{{"type":"FeatureCollection","features":[{},{}]}}"#,
            tight(20, (0.1, 51.5)),
            tight(20, (-140.0, -40.0))
        ))
        .expect("the document parses");
        source
            .set_geojson_data("g", &document)
            .expect("the source takes it");

        assert!(
            landed(8),
            "the replacement was not clustered -- two groups should draw two clusters"
        );
    }
}
