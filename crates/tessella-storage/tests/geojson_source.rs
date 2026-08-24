//! GeoJSON sources: inline, by URL, and the ways a URL goes wrong.
//!
//! The style spec overloads one key for two things, so the shape of the bug this closes is
//! worth naming: a URL-valued source parsed fine, declared a source, and produced no features,
//! reporting "a GeoJSON object needs a string `type`" — an error about the document's contents
//! for a value that was never a document.

#![cfg(feature = "http")]

use tessella_storage::geojson::{GeoJsonSourceError, Origin, origin, resolve};
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
             "layers": [{{"id": "f", "type": "fill", "source": "g",
                          "paint": {{"fill-color": "#ff0000"}}}}]}}"##
    ))
    .expect("style parses")
}

fn source(style: &Style) -> &tessella_style::GeojsonSource {
    match style.source("g") {
        Some(Source::Geojson(source)) => source,
        other => panic!("{other:?}"),
    }
}

fn served(body: &str) -> tile_server::Server {
    tile_server::Server::start(tile_server::Routes::new().at(
        "/features.geojson",
        "application/json",
        body.as_bytes().to_vec(),
    ))
    .expect("binds")
}

/// An object is the document; a string is where to get it.
#[test]
fn data_is_discriminated_by_json_type() {
    let inline = style_with(DOCUMENT);
    assert_eq!(origin(source(&inline)).expect("classifies"), Origin::Inline);

    let remote = style_with(r#""https://host/features.geojson""#);
    assert_eq!(
        origin(source(&remote)).expect("classifies"),
        Origin::Url("https://host/features.geojson")
    );

    // Anything else is neither, and mbgl says so in these words.
    for data in ["12", "true", "null", "[1, 2]"] {
        let odd = style_with(data);
        assert_eq!(
            origin(source(&odd)),
            Err(GeoJsonSourceError::NotUrlOrObject),
            "{data}"
        );
    }
}

/// An inline document is not fetched, and not copied to be read.
#[test]
fn an_inline_document_costs_no_request() {
    let server = served(DOCUMENT);
    let style = style_with(DOCUMENT);
    let document = resolve(source(&style), &HttpFileSource::default()).expect("resolves");

    assert!(matches!(document, std::borrow::Cow::Borrowed(_)));
    assert_eq!(server.requests(), 0, "nothing was asked for");
    let features = tessella_source::geojson::read(&document).expect("reads");
    assert_eq!(features.len(), 2);
}

/// A URL is fetched once and parsed, and the result reads as GeoJSON.
#[test]
fn a_url_document_is_fetched_and_parsed() {
    let server = served(DOCUMENT);
    let style = style_with(&format!(r#""{}/features.geojson""#, server.origin()));
    let document = resolve(source(&style), &HttpFileSource::default()).expect("resolves");

    assert!(matches!(document, std::borrow::Cow::Owned(_)));
    assert_eq!(server.paths(), ["/features.geojson"], "once");

    let features = tessella_source::geojson::read(&document).expect("reads");
    assert_eq!(features.len(), 2);
    assert_eq!(features[0].geometry.type_name(), "Polygon");
    assert_eq!(features[1].geometry.type_name(), "LineString");
}

/// A missing document is an error naming the URL and the status.
#[test]
fn a_missing_document_is_an_error() {
    let server = served(DOCUMENT);
    let style = style_with(&format!(r#""{}/no-such-file.geojson""#, server.origin()));
    match resolve(source(&style), &HttpFileSource::default()) {
        Err(GeoJsonSourceError::Status { url, status }) => {
            assert!(url.ends_with("/no-such-file.geojson"), "{url}");
            assert_eq!(status, 404);
        }
        other => panic!("{other:?}"),
    }
}

/// An empty body is its own error, not a parse failure.
///
/// A tile that is empty is an ordinary tile. A *source* that is empty is a style that will draw
/// nothing and never say why, which is why mbgl calls this "unexpectedly empty GeoJSON" and
/// treats it as an error rather than as no features.
#[test]
fn an_empty_document_is_its_own_error() {
    let server = tile_server::Server::start(tile_server::Routes::new().at(
        "/features.geojson",
        "application/json",
        Vec::new(),
    ))
    .expect("binds");
    let style = style_with(&format!(r#""{}/features.geojson""#, server.origin()));
    match resolve(source(&style), &HttpFileSource::default()) {
        Err(GeoJsonSourceError::Empty { url }) => assert!(url.ends_with("/features.geojson")),
        other => panic!("{other:?}"),
    }
}

/// A document that is not JSON is reported, not substituted with nothing.
///
/// mbgl logs and carries on with an empty source, because its tiles are waiting on a callback
/// that has to fire. Nothing here is waiting, so the failure is returned and the caller may
/// choose mbgl's behaviour — the reverse is not available to a caller handed an empty source.
#[test]
fn a_malformed_document_is_reported() {
    let server = served("this is not json");
    let style = style_with(&format!(r#""{}/features.geojson""#, server.origin()));
    match resolve(source(&style), &HttpFileSource::default()) {
        Err(GeoJsonSourceError::Malformed { url, .. }) => {
            assert!(url.ends_with("/features.geojson"), "{url}");
        }
        other => panic!("{other:?}"),
    }
}

/// A dead origin is a fetch failure, distinct from a status or a parse one.
#[test]
fn a_dead_origin_is_a_fetch_failure() {
    let origin = {
        let server = served(DOCUMENT);
        server.origin()
    };
    let style = style_with(&format!(r#""{origin}/features.geojson""#));
    match resolve(source(&style), &HttpFileSource::default()) {
        Err(GeoJsonSourceError::Fetch { url, .. }) => assert!(url.starts_with(&origin), "{url}"),
        other => panic!("{other:?}"),
    }
}
