// SPDX-License-Identifier: BSD-2-Clause
//! A GeoJSON line with `lineMetrics`, carried through a tile into the distance field a gradient
//! reads.
//!
//! Two faults sit behind the gradient-line examples drawing no line at all. `["line-progress"]` did
//! not parse, so a layer with a `line-gradient` failed to resolve its paint and the whole tile's
//! build failed with it; and nothing read `lineMetrics`, so a line cut by a tile boundary spread its
//! distances over its own piece rather than over the whole line, and a gradient would have started
//! over at every tile edge.

use tessella_orchestrate::tile::{Content, TileId, build_tile};
use tessella_source::geojson;
use tessella_source::tiling::{EXTENT, TilingOptions};
use tessella_style::{Source, Style};

/// Where the line starts and ends, in degrees of longitude.
const WEST: f64 = -135.0;
const EAST: f64 = 135.0;

/// A line from 135 degrees west to 135 degrees east along the tenth parallel, under a gradient.
fn style(line_metrics: bool) -> Style {
    serde_json::from_str(&format!(
        r#"{{"version": 8,
            "sources": {{"line": {{"type": "geojson", "lineMetrics": {line_metrics},
                "data": {{"type": "Feature", "properties": {{}},
                    "geometry": {{"type": "LineString", "coordinates": [[{WEST}, 10], [{EAST}, 10]]}}}}}}}},
            "layers": [{{"id": "line", "type": "line", "source": "line",
                "paint": {{"line-width": 14,
                    "line-gradient": ["interpolate", ["linear"], ["line-progress"],
                        0, "blue", 1, "red"]}}}}]}}"#
    ))
    .expect("a style")
}

/// Distance along the line, as a vertex carries it across two bytes.
fn linesofar(data: [u8; 4]) -> i32 {
    i32::from(data[2] >> 2) | (i32::from(data[3]) << 6)
}

/// The distance field's top: `(MAX_LINE_DISTANCE - 1) * LINE_DISTANCE_SCALE`.
const TOP: f64 = 16383.0;

/// Builds tile 1/0/0 -- the western half of the northern hemisphere -- and returns the first and
/// last distance its line's vertices carry.
fn distances(line_metrics: bool) -> (i32, i32) {
    let style = style(line_metrics);
    let Some(Source::Geojson(source)) = style.source("line") else {
        panic!("a geojson source");
    };
    let features = geojson::read(&source.data).expect("features");
    let buckets = build_tile(
        &style,
        "line",
        TileId::new(1, 0, 0),
        &features,
        TilingOptions::default(),
    )
    .expect("a layer with a line-gradient builds");
    let Content::Line(line) = &buckets[0].content else {
        panic!("a line bucket");
    };
    let first = linesofar(line.vertices[0].data);
    let last = linesofar(line.vertices.last().expect("vertices").data);
    (first, last)
}

/// The tile holds the line from its start to the eastern edge of its buffer. With metrics its
/// distances run from the start of the field to that share of the top.
///
/// The share is computed rather than written down: the buffer reaches past 0 degrees by however
/// many tile units the tiling options give it, and at zoom 1 a tile spans 180 degrees of longitude
/// linearly, so the edge's longitude and its fraction of the line follow from the clip range.
#[test]
fn a_clipped_piece_spans_its_share_of_the_whole_line() {
    let (first, last) = distances(true);
    assert_eq!(first, 0, "the piece starts where the line does");

    let (_, hi) = TilingOptions::default().clip_range();
    let edge = f64::from(hi) / f64::from(EXTENT) * 180.0 - 180.0;
    let share = (edge - WEST) / (EAST - WEST);
    assert!(
        share > 0.0 && share < 1.0,
        "the tile's buffer is meant to cut the line: {share}"
    );
    let expected = share * TOP;
    assert!(
        (f64::from(last) - expected).abs() <= 1.0,
        "the piece ends at {last}, where {expected} is {share} of the way along"
    );
}

/// Without metrics the same piece measures only itself, which is not a fraction of anything.
#[test]
fn without_metrics_the_piece_measures_itself() {
    let (_, metered) = distances(true);
    let (_, plain) = distances(false);
    assert_ne!(plain, metered);
}
