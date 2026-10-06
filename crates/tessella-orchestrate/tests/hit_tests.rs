//! Which recorded features a region of a tile touches.
//!
//! # What this is for
//!
//! The half of a rendered-feature query (#338) that decides what was hit. The records from #355 say
//! which vertices belong to which feature; this says which of those features a tap or a box lands
//! on, and the answer has to put the paint back, because a bucket's vertices are not what was drawn.
//!
//! # What would be caught
//!
//! Three things, in order of how badly they would read:
//!
//! - a tap that reports the road beside the one under the finger. Nothing about the map looks wrong,
//!   and the host acts on it;
//! - a tap near a hairline road reporting a hit, or one on a thick casing reporting a miss, because
//!   the width was not put back. The vertices are the centerline whatever `line-width` says;
//! - a tap in open water reporting a road, because a feature clipped into two pieces was treated as
//!   joined. That phantom segment can cross the whole tile.

use std::collections::BTreeMap;

use tessella_orchestrate::hit::{Region, touched};
use tessella_orchestrate::tile::{LayerBucket, TileId, build_tile};
use tessella_source::tiling::{EXTENT, TilingOptions};
use tessella_style::Style;

/// Tile-space x at z0 for a longitude, which is `EXTENT` units across the world.
fn x_at(longitude: f64) -> f64 {
    (longitude + 180.0) / 360.0 * f64::from(EXTENT)
}

/// Tile-space y at z0 for the equator, which is the middle of the tile.
fn y_at_equator() -> f64 {
    f64::from(EXTENT) / 2.0
}

/// One layer's buckets over `data`, at z0.
fn built(kind: &str, paint: &str, data: &str) -> Vec<LayerBucket> {
    let style_json = format!(
        r##"{{"version":8,"sources":{{"s":{{"type":"geojson","data":{data}}}}},
             "layers":[{{"id":"L","type":"{kind}","source":"s","paint":{paint}}}]}}"##
    );
    let style = Style::parse(&style_json).expect("the style parses");
    let tessella_style::Source::Geojson(source) = style.source("s").expect("a source") else {
        panic!("the fixture has one geojson source")
    };
    let features = tessella_source::geojson::read(&source.data).expect("features read");
    build_tile(
        &style,
        "s",
        TileId::new(0, 0, 0),
        &features,
        TilingOptions::default(),
    )
    .expect("the tile builds")
}

/// `touched` over a bucket, with one tile unit to a pixel so the arithmetic is readable.
fn hits(bucket: &LayerBucket, region: &Region) -> Vec<usize> {
    touched(
        &bucket.content,
        &bucket.features,
        &bucket.paint,
        0.0,
        1.0,
        region,
    )
}

/// Two squares a third of a world apart.
const SQUARES: &str = r##"{"type":"FeatureCollection","features":[
  {"type":"Feature","id":1,"properties":{"kind":"park"},
   "geometry":{"type":"Polygon","coordinates":[[[-60.0,-10.0],[-40.0,-10.0],[-40.0,10.0],[-60.0,10.0],[-60.0,-10.0]]]}},
  {"type":"Feature","id":2,"properties":{"kind":"wood"},
   "geometry":{"type":"Polygon","coordinates":[[[40.0,-10.0],[60.0,-10.0],[60.0,10.0],[40.0,10.0],[40.0,-10.0]]]}}]}"##;

/// A tap inside a polygon finds it, and only it.
#[test]
fn a_tap_inside_a_polygon_finds_it() {
    let buckets = built("fill", r##"{"fill-color":"#ff0000"}"##, SQUARES);
    let bucket = buckets.first().expect("one fill bucket");
    assert_eq!(bucket.features.len(), 2, "both squares recorded");

    assert_eq!(
        hits(bucket, &Region::at([x_at(-50.0), y_at_equator()])),
        [0],
        "a tap in the first square is the first record and not the second"
    );
    assert_eq!(
        hits(bucket, &Region::at([x_at(50.0), y_at_equator()])),
        [1],
        "and a tap in the second is the second"
    );
}

/// A tap between two polygons finds neither.
///
/// The control for the test above. Without it, a hit test that answers "every record" passes the
/// first assertion of each pair and is wrong about everything.
#[test]
fn a_tap_between_two_polygons_finds_neither() {
    let buckets = built("fill", r##"{"fill-color":"#ff0000"}"##, SQUARES);
    let bucket = buckets.first().expect("one fill bucket");

    assert_eq!(
        hits(bucket, &Region::at([x_at(0.0), y_at_equator()])),
        [] as [usize; 0],
        "the gap between them belongs to nothing"
    );
    assert_eq!(
        hits(bucket, &Region::at([x_at(-50.0), y_at_equator() - 2000.0])),
        [] as [usize; 0],
        "and so does the space above the first"
    );
}

/// A box spanning both polygons finds both, in record order.
#[test]
fn a_box_over_both_polygons_finds_both() {
    let buckets = built("fill", r##"{"fill-color":"#ff0000"}"##, SQUARES);
    let bucket = buckets.first().expect("one fill bucket");

    let wide = Region::quad([
        [x_at(-70.0), y_at_equator() - 100.0],
        [x_at(70.0), y_at_equator() - 100.0],
        [x_at(70.0), y_at_equator() + 100.0],
        [x_at(-70.0), y_at_equator() + 100.0],
    ]);
    assert_eq!(hits(bucket, &wide), [0, 1], "both, in record order");

    // A box in the gap still finds neither, which says the box test is a box test rather than a
    // bounding-box one over the whole bucket.
    let narrow = Region::quad([
        [x_at(-10.0), y_at_equator() - 100.0],
        [x_at(10.0), y_at_equator() - 100.0],
        [x_at(10.0), y_at_equator() + 100.0],
        [x_at(-10.0), y_at_equator() + 100.0],
    ]);
    assert_eq!(hits(bucket, &narrow), [] as [usize; 0]);
}

/// One straight line along the equator.
const LINE: &str = r##"{"type":"FeatureCollection","features":[
  {"type":"Feature","id":1,"properties":{"kind":"road"},
   "geometry":{"type":"LineString","coordinates":[[-60.0,0.0],[60.0,0.0]]}}]}"##;

/// A line is hit out to half its width and no further.
///
/// The test that says the paint is put back. The vertices are the centerline doubled -- `line.rs`
/// writes `p * 2 | flag` and the extrusion arrives in the vertex shader -- so a hit test reading the
/// buffer alone answers the same for a hairline and for a casing twenty pixels wide.
///
/// Measured at a fixed offset of eight units with the width varied, rather than the other way round:
/// one geometry, one region, and the only thing differing is the number the style wrote.
#[test]
fn a_line_is_hit_out_to_half_its_width() {
    let eight_below = [x_at(0.0), y_at_equator() + 8.0];

    let hairline = built(
        "line",
        r##"{"line-color":"#ff0000","line-width":2.0}"##,
        LINE,
    );
    assert_eq!(
        hits(
            hairline.first().expect("one line bucket"),
            &Region::at(eight_below)
        ),
        [] as [usize; 0],
        "a 2px line reaches one unit from its centerline, not eight"
    );

    let casing = built(
        "line",
        r##"{"line-color":"#ff0000","line-width":20.0}"##,
        LINE,
    );
    assert_eq!(
        hits(
            casing.first().expect("one line bucket"),
            &Region::at(eight_below)
        ),
        [0],
        "a 20px line reaches ten, so the same tap lands on it"
    );
}

/// A gap splits a line, so its outer edge is the gap's half plus the whole width.
///
/// mbgl's `getLineWidth`, which is not `width / 2` when `line-gap-width` is set. A reading that took
/// half the width regardless would answer eighteen units here instead of thirty.
#[test]
fn a_line_gap_widens_the_reach() {
    let at = |offset: f64| Region::at([x_at(0.0), y_at_equator() + offset]);
    let bucket = built(
        "line",
        r##"{"line-color":"#ff0000","line-width":20.0,"line-gap-width":20.0}"##,
        LINE,
    );
    let bucket = bucket.first().expect("one line bucket");

    assert_eq!(hits(bucket, &at(25.0)), [0], "inside 10 + 20");
    assert_eq!(hits(bucket, &at(35.0)), [] as [usize; 0], "and outside it");
}

/// A data-driven width is read per feature.
///
/// Two parallel lines, one wide and one narrow, from one expression over a property. A hit test that
/// evaluated the layer's width once for the bucket would answer the same for both.
#[test]
fn a_data_driven_width_is_read_per_feature() {
    const PAIR: &str = r##"{"type":"FeatureCollection","features":[
      {"type":"Feature","id":1,"properties":{"grade":"casing"},
       "geometry":{"type":"LineString","coordinates":[[-60.0,20.0],[60.0,20.0]]}},
      {"type":"Feature","id":2,"properties":{"grade":"hairline"},
       "geometry":{"type":"LineString","coordinates":[[-60.0,-20.0],[60.0,-20.0]]}}]}"##;
    let buckets = built(
        "line",
        r##"{"line-color":"#ff0000",
             "line-width":["match",["get","grade"],"casing",40.0,2.0]}"##,
        PAIR,
    );
    let bucket = buckets.first().expect("one line bucket");
    assert_eq!(bucket.features.len(), 2, "both lines recorded");

    // Eight units off each centerline. The wide one reaches twenty, the narrow one reaches one.
    let wide_y = bucket.features[0].vertices.clone().map(|_| ()).count();
    let _ = wide_y;
    let first = centerline_y(bucket, 0);
    let second = centerline_y(bucket, 1);

    assert_eq!(
        hits(bucket, &Region::at([x_at(0.0), first + 8.0])),
        [0],
        "eight units off the casing is inside it"
    );
    assert_eq!(
        hits(bucket, &Region::at([x_at(0.0), second + 8.0])),
        [] as [usize; 0],
        "eight units off the hairline is not"
    );
}

/// Every centerline x of a line record, in emission order.
///
/// Read back rather than computed, because what this pins is where the pieces are *in the buffer* --
/// which is what a join would be invented across.
fn centerline_xs(bucket: &LayerBucket, record: usize) -> Vec<f64> {
    let tessella_orchestrate::tile::Content::Line(line) = &bucket.content else {
        panic!("a line bucket")
    };
    bucket.features[record]
        .vertices
        .clone()
        .step_by(2)
        .map(|at| f64::from(line.vertices[at as usize].pos_normal[0] >> 1))
        .collect()
}

/// The y of a line record's first centerline point, read back out of the bucket.
///
/// Read rather than computed from the latitude: the projection is Mercator and the point of this
/// test is the width, not the projection.
fn centerline_y(bucket: &LayerBucket, record: usize) -> f64 {
    let tessella_orchestrate::tile::Content::Line(line) = &bucket.content else {
        panic!("a line bucket")
    };
    let at = bucket.features[record].vertices.start as usize;
    f64::from(line.vertices[at].pos_normal[1] >> 1)
}

/// A feature in two pieces is not joined across the gap between them.
///
/// The reason a line's shape is read out of the index buffer rather than off consecutive vertices.
/// This is one feature whose two parts are a third of a world apart; connecting its centerline
/// points in emission order invents a segment straight through the middle, and a tap there would
/// report a road in open water.
///
/// Asserted both ways, because the half that fails is not the obvious one: a hit test that answers
/// nothing anywhere also has no phantom join.
#[test]
fn a_feature_in_two_pieces_is_not_joined_across_the_gap() {
    const SPLIT: &str = r##"{"type":"FeatureCollection","features":[
      {"type":"Feature","id":1,"properties":{"kind":"road"},
       "geometry":{"type":"MultiLineString","coordinates":[
         [[-100.0,0.0],[-90.0,0.0]],
         [[90.0,0.0],[100.0,0.0]]]}}]}"##;
    let buckets = built(
        "line",
        r##"{"line-color":"#ff0000","line-width":4.0}"##,
        SPLIT,
    );
    let bucket = buckets.first().expect("one line bucket");

    // Three records, which is the world-copy walk: the feature is offered to this tile once per
    // copy, and only the middle one has both pieces on the tile -- the eastern copy carries the
    // western piece at x 10012 and the western copy the eastern piece at x -2048, both off-tile.
    // So the record under test is the middle one, and it is the one holding the gap.
    assert_eq!(bucket.features.len(), 3, "one record per world copy");
    let center_copy = 1;
    let spans = centerline_xs(bucket, center_copy);
    assert_eq!(
        spans,
        [1820.0, 2048.0, 6144.0, 6372.0],
        "the middle copy holds both pieces, with 4096 units of nothing between them"
    );

    let on_a_piece = Region::at([x_at(-95.0), y_at_equator()]);
    assert_eq!(
        hits(bucket, &on_a_piece),
        [center_copy],
        "a tap on one of the pieces finds the feature -- the other half of this test"
    );

    let in_the_gap = Region::at([x_at(0.0), y_at_equator()]);
    assert_eq!(
        hits(bucket, &in_the_gap),
        [] as [usize; 0],
        "and a tap between the pieces finds nothing, where a join would put a road"
    );
}

/// A circle is hit out to its radius plus its stroke.
#[test]
fn a_circle_is_hit_out_to_its_radius() {
    const POINT: &str = r##"{"type":"FeatureCollection","features":[
      {"type":"Feature","id":1,"properties":{"kind":"poi"},
       "geometry":{"type":"Point","coordinates":[0.0,0.0]}}]}"##;

    let small = built(
        "circle",
        r##"{"circle-color":"#ff0000","circle-radius":3.0}"##,
        POINT,
    );
    let small = small.first().expect("one circle bucket");
    assert_eq!(small.features.len(), 1);
    let center = [x_at(0.0), y_at_equator()];
    let ten_away = [center[0] + 10.0, center[1]];

    assert_eq!(hits(small, &Region::at(center)), [0], "the center is a hit");
    assert_eq!(
        hits(small, &Region::at(ten_away)),
        [] as [usize; 0],
        "ten units from a 3-unit radius is not"
    );

    let big = built(
        "circle",
        r##"{"circle-color":"#ff0000","circle-radius":8.0,"circle-stroke-width":4.0}"##,
        POINT,
    );
    assert_eq!(
        hits(
            big.first().expect("one circle bucket"),
            &Region::at(ten_away)
        ),
        [0],
        "and a radius of 8 with a 4-unit stroke reaches twelve"
    );
}

/// The pixel-to-unit scale multiplies the reach.
///
/// A query happens in tile units and a paint property is in screen pixels, so the caller's scale is
/// what relates them -- mbgl's `pixelsToTileUnits`. Without it a `circle-radius` of 5 would mean
/// five tile units, which at z0 is a thousandth of the world and at z16 is most of a tile.
#[test]
fn the_pixel_scale_multiplies_the_reach() {
    const POINT: &str = r##"{"type":"FeatureCollection","features":[
      {"type":"Feature","id":1,"properties":{},
       "geometry":{"type":"Point","coordinates":[0.0,0.0]}}]}"##;
    let buckets = built(
        "circle",
        r##"{"circle-color":"#ff0000","circle-radius":4.0}"##,
        POINT,
    );
    let bucket = buckets.first().expect("one circle bucket");
    let twelve_away = Region::at([x_at(0.0) + 12.0, y_at_equator()]);

    let at = |units_per_pixel: f64| {
        touched(
            &bucket.content,
            &bucket.features,
            &bucket.paint,
            0.0,
            units_per_pixel,
            &twelve_away,
        )
    };
    assert_eq!(
        at(1.0),
        [] as [usize; 0],
        "four units does not reach twelve"
    );
    assert_eq!(at(4.0), [0], "four pixels of four units each does");
}

/// The pixel scale multiplies a line's reach too.
///
/// Separate from the circle's. A mutation that dropped `units_per_pixel` from the line arm alone
/// left every test green, because the only scale assertion was over a circle -- two arms, two
/// conversions, and one of them was unguarded.
#[test]
fn the_pixel_scale_multiplies_a_lines_reach() {
    let buckets = built(
        "line",
        r##"{"line-color":"#ff0000","line-width":4.0}"##,
        LINE,
    );
    let bucket = buckets.first().expect("one line bucket");
    let six_below = Region::at([x_at(0.0), y_at_equator() + 6.0]);

    let at = |units_per_pixel: f64| {
        touched(
            &bucket.content,
            &bucket.features,
            &bucket.paint,
            0.0,
            units_per_pixel,
            &six_below,
        )
    };
    assert_eq!(
        at(1.0),
        [] as [usize; 0],
        "a 4px line is two units either side, which does not reach six"
    );
    assert_eq!(at(4.0), [0], "at four units to a pixel it reaches eight");
}

/// A family that draws from no features answers nothing.
#[test]
fn a_background_is_never_hit() {
    let style_json = r##"{"version":8,"sources":{},
      "layers":[{"id":"B","type":"background","paint":{"background-color":"#ff0000"}}]}"##;
    let style = Style::parse(style_json).expect("the style parses");
    let buckets = tessella_orchestrate::tile::build_sourceless(&style, TileId::new(0, 0, 0))
        .expect("the tile builds");
    let bucket = buckets.first().expect("one background bucket");

    assert_eq!(
        touched(
            &bucket.content,
            &bucket.features,
            &BTreeMap::new(),
            0.0,
            1.0,
            &Region::at([100.0, 100.0]),
        ),
        [] as [usize; 0],
        "a background covers the viewport and is not a feature"
    );
}
