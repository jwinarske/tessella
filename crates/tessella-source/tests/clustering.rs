//! supercluster's own expectations, over supercluster's own fixture.
//!
//! # Why these numbers and not others
//!
//! Because clustering is a choice among many valid ones. Two implementations that both group
//! points within a radius disagree about *which* points end up together: the index's visit order
//! decides which cluster absorbs a point, so the grouping is a property of the whole
//! construction rather than of the radius. A style that renders correctly against mbgl renders
//! differently against anything else, and a test that only checked "nearby points are together"
//! would pass for an implementation that draws a different map.
//!
//! So these are the assertions from `vendor/supercluster/test/test.cpp`, against
//! `test/fixtures/supercluster/places.json`. They reach through everything: the projection, the
//! per-level radius, the id encoding, the tree layout, the order neighbours are visited in, and
//! the tie-breaking when a point is within reach of two clusters.

use std::collections::BTreeMap;

use tessella_source::cluster::{Clustered, Options};
use tessella_source::geojson::{GeoJsonFeature, Geometry};
use tessella_style::Value;

const PLACES: &str = include_str!("../../../tests/cluster-fixtures/places.json");

/// The fixture's points, in file order — which is what gives each its id.
fn places() -> Vec<GeoJsonFeature> {
    let document: serde_json::Value = serde_json::from_str(PLACES).expect("the fixture parses");
    document["features"]
        .as_array()
        .expect("a feature collection")
        .iter()
        .map(|feature| {
            let coordinates = &feature["geometry"]["coordinates"];
            let point = [
                coordinates[0].as_f64().expect("a longitude"),
                coordinates[1].as_f64().expect("a latitude"),
            ];
            let mut properties = BTreeMap::new();
            if let Some(name) = feature["properties"]["name"].as_str() {
                properties.insert("name".to_owned(), Value::String(name.to_owned()));
            }
            GeoJsonFeature {
                id: None,
                properties,
                geometry: Geometry::Point(vec![point]),
            }
        })
        .collect()
}

fn index() -> Clustered {
    Clustered::new(places(), Options::default())
}

fn name(feature: &GeoJsonFeature) -> &str {
    match feature.properties.get("name") {
        Some(Value::String(name)) => name,
        other => panic!("expected a name, got {other:?}"),
    }
}

fn count(properties: &BTreeMap<String, Value>) -> u64 {
    match properties.get("point_count") {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Some(Value::Number(count)) => *count as u64,
        other => panic!("expected a point count, got {other:?}"),
    }
}

/// The whole world at zoom zero: thirty-nine features standing for a hundred and ninety-six
/// points.
///
/// The second number is the one that catches a dropped point: a grouping that lost one still
/// reports a plausible feature count, and only the sum says every place is still represented.
#[test]
fn the_world_tile_holds_what_supercluster_says_it_does() {
    let tile = index().tile(0, 0, 0);
    assert_eq!(tile.len(), 39, "features in the tile at 0/0/0");

    let points: u64 = tile
        .iter()
        .map(|feature| {
            if feature.properties.get("cluster") == Some(&Value::Bool(true)) {
                count(&feature.properties)
            } else {
                1
            }
        })
        .sum();
    assert_eq!(points, 196, "original points those features stand for");
}

/// Cluster one splits into four, and into these four.
///
/// The counts are what pin the grouping: six, seven and two points, and a single place that did
/// not cluster with anything. Their *order* is the index's, not a sort — so this also pins the
/// tree layout, which nothing else can.
#[test]
fn a_clusters_children_are_the_ones_it_was_made_from() {
    let children = index().children(1).expect("cluster 1 has children");
    assert_eq!(children.len(), 4);
    assert_eq!(count(&children[0].properties), 6);
    assert_eq!(count(&children[1].properties), 7);
    assert_eq!(count(&children[2].properties), 2);
    assert_eq!(name(&children[3]), "Bermuda Islands");
}

/// Where each cluster stops being one.
#[test]
fn a_cluster_expands_at_the_zoom_that_splits_it() {
    let index = index();
    for (cluster_id, zoom) in [(1, 1), (33, 1), (353, 2), (833, 2), (1857, 3)] {
        assert_eq!(
            index.expansion_zoom(cluster_id).expect("a known cluster"),
            zoom,
            "cluster {cluster_id}"
        );
    }
}

/// The points under a cluster, ten of them from an offset of five.
///
/// Depth-first through the sub-clusters, so the offset skips whole clusters where it can and
/// descends where it cannot — which is why the names are these and in this order.
#[test]
fn leaves_come_back_in_the_index_s_own_order() {
    let leaves = index().leaves(1, 10, 5).expect("cluster 1 has leaves");
    let names: Vec<&str> = leaves.iter().map(name).collect();
    assert_eq!(
        names,
        vec![
            "Niagara Falls",
            "Cape San Blas",
            "Cape Sable",
            "Cape Canaveral",
            "San  Salvador",
            "Cabo Gracias a Dios",
            "I. de Cozumel",
            "Grand Cayman",
            "Miquelon",
            "Cape Bauld",
        ]
    );
}

/// An id naming no cluster is refused rather than answered with nothing.
///
/// Not id zero, which was the obvious case to try and is not one. A cluster's id is
/// `(index << 5) + (zoom + 1)`, so its low five bits are at least one and zero is never a
/// cluster. Asking for it answers *every* cluster whose `parent_id` is still the default —
/// which is what supercluster does too, since it defaults that field to zero and compares on
/// it. The id is unreachable rather than the comparison being wrong.
#[test]
fn an_unknown_cluster_id_is_an_error() {
    let index = index();
    // Thirty-one in the low bits names a level past `max_zoom + 1`.
    assert!(index.children(31).is_err(), "no such level");
    // A level that exists, with nothing at that index in it.
    assert!(index.children(!31 | 1).is_err(), "no such cluster");
}

/// The label a cluster draws.
#[test]
fn the_abbreviated_count_reads_as_supercluster_writes_it() {
    let tile = index().tile(0, 0, 0);
    for feature in &tile {
        if feature.properties.get("cluster") != Some(&Value::Bool(true)) {
            continue;
        }
        let count = count(&feature.properties);
        let Some(Value::String(abbreviated)) = feature.properties.get("point_count_abbreviated")
        else {
            panic!("a cluster with no abbreviated count")
        };
        // Every count in this fixture is under a thousand, so the abbreviation is the number —
        // which is the case a style actually draws for a map of this size.
        assert_eq!(abbreviated, &count.to_string());
    }
}
