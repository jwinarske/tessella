//! What a host asks the map: which features are drawn under this point.
//!
//! # What this is for
//!
//! The entry point of a rendered-feature query (#338). #355 and #356 recorded what each feature left
//! in a bucket and #357 decided which records a region of one tile touches; this walks the cover,
//! turns a screen rectangle into each tile's own region, and answers in draw order.
//!
//! # What would be caught
//!
//! A query that answers about the wrong place. The screen-to-tile step is three transforms deep --
//! unproject to a coordinate, project into tile units, offset by the world copy -- and getting any
//! of them wrong gives an answer that is well formed, plausible and about somewhere else. So the
//! points here are not written as tile units: they are produced by projecting a known feature's own
//! coordinate *to* the screen, so the test asks about the pixel the feature is actually drawn on.

use std::sync::Arc;

use tessella_capture_abi::envelope::ViewId;
use tessella_capture_abi::ring::{self, region_size};
use tessella_orchestrate::map::{Map, Tiles};
use tessella_orchestrate::tile::{LayerBucket, TileId, build_tile};
use tessella_source::tiling::TilingOptions;
use tessella_style::{Style, Value};
use tessella_tile::camera;
use tessella_tile::cover::ViewTransform;

/// Two squares a degree apart near Berlin, and a point in the first of them.
///
/// A degree rather than a hundredth: at the z6 camera below a hundredth of a degree is under a tile
/// unit and rounds away, and the test would measure the rounding.
const DATA: &str = r##"{"type":"FeatureCollection","features":[
  {"type":"Feature","id":1,"properties":{"kind":"park","name":"Tiergarten"},
   "geometry":{"type":"Polygon","coordinates":[
     [[12.0,52.0],[13.0,52.0],[13.0,53.0],[12.0,53.0],[12.0,52.0]]]}},
  {"type":"Feature","id":2,"properties":{"kind":"wood","name":"Grunewald"},
   "geometry":{"type":"Polygon","coordinates":[
     [[15.0,52.0],[16.0,52.0],[16.0,53.0],[15.0,53.0],[15.0,52.0]]]}}]}"##;

/// Inside the first square, and inside the second.
const IN_FIRST: [f64; 2] = [12.5, 52.5];
const IN_SECOND: [f64; 2] = [15.5, 52.5];
/// Between them, inside neither.
const BETWEEN: [f64; 2] = [14.0, 52.5];

/// A store holding each tile's own buckets.
///
/// Per tile rather than one set answered for every coordinate, which is not a tidiness point: a
/// bucket's geometry is in *its* tile's local frame, and the query builds each region in the frame
/// of the tile serving that coordinate. A store answering tile (6,35,21) with (6,34,21)'s buckets
/// puts the geometry a whole tile away from the region, so every coordinate but one silently
/// misses -- and a test over such a store passes while measuring one tile.
struct Everywhere {
    by_tile: std::collections::BTreeMap<TileId, Arc<Vec<LayerBucket>>>,
}

impl Tiles for Everywhere {
    fn buckets(&self, tile: TileId) -> Option<Arc<Vec<LayerBucket>>> {
        self.by_tile.get(&tile).map(Arc::clone)
    }
}

/// Buckets for every tile of `zoom` that the fixture could fall in, built in its own frame.
fn store(style: &Style, zoom: u8) -> Everywhere {
    let tessella_style::Source::Geojson(source) = style.source("s").expect("a source") else {
        panic!("the fixture has one geojson source")
    };
    let features = tessella_source::geojson::read(&source.data).expect("features read");
    let span = 1u32 << zoom;
    let mut by_tile = std::collections::BTreeMap::new();
    // A block around the fixture rather than the whole level: at z6 that is 4096 tiles and the
    // fixture is in four of them.
    let (cx, cy) = (
        (span as f64 * (13.5 + 180.0) / 360.0) as u32,
        (span as f64 * 0.3) as u32,
    );
    for x in cx.saturating_sub(3)..=(cx + 3).min(span - 1) {
        for y in cy.saturating_sub(3)..=(cy + 3).min(span - 1) {
            let tile = TileId::new(zoom, x, y);
            let built = build_tile(style, "s", tile, &features, TilingOptions::default())
                .expect("the tile builds");
            by_tile.insert(tile, Arc::new(built));
        }
    }
    Everywhere { by_tile }
}

fn view(zoom: f64, pitch: f64) -> ViewTransform {
    camera::settled(&ViewTransform {
        longitude: 13.5,
        latitude: 52.5,
        zoom,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch,
        ground_below: 0.0,
    })
}

/// A style over the fixture, with the layers given as `(id, kind, paint)`.
fn style_with(layers: &[(&str, &str, &str)]) -> Style {
    let written: Vec<String> = layers
        .iter()
        .map(|(id, kind, paint)| {
            format!(r##"{{"id":"{id}","type":"{kind}","source":"s","paint":{paint}}}"##)
        })
        .collect();
    let style_json = format!(
        r##"{{"version":8,"sources":{{"s":{{"type":"geojson","data":{DATA}}}}},
             "layers":[{}]}}"##,
        written.join(",")
    );
    Style::parse(&style_json).expect("the style parses")
}

/// A map ticked until it has drawn, and the store behind it.
fn ticked(style: Style, view: ViewTransform) -> (Map, Everywhere, Vec<u64>) {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let zoom = view.zoom as u8;
    let tiles = store(&style, zoom);

    const CAPACITY: usize = 1 << 22;
    let mut region = vec![0u64; region_size(CAPACITY).div_ceil(8)];
    // SAFETY: sized by `region_size`, eight-aligned as a `Vec<u64>`, and the region outlives both
    // halves because it is returned alongside the map.
    let (mut producer, _consumer) =
        unsafe { ring::init(region.as_mut_ptr().cast::<u8>(), CAPACITY) };
    let mut map = Map::new(style, view, ViewId(0));
    for _ in 0..4 {
        let _ = map.tick(&mut producer, &tiles);
    }
    (map, tiles, region)
}

/// Where a coordinate is drawn, in screen pixels.
///
/// The round trip is the point: the query unprojects a pixel and this projects a coordinate, so a
/// test that asks about `to_screen(IN_FIRST)` is asking about the pixel the first square is drawn on
/// whatever the camera is.
fn at(map_view: &ViewTransform, coordinate: [f64; 2]) -> [f64; 2] {
    tessella_tile::screen::to_screen(map_view, coordinate[0], coordinate[1])
        .expect("the coordinate is on screen")
}

/// A tap over a polygon names its layer, its source and the feature.
#[test]
fn a_tap_names_the_feature_under_it() {
    let style = style_with(&[("fills", "fill", r##"{"fill-color":"#ff0000"}"##)]);
    let camera = view(6.0, 0.0);
    let (map, tiles, _region) = ticked(style, camera);

    let point = at(&camera, IN_FIRST);
    let hits = map.query_rendered_features(&tiles, [point, point], None);

    assert_eq!(hits.len(), 1, "one layer, one square: {hits:?}");
    let hit = &hits[0];
    assert_eq!(hit.layer_id, "fills");
    assert_eq!(hit.source.as_deref(), Some("s"));
    assert_eq!(hit.source_layer, None, "a GeoJSON source has no sub-layer");
    assert_eq!(hit.id, Some(Value::Number(1.0)));
    assert_eq!(hit.geometry_type, "Polygon");
    assert_eq!(
        hit.properties.get("name"),
        Some(Value::String("Tiergarten".into()))
    );
}

/// The second square answers as itself, not as the first.
///
/// The control that matters. A query that answered "the first record of the bucket" whatever it was
/// asked passes the test above and nothing else.
#[test]
fn a_tap_distinguishes_the_two_squares() {
    let style = style_with(&[("fills", "fill", r##"{"fill-color":"#ff0000"}"##)]);
    let camera = view(6.0, 0.0);
    let (map, tiles, _region) = ticked(style, camera);

    let second = at(&camera, IN_SECOND);
    let hits = map.query_rendered_features(&tiles, [second, second], None);
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].id, Some(Value::Number(2.0)));
    assert_eq!(
        hits[0].properties.get("name"),
        Some(Value::String("Grunewald".into()))
    );
}

/// A tap between them finds nothing.
#[test]
fn a_tap_between_the_squares_finds_nothing() {
    let style = style_with(&[("fills", "fill", r##"{"fill-color":"#ff0000"}"##)]);
    let camera = view(6.0, 0.0);
    let (map, tiles, _region) = ticked(style, camera);

    let gap = at(&camera, BETWEEN);
    assert_eq!(
        map.query_rendered_features(&tiles, [gap, gap], None),
        [],
        "the gap between the squares belongs to nothing"
    );
}

/// A box over both squares finds both.
#[test]
fn a_box_over_both_finds_both() {
    let style = style_with(&[("fills", "fill", r##"{"fill-color":"#ff0000"}"##)]);
    let camera = view(6.0, 0.0);
    let (map, tiles, _region) = ticked(style, camera);

    let first = at(&camera, IN_FIRST);
    let second = at(&camera, IN_SECOND);
    let hits = map.query_rendered_features(&tiles, [first, second], None);

    let mut ids: Vec<Option<Value>> = hits.iter().map(|hit| hit.id.clone()).collect();
    ids.sort_by_key(|id| format!("{id:?}"));
    assert_eq!(
        ids,
        [Some(Value::Number(1.0)), Some(Value::Number(2.0))],
        "a box spanning both squares finds both: {hits:?}"
    );
}

/// Two layers over one feature answer topmost first.
///
/// The order is what a host acts on: it takes the first entry. Paint order puts the later layer on
/// top, so the answer is the reverse of the style's own list.
#[test]
fn the_answer_is_topmost_first() {
    let style = style_with(&[
        ("under", "fill", r##"{"fill-color":"#ff0000"}"##),
        ("over", "fill", r##"{"fill-color":"#00ff00"}"##),
    ]);
    let camera = view(6.0, 0.0);
    let (map, tiles, _region) = ticked(style, camera);

    let point = at(&camera, IN_FIRST);
    let hits = map.query_rendered_features(&tiles, [point, point], None);
    let layers: Vec<&str> = hits.iter().map(|hit| hit.layer_id.as_str()).collect();
    assert_eq!(
        layers,
        ["over", "under"],
        "the layer drawn last is answered first: {hits:?}"
    );
}

/// A layer filter keeps only the layers named.
#[test]
fn a_layer_filter_keeps_only_what_it_names() {
    let style = style_with(&[
        ("under", "fill", r##"{"fill-color":"#ff0000"}"##),
        ("over", "fill", r##"{"fill-color":"#00ff00"}"##),
    ]);
    let camera = view(6.0, 0.0);
    let (map, tiles, _region) = ticked(style, camera);
    let point = at(&camera, IN_FIRST);

    let only_under = map.query_rendered_features(&tiles, [point, point], Some(&["under"]));
    let layers: Vec<&str> = only_under.iter().map(|hit| hit.layer_id.as_str()).collect();
    assert_eq!(layers, ["under"], "{only_under:?}");

    let neither = map.query_rendered_features(&tiles, [point, point], Some(&["absent"]));
    assert_eq!(
        neither,
        [],
        "a filter naming no layer of the style answers nothing"
    );

    let both = map.query_rendered_features(&tiles, [point, point], Some(&["over", "under"]));
    assert_eq!(both.len(), 2, "{both:?}");
}

/// A pitched camera answers about the ground, not about the middle of the screen.
///
/// Under pitch a screen pixel's distance from the center is no longer proportional to its distance
/// on the ground, so this is the case an unprojection that quietly assumed an orthographic camera
/// would get wrong -- and get wrong *plausibly*, by naming the neighboring feature.
///
/// There is no companion test for a tap above the horizon. `from_screen_detail` reports
/// `met_the_plane: false` for a ray that does not descend, and the query refuses on it, but no camera
/// this crate can build reaches that: measured at zoom 6 over pitches from 45 to **89.2** degrees --
/// `MAX_PITCH` is 89.25 -- the top row of pixels still descends to the plane every time. The guard
/// stays because answering with the near point would be a hit invented out of sky, but it guards a
/// projection that is not ours, and a test asserting it would be asserting its own fixture.
#[test]
fn a_pitched_camera_answers_about_the_ground() {
    let style = style_with(&[("fills", "fill", r##"{"fill-color":"#ff0000"}"##)]);
    let camera = view(6.0, 60.0);
    let (map, tiles, _region) = ticked(style, camera);

    let first = at(&camera, IN_FIRST);
    let hits = map.query_rendered_features(&tiles, [first, first], None);
    assert_eq!(hits.len(), 1, "a pitched camera answered nothing: {hits:?}");
    assert_eq!(hits[0].id, Some(Value::Number(1.0)));

    // The second square and the gap, under the same pitched camera: the answer tracks the ground
    // rather than the screen.
    let second = at(&camera, IN_SECOND);
    let hits = map.query_rendered_features(&tiles, [second, second], None);
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].id, Some(Value::Number(2.0)));

    let gap = at(&camera, BETWEEN);
    assert_eq!(
        map.query_rendered_features(&tiles, [gap, gap], None),
        [],
        "the gap between the squares answered something under pitch"
    );

    // And the three pixels really are different ones, which is what says the test is not asking the
    // same question three times.
    assert!(
        (first[0] - second[0]).abs() > 50.0 && (first[0] - gap[0]).abs() > 10.0,
        "the fixture's premise: the three taps are far apart on screen -- {first:?} {second:?} {gap:?}"
    );
}

/// A map that has drawn nothing answers nothing.
///
/// Not an edge case: a host wires up a tap handler before the first tile lands, and "rendered" has
/// to mean rendered. An empty answer is the truth there.
#[test]
fn a_map_that_has_not_drawn_answers_nothing() {
    let style = style_with(&[("fills", "fill", r##"{"fill-color":"#ff0000"}"##)]);
    let camera = view(6.0, 0.0);
    let tiles = store(&style, 6);
    // Never ticked, so the cover is empty.
    let map = Map::new(style, camera, ViewId(0));

    let point = at(&camera, IN_FIRST);
    assert_eq!(
        map.query_rendered_features(&tiles, [point, point], None),
        [],
        "a map with no cover answered about tiles it has not drawn"
    );
}

/// A feature in a second copy of the world answers as itself.
///
/// At zoom 0 a 1024-pixel viewport is two worlds wide, so the cover carries the same tile at
/// `wrap` -1, 0 and 1, and the same feature is drawn in each. A query has to take the copy off the
/// longitude before projecting into the tile's frame, because the tile is one tile and the copies
/// differ only in the matrix that draws them.
///
/// Measured by asking about the *east* copy: `project` leaves longitude alone, so the screen
/// position of `longitude + 360` is where the east copy is drawn. Without the wrap subtraction that
/// coordinate lands a whole world outside the tile and the query answers nothing.
#[test]
fn a_feature_in_another_world_copy_is_found() {
    let style = style_with(&[("fills", "fill", r##"{"fill-color":"#ff0000"}"##)]);
    let camera = view(0.0, 0.0);
    let (map, tiles, _region) = ticked(style, camera);

    // The premise: this camera really does draw more than one copy of the world.
    let home = at(&camera, IN_FIRST);
    let east = at(&camera, [IN_FIRST[0] + 360.0, IN_FIRST[1]]);
    assert!(
        east[0] > home[0] && east[0] < camera.width,
        "the fixture's premise: the east copy of {IN_FIRST:?} is on screen at {east:?}, \
         right of {home:?} and inside {}",
        camera.width
    );

    for (what, point) in [("the home copy", home), ("the east copy", east)] {
        let hits = map.query_rendered_features(&tiles, [point, point], None);
        assert_eq!(
            hits.len(),
            1,
            "{what} at {point:?} answered {} hits",
            hits.len()
        );
        assert_eq!(hits[0].id, Some(Value::Number(1.0)), "{what}");
    }
}

/// A line's reach is converted from screen pixels into the tile's units.
///
/// The paint is in pixels and the region is in tile units, and at this camera the two differ by a
/// factor of sixteen -- so a query that skipped the conversion would answer as though a 24-pixel
/// casing were a pixel and a half wide. Asked a few pixels off the centerline, which is inside the
/// line and far outside an unconverted reading of it.
#[test]
fn a_lines_reach_is_converted_into_tile_units() {
    const ROAD: &str = r##"{"type":"FeatureCollection","features":[
      {"type":"Feature","id":7,"properties":{"kind":"road"},
       "geometry":{"type":"LineString","coordinates":[[12.0,52.5],[16.0,52.5]]}}]}"##;
    let style_json = format!(
        r##"{{"version":8,"sources":{{"s":{{"type":"geojson","data":{ROAD}}}}},
             "layers":[{{"id":"roads","type":"line","source":"s",
                         "paint":{{"line-color":"#ff0000","line-width":24.0}}}}]}}"##
    );
    let style = Style::parse(&style_json).expect("the style parses");
    let camera = view(6.0, 0.0);
    let (map, tiles, _region) = ticked(style, camera);

    let on_it = at(&camera, [13.5, 52.5]);
    let nearby = [on_it[0], on_it[1] + 8.0];
    let far = [on_it[0], on_it[1] + 200.0];

    assert_eq!(
        map.query_rendered_features(&tiles, [on_it, on_it], None)
            .len(),
        1,
        "the centerline itself is not a hit"
    );
    let hits = map.query_rendered_features(&tiles, [nearby, nearby], None);
    assert_eq!(
        hits.len(),
        1,
        "eight pixels off a 24-pixel line is inside it: {hits:?}"
    );
    assert_eq!(hits[0].id, Some(Value::Number(7.0)));
    assert_eq!(
        map.query_rendered_features(&tiles, [far, far], None),
        [],
        "two hundred pixels off it is not, so the reach is a reach and not everything"
    );
}

/// One feature drawn in two tiles is one answer.
///
/// A feature crossing a tile seam is built into both tiles, each holding its own clipped half, so a
/// box over the seam touches two records of one feature. A host offered the same thing twice has to
/// dedupe it, which it cannot do better than this can.
///
/// The premise is asserted rather than assumed. The first version of this test used a store that
/// answered every coordinate with one tile's buckets, so only one tile ever matched and removing the
/// dedupe changed nothing -- it passed while measuring the opposite of its name.
#[test]
fn a_feature_across_a_seam_is_answered_once() {
    // The z6 seam between x=34 and x=35 is at longitude 16.875, so this square straddles it.
    const STRADDLING: &str = r##"{"type":"FeatureCollection","features":[
      {"type":"Feature","id":9,"properties":{"kind":"straddle"},
       "geometry":{"type":"Polygon","coordinates":[
         [[16.0,52.0],[18.0,52.0],[18.0,53.0],[16.0,53.0],[16.0,52.0]]]}}]}"##;
    let style_json = format!(
        r##"{{"version":8,"sources":{{"s":{{"type":"geojson","data":{STRADDLING}}}}},
             "layers":[{{"id":"fills","type":"fill","source":"s",
                         "paint":{{"fill-color":"#ff0000"}}}}]}}"##
    );
    let style = Style::parse(&style_json).expect("the style parses");
    let camera = view(6.0, 0.0);
    let (map, tiles, _region) = ticked(style, camera);

    // The premise: both tiles either side of the seam carry a record for feature 9.
    for x in [34, 35] {
        let tile = TileId::new(6, x, 21);
        let built = tiles.buckets(tile).expect("the store holds it");
        let records: usize = built.iter().map(|bucket| bucket.features.len()).sum();
        assert!(
            records > 0,
            "tile (6,{x},21) holds no record of the straddling square, so this test \
             cannot measure a duplicate"
        );
    }

    // A box across the seam, from inside the western half to inside the eastern one.
    let west = at(&camera, [16.3, 52.5]);
    let east = at(&camera, [17.5, 52.5]);
    let hits = map.query_rendered_features(&tiles, [west, east], None);
    assert_eq!(
        hits.len(),
        1,
        "the same feature of the same layer was answered more than once: {hits:?}"
    );
    assert_eq!(hits[0].id, Some(Value::Number(9.0)));
}
