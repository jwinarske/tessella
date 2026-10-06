//! Every bucket records what each feature left in it.
//!
//! # What this is for
//!
//! A rendered-feature query (#338) has to say which features are drawn at a point. Nothing in the
//! tree could answer that: the build job drops the decoded tile and the response body when it
//! returns, so the only thing left to read is the buckets, and a bucket's vertices carry no feature
//! boundary at all -- a `FillBucket` is one flat `Vec<Position>`.
//!
//! `PaintBinder`'s index from #353 is not that record, which is the premise this file was written
//! against. It is gated on the paint reading `feature-state`, and a layer whose paint is wholly
//! uniform has no per-vertex buffer to hang a range off in the first place. The first test here is
//! that row.
//!
//! # What would be caught
//!
//! A query that reports the wrong feature, which is worse than one that reports none: a host taps
//! a park and gets the road beside it, and nothing about the map looks wrong. So the central test
//! is not that records exist but that each record's vertex range really holds *that* feature's
//! geometry, checked against the geometry's own bounds rather than against another record.

use std::sync::Arc;

use tessella_orchestrate::query::Tags;
use tessella_orchestrate::tile::{Content, LayerBucket, TileId, build_mvt_tile, build_tile};
use tessella_source::tiling::TilingOptions;
use tessella_style::{Style, Value};

const BERLIN: &[u8] =
    include_bytes!("../../../tests/mvt-fixtures/protomaps-berlin-14-8802-5373.mvt");

/// Two squares well apart, and a line between them with a string id.
///
/// Degrees rather than a tight cluster: at z0 a tenth of a degree is a fifth of a tile unit and
/// rounds to nothing, so a fixture written small measures the rounding instead of the records. The
/// first version of this file did exactly that and every count came back zero.
const DATA: &str = r##"{"type":"FeatureCollection","features":[
  {"type":"Feature","id":1,"properties":{"kind":"park","name":"Tiergarten"},
   "geometry":{"type":"Polygon","coordinates":[[[-60.0,10.0],[-40.0,10.0],[-40.0,30.0],[-60.0,30.0],[-60.0,10.0]]]}},
  {"type":"Feature","id":2,"properties":{"kind":"wood"},
   "geometry":{"type":"Polygon","coordinates":[[[40.0,-30.0],[60.0,-30.0],[60.0,-10.0],[40.0,-10.0],[40.0,-30.0]]]}},
  {"type":"Feature","id":"ribbon","properties":{"kind":"path"},
   "geometry":{"type":"LineString","coordinates":[[-10.0,0.0],[10.0,0.0]]}},
  {"type":"Feature","properties":{"kind":"marker"},
   "geometry":{"type":"Point","coordinates":[0.0,40.0]}}]}"##;

/// The fixture's buckets for a one-layer style of `kind`, with the paint written as given.
fn built(kind: &str, paint: &str) -> Vec<LayerBucket> {
    let style_json = format!(
        r##"{{"version":8,"sources":{{"s":{{"type":"geojson","data":{DATA}}}}},
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

/// A bucket's vertex positions, whichever family it is.
fn positions(bucket: &LayerBucket) -> Vec<[i16; 2]> {
    match &bucket.content {
        Content::Fill(fill) => fill.vertices.clone(),
        Content::Fill3d(fill) => fill.vertices.iter().map(|v| v.position).collect(),
        // A line's position is doubled with cap and side flags in the low bits, which is
        // near enough for a bounds check and is what a query will have to decode as well.
        Content::Line(line) => line.vertices.iter().map(|v| v.pos_normal).collect(),
        Content::Circle(circle) => circle.vertices.clone(),
        other => panic!("no vertices for {other:?}"),
    }
}

/// A uniform-paint layer records its features, which is the row #353's index cannot reach.
///
/// Both halves asserted together on purpose. That the records appear is half the claim; that the
/// binder's own index is still empty is what says the two are different mechanisms rather than
/// this test measuring the old one.
#[test]
fn a_uniform_paint_layer_records_its_features() {
    let buckets = built("fill", r##"{"fill-color":"#ff0000"}"##);
    let bucket = buckets.first().expect("one fill bucket");

    assert_eq!(
        bucket.binder.index().len(),
        0,
        "the paint is uniform, so the binder has no per-vertex buffer and records nothing"
    );
    assert_eq!(
        bucket.features.len(),
        3,
        "both polygons and the line are recorded -- `FillBucket::addFeature` makes no geometry-type \
         check, so a line in a fill layer becomes a degenerate ring whose vertices are still written"
    );
}

/// A data-driven layer that reads no state records too.
///
/// The control for the test above: the binder's buffer exists here and its index is *still* empty,
/// so the emptiness is #353's state gate rather than an absent buffer, and the records are not
/// riding on the buffer either way.
#[test]
fn a_data_driven_layer_records_without_reading_state() {
    let buckets = built(
        "fill",
        r##"{"fill-color":["match",["get","kind"],"park","#00ff00","#0000ff"]}"##,
    );
    let bucket = buckets.first().expect("one fill bucket");

    assert!(
        bucket.binder.vertex_count() > 0,
        "a data-driven paint writes a per-vertex buffer"
    );
    assert_eq!(bucket.binder.index().len(), 0, "and still records nothing");
    assert_eq!(bucket.features.len(), 3, "the records are there regardless");
}

/// Each record's vertex range holds that feature's own geometry.
///
/// The one that matters. A query walks these ranges to decide what was hit, so a range attributed
/// to the wrong feature is a host told it tapped the park when it tapped the wood -- and both
/// records exist, both ranges are in bounds, and nothing about the map looks wrong.
///
/// Checked against each polygon's own tile-space bounds rather than against the other record: the
/// two squares are a third of the world apart, so a swapped or slid range cannot land inside the
/// box it claims.
#[test]
fn a_records_range_holds_that_features_vertices() {
    let buckets = built("fill", r##"{"fill-color":"#ff0000"}"##);
    let bucket = buckets.first().expect("one fill bucket");
    let vertices = positions(bucket);

    // Tile-space x at z0, which is `tiling::EXTENT` units across the whole world -- 8192, not the
    // 4096 this test first assumed. The line is absent on purpose: `classify_rings` drops it on
    // area, where it keeps the lone *point* by short-circuiting before the area filter, so a fill
    // layer's three records are the two polygons and the point.
    let expected = [
        (Value::Number(1.0), (-60.0, -40.0)),
        (Value::Number(2.0), (40.0, 60.0)),
        (Value::Null, (0.0, 0.0)),
    ];
    assert_eq!(bucket.features.len(), expected.len());

    for (record, (id, (west, east))) in bucket.features.iter().zip(expected) {
        let expected_id = (id != Value::Null).then_some(id.clone());
        assert_eq!(record.id, expected_id, "records are in draw order");
        assert!(
            !record.vertices.is_empty(),
            "a recorded feature filled vertices"
        );
        let extent = f64::from(tessella_source::tiling::EXTENT);
        let lo = ((west + 180.0) / 360.0 * extent) as i16;
        let hi = ((east + 180.0) / 360.0 * extent) as i16;
        for index in record.vertices.clone() {
            let point = vertices[index as usize];
            assert!(
                (lo - 1..=hi + 1).contains(&point[0]),
                "feature {expected_id:?} claims vertex {index} at x={} but its own span is \
                 {lo}..={hi}",
                point[0],
            );
        }
    }
}

/// The ranges tile the buffer: no gap, no overlap, nothing past the end.
///
/// Separate from the test above because it fails differently. That one catches a range pointing at
/// another feature's geometry; this catches a tracker that never advanced, which makes every range
/// start at zero -- each one still inside the first feature's box, so the bounds check alone would
/// pass the first record and only fail the second.
#[test]
fn the_ranges_tile_the_buffer() {
    let buckets = built("line", r##"{"line-color":"#ff0000"}"##);
    let bucket = buckets.first().expect("one line bucket");
    let total = positions(bucket).len();

    let mut at = 0u32;
    for record in &bucket.features {
        assert_eq!(
            record.vertices.start, at,
            "a record begins where the last one ended"
        );
        assert!(
            record.vertices.end > record.vertices.start,
            "and is not empty"
        );
        at = record.vertices.end;
    }
    assert_eq!(
        at as usize, total,
        "and the last one ends at the buffer's end"
    );
}

/// A feature that reached the arm and drew nothing is not recorded.
///
/// The case the guard exists for, and it is narrower than it looks. A feature the filter rejected,
/// or one whose geometry type the arm does not draw, never reaches the recording at all -- the
/// circle arm `continue`s on a non-point before it gets there. What does reach it is a feature
/// whose geometry was *clipped away*: the line arm runs the clip, gets nothing back, and arrives at
/// the push site with the bucket's vertex count unmoved.
///
/// Without the guard this fixture records the far line **twice**, once per world copy that reached
/// the push site, each with a range of `4..4`. An empty range is worse than a missing record: it
/// has no vertices to test a query box against, so depending on how the test is written it is
/// either never hit or hit by everything, and the feature is reported for a tile it is not on.
#[test]
fn a_feature_that_drew_nothing_is_not_recorded() {
    // One line on the tile and one on the far side of the world, at z2 so that "far" is outside
    // the tile and still inside the world. At z0 there is nowhere to put it.
    const FAR: &str = r##"{"type":"FeatureCollection","features":[
      {"type":"Feature","id":"near",
       "geometry":{"type":"LineString","coordinates":[[-170.0,80.0],[-160.0,80.0]]}},
      {"type":"Feature","id":"far",
       "geometry":{"type":"LineString","coordinates":[[150.0,-80.0],[160.0,-80.0]]}}]}"##;
    let style_json = format!(
        r##"{{"version":8,"sources":{{"s":{{"type":"geojson","data":{FAR}}}}},
             "layers":[{{"id":"L","type":"line","source":"s","paint":{{"line-color":"#ff0000"}}}}]}}"##
    );
    let style = Style::parse(&style_json).expect("the style parses");
    let tessella_style::Source::Geojson(source) = style.source("s").expect("a source") else {
        panic!("the fixture has one geojson source")
    };
    let features = tessella_source::geojson::read(&source.data).expect("features read");
    let buckets = build_tile(
        &style,
        "s",
        TileId::new(2, 0, 0),
        &features,
        TilingOptions::default(),
    )
    .expect("the tile builds");
    let bucket = buckets.first().expect("one line bucket");

    assert_eq!(
        bucket.features.len(),
        1,
        "only the line on this tile is recorded: {:?}",
        bucket
            .features
            .iter()
            .map(|f| (f.id.clone(), f.vertices.clone()))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        bucket.features[0].id,
        Some(Value::String("near".into())),
        "and it is the near one"
    );
    assert!(
        bucket.features.iter().all(|f| !f.vertices.is_empty()),
        "no record carries an empty range"
    );
}

/// A circle layer records only the points it drew.
///
/// The other half of the arm: a non-point feature never reaches the recording, so the count is the
/// points alone rather than everything the filter admitted.
#[test]
fn a_circle_records_only_its_points() {
    let buckets = built("circle", r##"{"circle-color":"#ff0000"}"##);
    let bucket = buckets.first().expect("one circle bucket");

    assert_eq!(
        bucket.features.len(),
        1,
        "the fixture has one point; its polygons and line are not a circle's geometry"
    );
    let record = &bucket.features[0];
    assert_eq!(record.geometry_type, "Point");
    assert_eq!(record.id, None, "the fixture's point carries no id");
    assert!(!record.vertices.is_empty());
}

/// A string id survives, which feature state cannot carry.
///
/// `PaintBinder`'s index narrows an id through `numeric_id` and drops anything else, because state
/// is keyed by id and a feature without a numeric one cannot be named by a host. A query is the
/// other way round: it reports what the source said, and a host that gets `"ribbon"` back can act
/// on it even though it could not have set state for it.
#[test]
fn a_string_id_is_reported_as_it_was_given() {
    let buckets = built("line", r##"{"line-color":"#ff0000"}"##);
    let bucket = buckets.first().expect("one line bucket");

    let ids: Vec<Option<Value>> = bucket.features.iter().map(|f| f.id.clone()).collect();
    assert!(
        ids.contains(&Some(Value::String("ribbon".into()))),
        "the line's string id is reported verbatim, not dropped: {ids:?}"
    );
}

/// A record's properties are the feature's own, readable by name.
#[test]
fn a_record_carries_the_features_properties() {
    let buckets = built("fill", r##"{"fill-color":"#ff0000"}"##);
    let bucket = buckets.first().expect("one fill bucket");
    let park = &bucket.features[0];

    assert_eq!(
        park.properties.get("kind"),
        Some(Value::String("park".into()))
    );
    assert_eq!(
        park.properties.get("name"),
        Some(Value::String("Tiergarten".into()))
    );
    assert_eq!(park.properties.get("absent"), None);
    assert_eq!(park.properties.len(), 2);
    assert!(!park.properties.is_empty());
}

/// A family that draws from no features records nothing.
#[test]
fn a_background_records_nothing() {
    let style_json = r##"{"version":8,"sources":{},
      "layers":[{"id":"B","type":"background","paint":{"background-color":"#ff0000"}}]}"##;
    let style = Style::parse(style_json).expect("the style parses");
    // `build_sourceless`, not `build_tile`: a background names no source, and `draws_from` keys a
    // bucket to the source the layer asked for, so the feature builders never see one.
    let buckets = tessella_orchestrate::tile::build_sourceless(&style, TileId::new(0, 0, 0))
        .expect("the tile builds");

    let bucket = buckets.first().expect("one background bucket");
    assert_eq!(bucket.features.len(), 0);
}

/// Every MVT record points at one shared property table, not at a copy of its own.
///
/// This is the whole shape of the MVT arm, measured rather than asserted in prose. A layer's
/// `properties` is already one flat table of every property of every feature, which a `Feature`
/// indexes with a range -- so a record that copied its slice out would copy a table that already
/// exists. On a Berlin z14 tile that table is 122 KiB.
///
/// `Arc::ptr_eq` is the only thing that tells a share from a copy: two copies of one table compare
/// equal under `==`, so the pointer is the assertion.
///
/// The second half is the part that pays. A real style names one source layer from many style
/// layers -- a dozen road layers over `roads` is ordinary -- and each becomes its own bucket. Under
/// a per-feature copy each of those buckets carries its own slice of the same table, so the cost is
/// the table times the number of layers. Here the table is one.
#[test]
fn every_mvt_record_points_at_one_shared_table() {
    // Four style layers over *one* source layer, filtered differently so they are not the same
    // bucket twice: the shape a real style has, and the one a per-feature copy multiplies.
    let style_json = r##"{"version":8,
      "sources":{"p":{"type":"vector","tiles":["http://x/{z}/{x}/{y}"]}},
      "layers":[
        {"id":"r1","type":"line","source":"p","source-layer":"roads",
         "paint":{"line-color":"#ff0000"}},
        {"id":"r2","type":"line","source":"p","source-layer":"roads",
         "filter":["==",["geometry-type"],"LineString"],"paint":{"line-color":"#00ff00"}},
        {"id":"r3","type":"line","source":"p","source-layer":"roads",
         "paint":{"line-color":"#0000ff","line-width":3.0}},
        {"id":"r4","type":"fill","source":"p","source-layer":"roads",
         "paint":{"fill-color":"#ffff00"}}]}"##;
    let style = Style::parse(style_json).expect("the style parses");
    let tile = tessella_source::mvt::Tile::decode(BERLIN).expect("the fixture decodes");
    let buckets =
        build_mvt_tile(&style, "p", TileId::new(14, 8802, 5373), &tile).expect("the tile builds");

    assert_eq!(buckets.len(), 4, "four style layers over one source layer");

    // Raw pointers, not `Arc` clones: cloning one per record would bump the very count asserted
    // below, which is how the first version of this test read 1865 where it expected 934.
    let mut pointers: Vec<*const Vec<(Arc<str>, tessella_source::mvt::Value)>> = Vec::new();
    let mut records = 0usize;
    let mut handle = None;
    for bucket in &buckets {
        assert!(
            !bucket.features.is_empty(),
            "every one of these layers draws something"
        );
        for record in &bucket.features {
            records += 1;
            let Tags::Mvt { table, range } = &record.properties else {
                panic!("an MVT feature's tags are not a JSON object");
            };
            assert!(
                range.end as usize <= table.len(),
                "a record's range is inside the table it points at"
            );
            pointers.push(Arc::as_ptr(table));
            if handle.is_none() {
                handle = Some(Arc::clone(table));
            }
        }
    }

    assert!(
        records > 400,
        "a Berlin roads layer four times over: {records}"
    );
    let first = pointers[0];
    assert!(
        pointers.iter().all(|table| core::ptr::eq(*table, first)),
        "all {records} records across all four buckets point at one table, not at copies of it"
    );
    // And one allocation rather than one per record: a per-feature copy would be 900-odd tables
    // with 122 KiB of pairs between them. The `+ 2` is the decoded layer and this test's handle.
    assert_eq!(
        Arc::strong_count(handle.as_ref().expect("some record was seen")),
        records + 2,
        "one table, referenced once per record plus the layer and this test's own handle"
    );
}

/// Every MVT record's range selects its *own* feature's properties out of the shared table.
///
/// The test the sharing needs and the one two mutations walked straight through: widening the range
/// to the whole table, and dropping its start so every record reads from zero, both left the suite
/// green. Nothing was checking that a record's window is the right window -- the only MVT property
/// assertions were "some record has a `kind`" and a count compared against the range it came from,
/// which is true of any range at all.
///
/// Pinned by index rather than by id, because this layer has two adjacent features carrying the
/// *same* id and different tags -- which is also what makes it a good discriminator: a slice read
/// from the wrong start lands on a neighbor whose tags differ although its id does not.
#[test]
fn an_mvt_records_window_is_its_own_features() {
    let style_json = r##"{"version":8,
      "sources":{"p":{"type":"vector","tiles":["http://x/{z}/{x}/{y}"]}},
      "layers":[{"id":"roads","type":"line","source":"p","source-layer":"roads",
                 "paint":{"line-color":"#ff0000"}}]}"##;
    let style = Style::parse(style_json).expect("the style parses");
    let tile = tessella_source::mvt::Tile::decode(BERLIN).expect("the fixture decodes");
    let layer = tile.layer("roads").expect("the tile carries roads");
    let buckets =
        build_mvt_tile(&style, "p", TileId::new(14, 8802, 5373), &tile).expect("the tile builds");
    let bucket = buckets.first().expect("one roads bucket");

    // No filter and no sort key, so the arm keeps every feature in source order and record `i` is
    // feature `i`. Asserted rather than assumed -- the mapping is what the rest of this test rests
    // on, and a dropped feature would slide every comparison after it.
    assert_eq!(
        bucket.features.len(),
        layer.len(),
        "an unfiltered layer records every feature"
    );

    let mut checked = 0usize;
    for (at, record) in bucket.features.iter().enumerate() {
        let feature = layer.feature(at).expect("a feature at every index");
        let expected: Vec<(String, String)> = feature
            .properties()
            .iter()
            .map(|(key, value)| (key.to_string(), format!("{value:?}")))
            .collect();
        let got: Vec<(String, String)> = record
            .properties
            .iter()
            .map(|(key, value)| (key.to_string(), format!("{value:?}")))
            .collect();
        assert_eq!(
            got.len(),
            expected.len(),
            "record {at} reports {} properties where its feature has {}",
            got.len(),
            expected.len()
        );
        for ((got_key, got_value), (want_key, want_value)) in got.iter().zip(&expected) {
            assert_eq!(got_key, want_key, "record {at}: key order is the tile's");
            // The values are compared through `Debug` because one side is an `mvt::Value` and the
            // other the `Value` it widens into; the point is that it is the same property, not that
            // the two enums are one type.
            assert!(
                want_value.contains(
                    got_value
                        .trim_start_matches("String(")
                        .trim_end_matches(')')
                ) || got_value.contains(
                    want_value
                        .trim_start_matches("String(")
                        .trim_end_matches(')')
                ),
                "record {at} key {got_key}: {got_value} is not its feature's {want_value}"
            );
        }
        checked += got.len();
    }
    assert!(
        checked > 1000,
        "the whole layer's properties were compared, not a corner: {checked}"
    );
}

/// An MVT record reports its numeric id and reads its tags by name.
#[test]
fn mvt_records_carry_id_and_tags() {
    let style_json = r##"{"version":8,"sources":{"p":{"type":"vector","tiles":["http://x/{z}/{x}/{y}"]}},
      "layers":[{"id":"roads","type":"line","source":"p","source-layer":"roads",
                 "paint":{"line-color":"#ff0000"}}]}"##;
    let style = Style::parse(style_json).expect("the style parses");
    let tile = tessella_source::mvt::Tile::decode(BERLIN).expect("the fixture decodes");
    let buckets =
        build_mvt_tile(&style, "p", TileId::new(14, 8802, 5373), &tile).expect("the tile builds");
    let bucket = buckets.first().expect("one roads bucket");

    assert!(
        bucket.features.len() > 100,
        "a Berlin z14 roads layer has many features, not {}",
        bucket.features.len()
    );
    assert!(
        bucket
            .features
            .iter()
            .any(|f| f.properties.get("kind").is_some()),
        "protomaps roads carry a `kind`"
    );
    assert!(
        bucket
            .features
            .iter()
            .all(|f| f.geometry_type == "LineString" || f.geometry_type == "Point"),
        "a roads layer's geometry is lines, with the odd point"
    );
    // Every record's range is inside the buffer it indexes, over a real tile rather than a fixture.
    let total = positions(bucket).len() as u32;
    for record in &bucket.features {
        assert!(
            record.vertices.end <= total,
            "a record points past the buffer: {:?} of {total}",
            record.vertices
        );
    }
}

/// Iterating a record's properties yields every one, in both shapes.
#[test]
fn properties_iterate_in_both_shapes() {
    let geojson = built("fill", r##"{"fill-color":"#ff0000"}"##);
    let park = &geojson[0].features[0];
    let mut pairs: Vec<(String, Value)> = park
        .properties
        .iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        pairs,
        vec![
            ("kind".to_string(), Value::String("park".into())),
            ("name".to_string(), Value::String("Tiergarten".into())),
        ]
    );

    let style_json = r##"{"version":8,"sources":{"p":{"type":"vector","tiles":["http://x/{z}/{x}/{y}"]}},
      "layers":[{"id":"roads","type":"line","source":"p","source-layer":"roads",
                 "paint":{"line-color":"#ff0000"}}]}"##;
    let style = Style::parse(style_json).expect("the style parses");
    let tile = tessella_source::mvt::Tile::decode(BERLIN).expect("the fixture decodes");
    let buckets =
        build_mvt_tile(&style, "p", TileId::new(14, 8802, 5373), &tile).expect("the tile builds");
    let record = buckets[0]
        .features
        .iter()
        .find(|f| f.properties.len() > 1)
        .expect("some road carries more than one tag");
    let Tags::Mvt { range, .. } = &record.properties else {
        panic!("an MVT feature's tags")
    };
    assert_eq!(
        record.properties.iter().count(),
        (range.end - range.start) as usize,
        "the iterator yields every tag of the feature's own slice, not the whole table"
    );
}
