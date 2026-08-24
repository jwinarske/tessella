//! A GeoJSON source given by URL, through the whole pipeline.
//!
//! `tessella-storage`'s own tests cover resolving the document. This is the half that only
//! makes sense above it: one fetch feeding every tile of a cover, because a GeoJSON source is
//! cut up by the *client*, and there is nothing per-tile to ask for.

#![cfg(feature = "std")]

use tessella_orchestrate::tile::{TileId, bucket_for, build_tile};
use tessella_source::geojson;
use tessella_source::tiling::TilingOptions;
use tessella_storage::geojson::resolve;
use tessella_storage::http::HttpFileSource;
use tessella_style::{Source, Style};

const DOCUMENT: &str = r#"{
  "type": "FeatureCollection",
  "features": [
    {"type": "Feature", "properties": {"kind": "a"},
     "geometry": {"type": "Polygon",
       "coordinates": [[[-0.16,51.49],[-0.16,51.52],[-0.12,51.52],[-0.12,51.49],[-0.16,51.49]]]}},
    {"type": "Feature", "properties": {"kind": "b"},
     "geometry": {"type": "LineString",
       "coordinates": [[-0.18,51.48],[-0.12,51.51],[-0.06,51.50]]}}
  ]
}"#;

fn style_with(data: &str) -> Style {
    Style::parse(&format!(
        r##"{{"version": 8,
             "sources": {{"g": {{"type": "geojson", "data": {data}}}}},
             "layers": [
               {{"id": "f", "type": "fill", "source": "g",
                 "paint": {{"fill-color": "#3050c0"}}}},
               {{"id": "l", "type": "line", "source": "g",
                 "paint": {{"line-color": "#c04030", "line-width": 2.0}}}}]}}"##
    ))
    .expect("style parses")
}

fn geojson_source(style: &Style) -> &tessella_style::GeojsonSource {
    match style.source("g") {
        Some(Source::Geojson(source)) => source,
        other => panic!("{other:?}"),
    }
}

/// The nine tiles around the document's features.
const COVER: [(u32, u32); 9] = [
    (4092, 2722),
    (4092, 2723),
    (4092, 2724),
    (4093, 2722),
    (4093, 2723),
    (4093, 2724),
    (4094, 2722),
    (4094, 2723),
    (4094, 2724),
];

/// One request serves every tile of the cover.
///
/// The structural difference from a vector source, and the reason a GeoJSON source cannot be
/// covered until its document lands: the tiling is the client's, so there is nothing per-tile
/// to fetch. Nine tiles, one request.
#[test]
fn one_request_serves_every_tile() {
    let server = tile_server::Server::start(tile_server::Routes::new().at(
        "/features.geojson",
        "application/json",
        DOCUMENT.as_bytes().to_vec(),
    ))
    .expect("binds");

    let style = style_with(&format!(r#""{}/features.geojson""#, server.origin()));
    let document = resolve(geojson_source(&style), &HttpFileSource::default()).expect("resolves");
    let features = geojson::read(&document).expect("reads");
    assert_eq!(features.len(), 2);

    let mut fills = 0usize;
    let mut lines = 0usize;
    for (x, y) in COVER {
        let buckets = build_tile(
            &style,
            TileId::new(13, x, y),
            &features,
            TilingOptions::default(),
        )
        .expect("builds");
        fills += bucket_for(&buckets, "f")
            .and_then(|b| b.content.as_fill())
            .map_or(0, |b| b.vertices.len());
        lines += bucket_for(&buckets, "l")
            .and_then(|b| b.content.as_line())
            .map_or(0, |b| b.vertices.len());
    }

    assert!(fills > 0, "the polygon tessellated somewhere");
    assert!(lines > 0, "the line extruded somewhere");
    assert_eq!(server.requests(), 1, "one request for nine tiles");
    assert_eq!(server.paths(), ["/features.geojson"]);
}

/// A URL-valued source produces the same map as the same document written inline.
///
/// The point of the whole exercise: `data` is one key meaning two things, and which one a style
/// used must not be visible in what it draws.
#[test]
fn a_url_source_draws_what_an_inline_one_does() {
    let server = tile_server::Server::start(tile_server::Routes::new().at(
        "/features.geojson",
        "application/json",
        DOCUMENT.as_bytes().to_vec(),
    ))
    .expect("binds");

    let inline_style = style_with(DOCUMENT);
    let remote_style = style_with(&format!(r#""{}/features.geojson""#, server.origin()));

    let files = HttpFileSource::default();
    let inline = resolve(geojson_source(&inline_style), &files).expect("resolves");
    let remote = resolve(geojson_source(&remote_style), &files).expect("resolves");
    let inline_features = geojson::read(&inline).expect("reads");
    let remote_features = geojson::read(&remote).expect("reads");

    for (x, y) in COVER {
        let build = |style: &Style, features: &[tessella_source::GeoJsonFeature]| {
            build_tile(
                style,
                TileId::new(13, x, y),
                features,
                TilingOptions::default(),
            )
            .expect("builds")
        };
        let a = build(&inline_style, &inline_features);
        let b = build(&remote_style, &remote_features);

        for id in ["f", "l"] {
            let left = bucket_for(&a, id).expect("the layer");
            let right = bucket_for(&b, id).expect("the layer");
            assert_eq!(left.content, right.content, "{id} at {x}/{y}");
            assert_eq!(
                left.binder.data(),
                right.binder.data(),
                "{id} paint at {x}/{y}"
            );
        }
    }
}
