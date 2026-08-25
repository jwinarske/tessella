//! A `.pmtiles` archive named by a style, resolved and fetched like any other source.

#![cfg(feature = "pmtiles")]

use tessella_storage::pmtiles::Archive;
use tessella_storage::pmtiles::source::{PmtilesFileSource, accepts};
use tessella_storage::source::FileSource;
use tessella_storage::{Router, tileset};

/// The archive the live tests use, when it is present. Two hundred megabytes is not something
/// to vendor, so these skip when it is absent.
fn archive_path() -> Option<String> {
    let path = "/mnt/dev/maplibre-frontend/tileserver/berlin_z15.pmtiles";
    std::path::Path::new(path).exists().then(|| path.to_owned())
}

fn source_url() -> Option<String> {
    archive_path().map(|path| format!("pmtiles://{path}"))
}

/// A style source given only a `pmtiles://` url must resolve to templates and a zoom range,
/// with no `.json` alongside the archive and no server in front of it.
#[test]
fn a_style_source_resolves_from_the_archive_alone() {
    let Some(url) = source_url() else {
        return;
    };
    let files = PmtilesFileSource::new();
    let source = tessella_style::TileSource {
        url: Some(url.clone()),
        ..tessella_style::TileSource::default()
    };

    let set = tileset::resolve(&source, &files).expect("resolves");

    assert_eq!(set.templates, [format!("{url}/{{z}}/{{x}}/{{y}}")]);
    // The archive's own range, not the style spec's 0..22 default, which is what a reader that
    // ignored the header would fall back to — and would then fetch a hundred tiles per frame
    // that the archive does not hold.
    let header = *Archive::open(std::fs::File::open(archive_path().expect("path")).expect("open"))
        .expect("archive")
        .header();
    assert_eq!(set.zooms.min, header.min_zoom);
    assert_eq!(set.zooms.max, header.max_zoom);
    // A v3 archive has no other scheme, so the manifest states it rather than leaving the
    // reader to guess: guessing TMS here would flip every row.
    assert_eq!(set.scheme, tessella_storage::url::Scheme::Xyz);
}

/// The whole round trip: resolve the source, expand its template for a tile, fetch that URL,
/// and get exactly the bytes the archive holds for it.
#[test]
fn the_expanded_template_fetches_the_right_tile() {
    let Some(url) = source_url() else {
        return;
    };
    let files = PmtilesFileSource::new();
    let source = tessella_style::TileSource {
        url: Some(url),
        ..tessella_style::TileSource::default()
    };
    let set = tileset::resolve(&source, &files).expect("resolves");

    let archive = Archive::open(std::fs::File::open(archive_path().expect("path")).expect("open"))
        .expect("archive");

    // A tile at the root of the archive, one deep enough to need a leaf directory, and one off
    // the edge of the covered area.
    for (z, x, y) in [(0, 0, 0), (14, 8802, 5373)] {
        let tile_url = set.url_for(z, x, y, 1.0).expect("a template");
        let response = files.fetch(&tile_url).expect("fetch");
        assert_eq!(response.status, 200, "{tile_url}");
        assert_eq!(
            response.body,
            archive.tile(z, x, y).expect("read").expect("present"),
            "{tile_url}"
        );
    }

    // Off the edge is a 404 and not an error, the same as it would be from an origin.
    let absent = set.url_for(14, 0, 0, 1.0).expect("a template");
    let response = files.fetch(&absent).expect("fetch");
    assert_eq!(response.status, 404);
    assert!(response.body.is_empty());
}

/// Nothing here reports freshness, because an archive on local storage is already the cache.
/// A `max-age` would send tiles into the SQLite cache and spend disk making reads slower.
#[test]
fn a_local_archive_states_no_freshness() {
    let Some(url) = source_url() else {
        return;
    };
    let files = PmtilesFileSource::new();
    let response = files.fetch(&format!("{url}/0/0/0")).expect("fetch");
    assert_eq!(response.etag, None);
    assert_eq!(response.max_age, None);
    assert_eq!(response.expires_at, None);
}

/// The point of the router: one style naming an archive for its tiles and an origin for
/// everything else, with each URL reaching the source that can answer it.
#[test]
fn the_router_sends_each_url_to_its_own_source() {
    let Some(url) = source_url() else {
        return;
    };

    struct Elsewhere;
    impl FileSource for Elsewhere {
        fn fetch(
            &self,
            url: &str,
        ) -> Result<tessella_storage::Response, tessella_storage::FetchError> {
            Ok(tessella_storage::Response {
                status: 200,
                body: url.as_bytes().to_vec(),
                ..tessella_storage::Response::default()
            })
        }
    }

    let files = Router::new()
        .route(accepts, PmtilesFileSource::new())
        .otherwise(Elsewhere);

    let tile = files.fetch(&format!("{url}/0/0/0")).expect("fetch");
    assert_eq!(tile.status, 200);
    assert!(!tile.body.is_empty());

    let glyphs = files
        .fetch("https://example.com/fonts/Noto/0-255.pbf")
        .expect("fetch");
    assert_eq!(glyphs.body, b"https://example.com/fonts/Noto/0-255.pbf");
}

/// A URL nothing claimed is a configuration fault, not an edge of coverage. Reporting it as a
/// 404 would let a missing route be absorbed silently as "the origin does not have that tile".
#[test]
fn an_unrouted_url_is_an_error_and_not_a_404() {
    let files = Router::new().route(accepts, PmtilesFileSource::new());
    let error = files
        .fetch("https://example.com/x.json")
        .expect_err("no route");
    assert!(
        format!("{error}").contains("no source is configured"),
        "{error}"
    );
}
