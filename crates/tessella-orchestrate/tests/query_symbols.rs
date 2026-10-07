//! A query finds the labels that are drawn, and only those.
//!
//! # What this is for
//!
//! The last part of #338's answer. A label is the thing a host most wants to tap -- a POI name, a
//! road shield -- and the only family whose extent is not in a tile at all: what a label occupies is
//! its collision box, in screen space, decided per frame against every other label on the map.
//!
//! # What would be caught
//!
//! A query that returns a label nobody can see. Two labels competing for one spot is the ordinary
//! case on a real map, not an edge one: a place name beats a house number, and the house number is
//! *drawn nowhere*. A query that answered it would send a host to a feature the user cannot see and
//! did not point at, and nothing about the map would look wrong.
//!
//! The second test here is that case, built by competing a layer against an identical one.

use std::collections::BTreeMap;
use std::sync::Arc;

use tessella_capture_abi::envelope::ViewId;
use tessella_capture_abi::ring::{self, region_size};
use tessella_glyph::fonts::{Dependencies, Fonts};
use tessella_orchestrate::map::{Map, Tiles};
use tessella_orchestrate::tile::{Content, LayerBucket, TileId, build_tile};
use tessella_source::tiling::TilingOptions;
use tessella_storage::source::{FetchError, FileSource, Response};
use tessella_style::{Style, Value};
use tessella_tile::camera;
use tessella_tile::cover::ViewTransform;

/// Serves the `file://` URLs the style's glyph template builds.
struct Disk;
impl FileSource for Disk {
    fn fetch(&self, url: &str) -> Result<Response, FetchError> {
        let path = url.strip_prefix("file://").unwrap_or(url);
        Ok(Response {
            status: 200,
            body: std::fs::read(path).unwrap_or_default(),
            ..Response::default()
        })
    }
}

/// Three named points, the same three the symbol capture uses.
///
/// Only **Alpha** falls in [`TILE`]; the other two are in neighboring z13 tiles, which the store
/// below does not serve. That is why every tap here is at Alpha, and it is load-bearing in a second
/// way: Alpha is drawn at screen y 571 of 768, well off center, so a query whose y axis ran the wrong
/// way would miss it. A label in the middle of the viewport is its own mirror and proves nothing.
const POINTS: &str = r##"{"type":"FeatureCollection","features":[
  {"type":"Feature","id":11,"properties":{"name":"Alpha"},
   "geometry":{"type":"Point","coordinates":[-0.13,51.515]}},
  {"type":"Feature","id":22,"properties":{"name":"Bravo"},
   "geometry":{"type":"Point","coordinates":[-0.09,51.495]}},
  {"type":"Feature","id":33,"properties":{"name":"Charlie"},
   "geometry":{"type":"Point","coordinates":[-0.11,51.505]}}]}"##;

/// Alpha is alone in one tile; Bravo and Charlie share the one below it.
///
/// That split is what the tests rest on. Alpha's tile proves the y axis -- it is drawn well off
/// center -- and Bravo and Charlie's shared tile proves the identity, because they are two labels of
/// one layout and telling them apart means following each symbol back to *its* pending rather than to
/// the layout's first.
const ALPHA: [f64; 2] = [-0.13, 51.515];
const BRAVO: [f64; 2] = [-0.09, 51.495];
const CHARLIE: [f64; 2] = [-0.11, 51.505];

/// The two tiles those three points fall in.
const TILES: [TileId; 2] = [TileId::new(13, 4093, 2723), TileId::new(13, 4093, 2724)];

/// A style over them, with the symbol layers given as `(id, allow_overlap)`.
fn style_with(layers: &[(&str, bool)]) -> Style {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let written: Vec<String> = layers
        .iter()
        .map(|(id, overlap)| {
            format!(
                r##"{{"id":"{id}","type":"symbol","source":"probe",
                     "layout":{{"text-field":"{{name}}","text-font":["TestFont"],
                                "text-size":16,"text-allow-overlap":{overlap}}},
                     "paint":{{"text-color":"#ffffff"}}}}"##
            )
        })
        .collect();
    let style_json = format!(
        r##"{{"version":8,
             "glyphs":"file://{root}/tests/glyph-fixtures/{{fontstack}}/{{range}}.pbf",
             "sources":{{"probe":{{"type":"geojson","data":{POINTS}}}}},
             "layers":[{}]}}"##,
        written.join(",")
    );
    Style::parse(&style_json).expect("the style parses")
}

/// The tiles the labels are in, each with its own buckets.
struct One {
    by_tile: BTreeMap<TileId, Arc<Vec<LayerBucket>>>,
}

impl Tiles for One {
    fn buckets(&self, tile: TileId) -> Option<Arc<Vec<LayerBucket>>> {
        self.by_tile.get(&tile).map(Arc::clone)
    }
}

fn view() -> ViewTransform {
    camera::settled(&ViewTransform {
        longitude: -0.11,
        latitude: 51.505,
        zoom: 13.0,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
        ground_below: 0.0,
    })
}

const CAPACITY: usize = 1 << 24;

/// A map over the style, ticked twice without glyphs and then `rounds` times with them.
///
/// Returned with the region it writes into, because the ring outlives neither half on its own.
fn drawn(style: Style, with_glyphs: bool) -> (Map, One, Vec<u64>) {
    let Some(tessella_style::Source::Geojson(source)) = style.source("probe") else {
        panic!("one geojson source")
    };
    let features = tessella_source::geojson::read(&source.data).expect("features read");
    let mut by_tile = BTreeMap::new();
    for tile in TILES {
        let built = build_tile(&style, "probe", tile, &features, TilingOptions::default())
            .expect("the tile builds");
        assert!(
            built
                .iter()
                .any(|bucket| matches!(bucket.content, Content::Symbol(_))),
            "every fixture tile carries a symbol layer"
        );
        by_tile.insert(tile, Arc::new(built));
    }
    let tiles = One { by_tile };

    let mut region = vec![0u64; region_size(CAPACITY).div_ceil(8)];
    // SAFETY: sized by `region_size`, eight-aligned as a `Vec<u64>`, returned alongside the map so
    // it outlives both halves.
    let (mut producer, _consumer) =
        unsafe { ring::init(region.as_mut_ptr().cast::<u8>(), CAPACITY) };
    let mut map = Map::new(style.clone(), view(), ViewId(0));
    // Twice before the glyphs: a symbol bucket is withheld until its glyphs are in, so these frames
    // draw everything that is not a label.
    for _ in 0..2 {
        let _ = map.tick(&mut producer, &tiles);
    }

    if with_glyphs {
        let mut fonts = Fonts::new(style.glyphs.clone().expect("a glyph URL"));
        let mut wanted: Dependencies = BTreeMap::new();
        for tile in TILES {
            for bucket in tiles.buckets(tile).expect("the tile").iter() {
                if let Content::Symbol(layout) = &bucket.content {
                    for (stack, codepoints) in layout.dependencies() {
                        wanted.entry(stack).or_default().extend(codepoints);
                    }
                }
            }
        }
        fonts.fetch(&wanted, &Disk).expect("the fonts read");
        map.set_fonts(fonts);
        for _ in 0..3 {
            let _ = map.tick(&mut producer, &tiles);
        }
    }
    (map, tiles, region)
}

/// Where a coordinate is drawn, in screen pixels.
fn at(coordinate: [f64; 2]) -> [f64; 2] {
    tessella_tile::screen::to_screen(&view(), coordinate[0], coordinate[1])
        .expect("the coordinate is on screen")
}

/// A tap on a label names its feature.
#[test]
fn a_tap_on_a_label_names_its_feature() {
    let (map, tiles, _region) = drawn(style_with(&[("labels", true)]), true);

    let point = at(ALPHA);
    let hits = map.query_rendered_features(&tiles, [point, point], None);

    assert!(
        !hits.is_empty(),
        "a tap on a drawn label at {point:?} found nothing: {hits:?}"
    );
    assert!(hits.iter().all(|hit| hit.layer_id == "labels"), "{hits:?}");
    assert!(
        hits.iter()
            .any(|hit| hit.properties.get("name") == Some(Value::String("Alpha".into()))),
        "the label under the tap is Alpha's: {hits:?}"
    );
    assert_eq!(hits[0].source.as_deref(), Some("probe"));
    assert_eq!(hits[0].geometry_type, "Point");
    // The id as the source gave it. Carried rather than assumed: the fixture's features had none
    // until this test needed them, and without one a mutation dropping the id read as correct.
    assert!(
        hits.iter().any(|hit| hit.id == Some(Value::Number(11.0))),
        "Alpha's id did not survive the layout: {hits:?}"
    );
}

/// The label is found where it is drawn, and not at the mirror of it.
///
/// A label collides in the matrix's own bottom-up space and is put in the grid unflipped, while
/// `to_screen` measures y from the top. A query that did not flip between them answers about the
/// point mirrored across the middle of the viewport -- correct for a label in the center and wrong
/// for every other one, which is as quiet as a bug gets.
///
/// Alpha is at screen y 571 of 768, so its mirror is y 197: far from it, and still on screen.
#[test]
fn a_label_is_not_found_at_its_mirror() {
    let (map, tiles, _region) = drawn(style_with(&[("labels", true)]), true);

    let point = at(ALPHA);
    let mirrored = [point[0], view().height - point[1]];
    assert!(
        (point[1] - mirrored[1]).abs() > 300.0,
        "the fixture's premise: {point:?} and its mirror {mirrored:?} are far apart"
    );

    assert!(
        !map.query_rendered_features(&tiles, [point, point], None)
            .is_empty(),
        "the label is not where it is drawn"
    );
    assert_eq!(
        map.query_rendered_features(&tiles, [mirrored, mirrored], None),
        [],
        "the label was found at the mirror of where it is drawn, so the y axis is inverted"
    );
}

/// A label that lost its space is not returned.
///
/// Two symbol layers over the same three features, neither allowing overlap. Placement runs the
/// topmost layer first, so `over` takes every box and `under` -- asking for exactly the same boxes --
/// is placed nowhere. It is built, it is laid out, it has a cross-tile identity and its bucket is on
/// the wire; what it does not have is a place on the screen.
///
/// So the assertion is not "fewer hits". It is that `under` appears **not at all**, which is the
/// difference between a query that reads the placement and one that reads the buckets.
#[test]
fn a_label_that_lost_its_space_is_not_returned() {
    let (map, tiles, _region) = drawn(style_with(&[("under", false), ("over", false)]), true);

    let mut answered = 0;
    for name in [ALPHA, BRAVO, CHARLIE] {
        let point = at(name);
        let hits = map.query_rendered_features(&tiles, [point, point], None);
        for hit in &hits {
            assert_ne!(
                hit.layer_id, "under",
                "a label placed nowhere was answered: {hits:?}"
            );
        }
        answered += hits.len();
    }
    assert!(
        answered > 0,
        "no label was answered at all, so this test is not measuring suppression"
    );
}

/// Before the glyphs arrive, nothing is drawn and nothing is answered.
///
/// The simplest form of the same rule. A symbol bucket is withheld until its glyphs are in -- which
/// is what keeps a half-drawn label off the screen -- so there is nothing placed and nothing to find.
#[test]
fn a_label_without_glyphs_is_not_returned() {
    let (map, tiles, _region) = drawn(style_with(&[("labels", true)]), false);

    let point = at(ALPHA);
    assert_eq!(
        map.query_rendered_features(&tiles, [point, point], None),
        [],
        "a label whose glyphs have not arrived is not drawn, so it cannot be under the finger"
    );
}

/// A tap away from every label finds nothing.
#[test]
fn a_tap_away_from_the_labels_finds_nothing() {
    let (map, tiles, _region) = drawn(style_with(&[("labels", true)]), true);

    // A corner of the viewport, far from all three points.
    let corner = [8.0, 8.0];
    assert_eq!(
        map.query_rendered_features(&tiles, [corner, corner], None),
        [],
        "a tap in an empty corner answered a label"
    );
}

/// The layer filter reaches symbols too.
#[test]
fn a_layer_filter_reaches_symbols() {
    let (map, tiles, _region) = drawn(style_with(&[("labels", true)]), true);
    let point = at(ALPHA);

    assert!(
        !map.query_rendered_features(&tiles, [point, point], Some(&["labels"]))
            .is_empty(),
        "naming the layer kept nothing"
    );
    assert_eq!(
        map.query_rendered_features(&tiles, [point, point], Some(&["absent"])),
        [],
        "naming another layer kept the symbol anyway"
    );
}

/// Two labels of one tile are told apart.
///
/// Bravo and Charlie are two pendings of one layout in one tile, so answering them means following
/// each placed symbol back to *its own* pending. A query that took the layout's first pending for
/// every symbol answers "Bravo" for both and passes every other test in this file -- the tile where
/// Alpha lives has one label, and one is its own first.
#[test]
fn two_labels_of_one_tile_are_told_apart() {
    let (map, tiles, _region) = drawn(style_with(&[("labels", true)]), true);

    for (expected, coordinate) in [("Bravo", BRAVO), ("Charlie", CHARLIE)] {
        let point = at(coordinate);
        let hits = map.query_rendered_features(&tiles, [point, point], None);
        let names: Vec<Option<Value>> = hits.iter().map(|hit| hit.properties.get("name")).collect();
        assert!(
            names.contains(&Some(Value::String(expected.into()))),
            "a tap on {expected} at {point:?} answered {names:?}"
        );
    }
}
