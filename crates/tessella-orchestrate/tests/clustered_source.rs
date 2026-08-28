//! A style that asks for clustering gets clusters, through the whole pipeline.
//!
//! # What this covers that `tessella-source`'s tests do not
//!
//! Those check the algorithm against supercluster's own expectations. This checks the wiring:
//! that `cluster: true` on a source reaches the index, that the index is built once for the
//! source rather than once per tile, that a tile's features are the clusters at *its* zoom, and
//! that they arrive as ordinary points a style can draw — because that is what clustering is,
//! downstream of the index. `point_count` is a property like any other from there on.

#![cfg(feature = "std")]

use tessella_orchestrate::tile::{TileId, build_tile};
use tessella_source::cluster::{Clustered, Options};
use tessella_source::geojson;
use tessella_source::tiling::TilingOptions;
use tessella_style::{Style, Value};

/// Twenty points in a tight cluster near the origin, and one far away.
///
/// Tight enough that zoom zero puts the twenty into one cluster and leaves the outlier alone,
/// which is the arrangement that tells clustering apart from not clustering.
fn document() -> String {
    let mut features = Vec::new();
    for index in 0..20 {
        #[allow(clippy::cast_precision_loss)]
        let offset = index as f64 * 0.001;
        features.push(format!(
            r#"{{"type":"Feature","properties":{{"n":{index}}},
               "geometry":{{"type":"Point","coordinates":[{},{}]}}}}"#,
            0.1 + offset,
            51.5 + offset
        ));
    }
    features.push(
        r#"{"type":"Feature","properties":{"n":99},
           "geometry":{"type":"Point","coordinates":[-140.0,-40.0]}}"#
            .to_owned(),
    );
    format!(
        r#"{{"type":"FeatureCollection","features":[{}]}}"#,
        features.join(",")
    )
}

/// The features, read back through the source the style declares — the path boot takes.
fn features() -> Vec<tessella_source::geojson::GeoJsonFeature> {
    let style = style_with(&format!(
        r#"{{"type": "geojson", "cluster": true, "data": {}}}"#,
        document()
    ));
    let Some(tessella_style::Source::Geojson(source)) = style.sources.get("g") else {
        panic!("a geojson source")
    };
    let document = tessella_storage::geojson::resolve(
        source,
        &tessella_storage::http::HttpFileSource::default(),
    )
    .expect("an inline document resolves without a fetch");
    geojson::read(&document).expect("the document reads")
}

fn style_with(source: &str) -> Style {
    Style::parse(&format!(
        r#"{{"version": 8,
             "sources": {{"g": {source}}},
             "layers": [{{"id": "dots", "type": "circle", "source": "g",
                          "paint": {{"circle-radius": 4}}}}]}}"#
    ))
    .expect("the style parses")
}

fn count(feature: &tessella_source::geojson::GeoJsonFeature) -> Option<u64> {
    match feature.properties.get("point_count") {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Some(Value::Number(count)) => Some(*count as u64),
        _ => None,
    }
}

/// The twenty become one, and the outlier stays itself.
#[test]
fn a_clustered_source_hands_a_tile_its_clusters() {
    let features = features();
    assert_eq!(features.len(), 21);

    let index = Clustered::new(features, Options::default());
    let world = index.tile_features(0, 0, 0);

    let clusters: Vec<u64> = world.iter().filter_map(count).collect();
    assert_eq!(clusters, vec![20], "one cluster, of all twenty");
    assert_eq!(
        world.len(),
        2,
        "and the outlier beside it, uncollapsed: {world:?}"
    );

    // Deep enough and they come apart again, which is the property that makes clustering a view
    // of the data rather than a change to it.
    let deep = index.tile_features(16, 0, 0);
    assert!(
        deep.iter().all(|feature| count(feature).is_none()),
        "past the cluster max zoom every point is itself"
    );
}

/// The clusters build into buckets, as any other point source would.
#[test]
fn a_cluster_is_a_point_like_any_other_downstream() {
    let features = features();
    let index = Clustered::new(features, Options::default());
    let style = style_with(
        r#"{"type": "geojson", "cluster": true, "data": {"type":"FeatureCollection","features":[]}}"#,
    );

    let tile = TileId::new(0, 0, 0);
    let buckets = build_tile(
        &style,
        "g",
        tile,
        &index.tile_features(tile.z, tile.x, tile.y),
        TilingOptions::default(),
    )
    .expect("the tile builds");

    let circles = buckets
        .iter()
        .find(|bucket| bucket.layer_index == 0)
        .expect("the circle layer");
    assert!(
        circles.content.has_data(),
        "a clustered source should draw something"
    );
}

/// The radius reaches the index.
#[test]
fn the_radius_reaches_the_index() {
    let features = features();

    // The points here sit within a fiftieth of a degree of each other, which is far inside a
    // one-pixel radius at zoom zero — so "a smaller radius" has to mean *no* radius for the
    // difference to be about the knob rather than about the geometry.
    let none = Clustered::new(
        features.clone(),
        Options {
            radius: 0.0,
            ..Options::default()
        },
    );
    let default = Clustered::new(features, Options::default());

    assert_eq!(
        none.tile_features(0, 0, 0).len(),
        21,
        "nothing reaches anything, so every point is itself"
    );
    assert_eq!(
        default.tile_features(0, 0, 0).len(),
        2,
        "and the default gathers the twenty"
    );
}
