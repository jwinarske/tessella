// SPDX-License-Identifier: BSD-2-Clause
//! Where a label anchors, for the two geometries that do not anchor at a point.
//!
//! A symbol layout reads a *tile's* geometry: geojson-vt cuts a feature to the buffered tile once,
//! before mbgl sees it, and `addSymbolInstance` then keeps only the anchors that fall inside the
//! tile proper. Neither held on the GeoJSON path here, and `symbol-placement: line-center` -- the
//! one placement that deliberately does not clip -- showed both: one label at the midpoint of the
//! whole road instead of one per tile it crosses, and a second copy from every tile whose buffer
//! the road merely reaches. An area was worse: it anchored at a ring's first vertex, which is a
//! corner of the shape, once per ring, so a hole got a label of its own.
//!
//! Measured on the parity scenes `linelabel_p` (1,727 gross pixels at z14 p0) and `arealabel_p`
//! (2,536), both 0 after.

use tessella_glyph::fonts::Fonts;
use tessella_layout::symbol_layout::{Anchoring, SymbolLayout};
use tessella_orchestrate::tile::{TileId, build_tile};
use tessella_source::tiling::TilingOptions;
use tessella_storage::source::{FetchError, FileSource, Response};
use tessella_style::Style;

const GLYPHS: &[u8] = include_bytes!("../../../tests/glyph-fixtures/TestFont/0-255.pbf");

/// A z14 tile's own units as a longitude, so a fixture can say where in a tile it sits.
fn lon_at(x: u32, units: f64) -> f64 {
    (f64::from(x) + units / 8192.0) / 16384.0 * 360.0 - 180.0
}

/// The same for a row, through the Mercator the projection uses.
fn lat_at(y: u32, units: f64) -> f64 {
    let fraction = (f64::from(y) + units / 8192.0) / 16384.0;
    (std::f64::consts::PI * (1.0 - 2.0 * fraction))
        .sinh()
        .atan()
        .to_degrees()
}

/// An origin serving the vendored font and nothing else.
struct Origin;

impl FileSource for Origin {
    fn fetch(&self, url: &str) -> Result<Response, FetchError> {
        let body = if url.contains("TestFont") && url.contains("0-255") {
            GLYPHS.to_vec()
        } else {
            Vec::new()
        };
        Ok(Response {
            status: 200,
            body,
            ..Response::default()
        })
    }
}

fn fonts_for(layout: &SymbolLayout) -> Fonts {
    let mut fonts = Fonts::new("https://example.com/fonts/{fontstack}/{range}.pbf");
    fonts
        .fetch(&layout.dependencies(), &Origin)
        .expect("the origin answers");
    fonts
}

/// A one-layer symbol style over `geometry`, placed as `placement` says.
fn style_over(placement: &str, geometry: &str) -> Style {
    Style::parse(&format!(
        r#"{{"version": 8,
             "sources": {{"probe": {{"type": "geojson", "data": {{
               "type": "Feature", "properties": {{}}, "geometry": {geometry}}}}}}},
             "layers": [{{"id": "labels", "type": "symbol", "source": "probe",
                          "layout": {{"text-field": "ABC", "text-font": ["TestFont"],
                                      "text-size": 14, "symbol-placement": "{placement}",
                                      "text-allow-overlap": true,
                                      "text-ignore-placement": true}}}}]}}"#
    ))
    .expect("the style parses")
}

/// The layout one tile of `style` produces.
fn layout_of(style: &Style, tile: TileId) -> Option<SymbolLayout> {
    let tessella_style::Source::Geojson(source) = style.source("probe").expect("a source") else {
        panic!("the fixture has one geojson source")
    };
    let features = tessella_source::geojson::read(&source.data).expect("features read");
    let built = build_tile(style, "probe", tile, &features, TilingOptions::default())
        .expect("the tile builds");
    built
        .iter()
        .find_map(|bucket| bucket.content.as_symbol().cloned())
}

/// Every anchor one tile places, in that tile's units.
fn anchors_in(style: &Style, tile: TileId) -> Vec<(f32, f32)> {
    let Some(layout) = layout_of(style, tile) else {
        return Vec::new();
    };
    let fonts = fonts_for(&layout);
    let (_, laid) = layout.lay_out(&fonts, None);
    laid.iter().map(|entry| entry.anchor).collect()
}

/// A line crossing the boundary between tiles 8801 and 8802, from 1,734 units into the first to
/// 7,968 units into the second.
fn crossing_line() -> Style {
    let (start, end) = (lon_at(8801, 1734.0), lon_at(8802, 7968.0));
    let lat = lat_at(5369, 4096.0);
    style_over(
        "line-center",
        &format!(r#"{{"type": "LineString", "coordinates": [[{start}, {lat}], [{end}, {lat}]]}}"#),
    )
}

/// Each tile centers the label on its own clipped run.
///
/// The run is the feature cut to `[-2048, EXTENT + 2048]`, so the two tiles disagree about where
/// the middle of the road is -- which is the point: each draws the name in the middle of the part
/// of it that is there. Uncut, the whole line's midpoint is 8,947 units into tile 8801, outside it
/// entirely, so that tile drew nothing and tile 8802 drew one label 755 units in.
#[test]
fn a_line_center_label_takes_the_middle_of_this_tiles_own_run() {
    let style = crossing_line();

    let first = anchors_in(&style, TileId::new(14, 8801, 5369));
    assert_eq!(first.len(), 1, "one label on the first tile");
    assert_eq!(first[0].0.round(), 5987.0, "the middle of [1734, 10240]");

    let second = anchors_in(&style, TileId::new(14, 8802, 5369));
    assert_eq!(second.len(), 1, "one label on the second tile");
    assert_eq!(second[0].0.round(), 2960.0, "the middle of [-2048, 7968]");
}

/// A line lying only in a tile's buffer is the neighbor's to label.
///
/// Its run has a midpoint like any other, and the anchor there is outside the tile: mbgl's
/// `addSymbolInstance` drops it because "neighboring symbols will be added as part of the
/// neighboring tiles". Kept, it is a second copy of the name a few pixels from the first.
#[test]
fn a_run_only_in_the_buffer_places_nothing() {
    let lat = lat_at(5369, 4096.0);
    let buffer = style_over(
        "line-center",
        &format!(
            r#"{{"type": "LineString", "coordinates": [[{}, {lat}], [{}, {lat}]]}}"#,
            lon_at(8802, -2000.0),
            lon_at(8802, -200.0)
        ),
    );
    assert!(
        anchors_in(&buffer, TileId::new(14, 8802, 5369)).is_empty(),
        "a label in the buffer was placed"
    );

    // The control, which differs only in being inside: the same 1,800-unit line moved in.
    let inside = style_over(
        "line-center",
        &format!(
            r#"{{"type": "LineString", "coordinates": [[{}, {lat}], [{}, {lat}]]}}"#,
            lon_at(8802, 200.0),
            lon_at(8802, 2000.0)
        ),
    );
    let placed = anchors_in(&inside, TileId::new(14, 8802, 5369));
    assert_eq!(placed.len(), 1, "the same line inside the tile is labeled");
    assert_eq!(placed[0].0.round(), 1100.0, "at the middle of its run");
}

/// An L anchors inside its own arm, not at the corner the ring starts from.
///
/// The arms below are 1,024 units wide, so the largest circle that fits has a radius of 512 and
/// its center runs down the middle of either arm -- near (1,536, 1,536). mbgl asks for the pole to
/// one pixel, which is 16 tile units, and the search stops as soon as nothing unexplored could
/// beat the best by more than that, so the answer is near the middle rather than on it. The first
/// vertex, which is what this used to anchor at, is at (1,024, 1,024) -- on the corner itself.
#[test]
fn an_area_anchors_at_its_pole_and_not_at_a_vertex() {
    let ell = |units: &[(f64, f64)]| {
        let points: Vec<String> = units
            .iter()
            .chain(units.first())
            .map(|&(x, y)| format!("[{}, {}]", lon_at(8802, x), lat_at(5369, y)))
            .collect();
        format!(
            r#"{{"type": "Polygon", "coordinates": [[{}]]}}"#,
            points.join(", ")
        )
    };
    let style = style_over(
        "point",
        &ell(&[
            (1024.0, 1024.0),
            (5120.0, 1024.0),
            (5120.0, 2048.0),
            (2048.0, 2048.0),
            (2048.0, 5120.0),
            (1024.0, 5120.0),
        ]),
    );
    let layout = layout_of(&style, TileId::new(14, 8802, 5369)).expect("a layout");
    assert_eq!(layout.pending.len(), 1, "one label for one polygon");
    let Anchoring::Point(at) = layout.pending[0].anchoring else {
        panic!("a point-placed label anchors at a point")
    };
    assert!(
        (at.0 - 1536.0).abs() < 100.0 && (at.1 - 1536.0).abs() < 100.0,
        "expected the pole near (1536, 1536), got {at:?}"
    );
}

/// A hole is part of its polygon, not a polygon of its own.
#[test]
fn a_hole_does_not_get_a_label_of_its_own() {
    let ring = |xs: (f64, f64), ys: (f64, f64), reverse: bool| {
        let mut corners = [
            (xs.0, ys.0),
            (xs.1, ys.0),
            (xs.1, ys.1),
            (xs.0, ys.1),
            (xs.0, ys.0),
        ];
        if reverse {
            corners.reverse();
        }
        let points: Vec<String> = corners
            .iter()
            .map(|&(x, y)| format!("[{}, {}]", lon_at(8802, x), lat_at(5369, y)))
            .collect();
        format!("[{}]", points.join(", "))
    };
    let style = style_over(
        "point",
        &format!(
            r#"{{"type": "Polygon", "coordinates": [{}, {}]}}"#,
            ring((512.0, 7680.0), (512.0, 7680.0), false),
            ring((2048.0, 6144.0), (2048.0, 6144.0), true)
        ),
    );
    let layout = layout_of(&style, TileId::new(14, 8802, 5369)).expect("a layout");
    assert_eq!(layout.pending.len(), 1, "the hole is not a second label");
}

/// A multipolygon is several polygons and several labels.
#[test]
fn each_polygon_of_a_multipolygon_is_labeled() {
    let square = |x: f64| {
        let points: Vec<String> = [
            (x, 1024.0),
            (x + 1024.0, 1024.0),
            (x + 1024.0, 2048.0),
            (x, 2048.0),
            (x, 1024.0),
        ]
        .iter()
        .map(|&(x, y)| format!("[{}, {}]", lon_at(8802, x), lat_at(5369, y)))
        .collect();
        format!("[[{}]]", points.join(", "))
    };
    let style = style_over(
        "point",
        &format!(
            r#"{{"type": "MultiPolygon", "coordinates": [{}, {}]}}"#,
            square(512.0),
            square(4096.0)
        ),
    );
    let layout = layout_of(&style, TileId::new(14, 8802, 5369)).expect("a layout");
    assert_eq!(layout.pending.len(), 2, "one label per polygon");
}

/// A line still anchors at its first vertex, which is the arm mbgl keeps for a `LineString`.
///
/// Three points rather than two, and bent: a straight line encloses nothing, and measured as an
/// area a shape that encloses nothing poles at its first vertex anyway -- so a two-point fixture
/// cannot tell the two arms apart. Bent, the area rule would answer somewhere in the middle.
#[test]
fn a_point_placed_line_still_anchors_at_its_first_vertex() {
    let style = style_over(
        "point",
        &format!(
            r#"{{"type": "LineString", "coordinates": [[{}, {}], [{}, {}], [{}, {}]]}}"#,
            lon_at(8802, 6144.0),
            lat_at(5369, 2048.0),
            lon_at(8802, 2048.0),
            lat_at(5369, 2048.0),
            lon_at(8802, 2048.0),
            lat_at(5369, 6144.0)
        ),
    );
    let layout = layout_of(&style, TileId::new(14, 8802, 5369)).expect("a layout");
    let Anchoring::Point(at) = layout.pending[0].anchoring else {
        panic!("a point-placed label anchors at a point")
    };
    assert_eq!(
        (at.0.round(), at.1.round()),
        (6144.0, 2048.0),
        "the first vertex, not a measure of the whole line"
    );
}

// `FileSource` is `Send + Sync`; nothing here is shared across threads.
unsafe impl Sync for Origin {}
unsafe impl Send for Origin {}
