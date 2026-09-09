//! Deciding what to fetch, separately from fetching it.
//!
//! The half a caller that cannot block needs first. `resolve` answers "here is the tileset" and
//! does whatever that takes; `plan` answers "this will take a manifest, at this URL" or "it will
//! take nothing at all", which is a question that has to be answerable a round trip earlier.

use tessella_storage::source::Response;
use tessella_storage::tileset::{self, Planned};
use tessella_style::TileSource;

fn source(json: &str) -> TileSource {
    serde_json::from_str(json).expect("the fixture parses")
}

fn manifest(body: &str) -> Response {
    Response {
        status: 200,
        body: body.as_bytes().to_vec(),
        ..Response::default()
    }
}

#[test]
fn a_style_that_lists_its_tiles_needs_no_manifest() {
    let source = source(
        r#"{"type": "vector", "tiles": ["https://o.invalid/{z}/{x}/{y}.pbf"],
            "minzoom": 2, "maxzoom": 9}"#,
    );
    match tileset::plan(&source).expect("addressable") {
        Planned::Ready(set) => {
            assert_eq!(set.templates, vec!["https://o.invalid/{z}/{x}/{y}.pbf"]);
            assert_eq!(set.zooms.min, 2);
            assert_eq!(set.zooms.max, 9);
        }
        Planned::Manifest(url) => panic!("an inline source asked for {url}"),
    }
}

#[test]
fn a_style_with_only_a_url_names_the_manifest_to_fetch() {
    let source = source(r#"{"type": "vector", "url": "https://o.invalid/tiles.json"}"#);
    assert_eq!(
        tileset::plan(&source).expect("addressable"),
        Planned::Manifest("https://o.invalid/tiles.json".to_string())
    );
}

#[test]
fn a_source_naming_neither_is_unaddressable_before_anything_is_fetched() {
    let source = source(r#"{"type": "vector"}"#);
    assert!(tileset::plan(&source).is_err());
}

#[test]
fn accepting_a_manifest_lets_the_style_narrow_it() {
    // The rule `resolve` carries and `accept` has to keep: a style may narrow a source it does
    // not own, and where the style is silent the manifest decides.
    let source = source(r#"{"type": "vector", "url": "https://o.invalid/t.json", "maxzoom": 9}"#);
    let set = tileset::accept(
        &source,
        "https://o.invalid/t.json",
        &manifest(
            r#"{"tiles": ["https://o.invalid/{z}/{x}/{y}.pbf"], "minzoom": 1, "maxzoom": 14}"#,
        ),
    )
    .expect("resolves");

    assert_eq!(set.zooms.min, 1, "the manifest's minzoom was overridden");
    assert_eq!(set.zooms.max, 9, "the style's maxzoom did not win");
}

#[test]
fn a_manifest_that_lists_no_tiles_is_malformed_rather_than_empty() {
    let source = source(r#"{"type": "vector", "url": "https://o.invalid/t.json"}"#);
    assert!(
        tileset::accept(
            &source,
            "https://o.invalid/t.json",
            &manifest(r#"{"tiles": []}"#)
        )
        .is_err()
    );
}
