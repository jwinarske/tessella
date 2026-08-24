//! Downloading a region end to end, against a real server.

#![cfg(all(feature = "cache", feature = "http"))]

use std::sync::atomic::{AtomicBool, Ordering};

use tessella_storage::cache::{CachingFileSource, SqliteCache};
use tessella_storage::download::{Download, DownloadError, Progress};
use tessella_storage::http::HttpFileSource;
use tessella_storage::offline::Region;
use tessella_tile::cover::Bounds;

const TILE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/real-world-0-0-0.mvt");
const NOW: i64 = 1_000_000;

// A clock the tests move, so an entry can be aged past what the origin allowed. Thread-local
// rather than static: cargo runs these in parallel, and one test winding time forward would
// otherwise expire another's entries out from under it.
thread_local! {
    static CLOCK: std::cell::Cell<i64> = const { std::cell::Cell::new(NOW) };
}
fn clock() -> i64 {
    CLOCK.with(std::cell::Cell::get)
}
fn set_clock(value: i64) {
    CLOCK.with(|clock| clock.set(value));
}

/// A style whose sources are inline, so the only network is tiles and assets.
fn style(origin: &str) -> tessella_style::Style {
    serde_json::from_str(&format!(
        r##"{{
          "version": 8,
          "sprite": "{origin}/sprite",
          "glyphs": "{origin}/fonts/{{fontstack}}/{{range}}.pbf",
          "sources": {{
            "base": {{ "type": "vector",
                       "tiles": ["{origin}/{{z}}/{{x}}/{{y}}.mvt"],
                       "minzoom": 0, "maxzoom": 14 }}
          }},
          "layers": [
            {{ "id": "labels", "type": "symbol", "source": "base",
               "layout": {{ "text-font": ["Noto Sans Regular"] }} }}
          ]
        }}"##
    ))
    .expect("a style")
}

/// One tile at zooms 4 and 5.
fn region(origin: &str) -> Region {
    Region {
        style_url: format!("{origin}/style.json"),
        bounds: Bounds::new(13.40, 52.51, 13.41, 52.52),
        min_zoom: 4.0,
        max_zoom: 5.0,
        pixel_ratio: 1.0,
        include_ideographs: false,
    }
}

fn routes() -> tile_server::Routes {
    let mut routes = tile_server::Routes::new()
        .at("/style.json", "application/json", b"{}".to_vec())
        .at("/sprite.json", "application/json", b"{}".to_vec())
        .at("/sprite.png", "image/png", b"png".to_vec())
        .at("/sprite@2x.json", "application/json", b"{}".to_vec())
        .at("/sprite@2x.png", "image/png", b"png".to_vec());
    for range in 0..5u32 {
        let start = range * 256;
        routes = routes.at(
            &format!("/fonts/Noto%20Sans%20Regular/{start}-{}.pbf", start + 255),
            "application/octet-stream",
            b"glyphs".to_vec(),
        );
    }
    routes.tiles(TILE.to_vec(), Some((0, 14)))
}

/// The style, both sprite densities, five glyph ranges, two tiles: eleven resources.
const EXPECTED: u64 = 1 + 4 + 5 + 2;

/// A download fetches everything and reports itself complete.
#[test]
fn a_download_gets_everything_the_style_needs() {
    let server = tile_server::Server::start(routes()).expect("binds");
    let origin = server.origin();
    let cache = SqliteCache::in_memory_with_capacity(1_000_000).expect("opens");
    let region = region(&origin);
    let id = cache
        .create_region(&region, Some("Berlin"), NOW)
        .expect("creates");

    let mut seen: Vec<Progress> = Vec::new();
    let summary = Download {
        cache: &cache,
        files: &HttpFileSource::default(),
        region: id,
        definition: &region,
        now: NOW,
    }
    .all(&style(&origin), &AtomicBool::new(false), &mut |progress| {
        seen.push(progress)
    })
    .expect("downloads");

    assert!(!summary.cancelled);
    assert_eq!(summary.fetched, EXPECTED);
    assert_eq!(summary.missing, 0);
    assert_eq!(summary.progress.required_resources, EXPECTED);
    assert!(summary.progress.required_precise);
    assert_eq!(summary.progress.completed_resources, EXPECTED);
    assert_eq!(summary.progress.fraction(), Some(1.0));

    // The bar only ever moves forwards, which is the property a user actually perceives.
    assert_eq!(seen.len() as u64, EXPECTED);
    for pair in seen.windows(2) {
        assert!(pair[1].completed_resources >= pair[0].completed_resources);
    }
}

/// Assets come before tiles, so a download stopped halfway still has a style to draw with.
#[test]
fn assets_are_fetched_before_tiles() {
    let server = tile_server::Server::start(routes()).expect("binds");
    let origin = server.origin();
    let cache = SqliteCache::in_memory_with_capacity(1_000_000).expect("opens");
    let region = region(&origin);
    let id = cache.create_region(&region, None, NOW).expect("creates");

    Download {
        cache: &cache,
        files: &HttpFileSource::default(),
        region: id,
        definition: &region,
        now: NOW,
    }
    .all(&style(&origin), &AtomicBool::new(false), &mut |_| {})
    .expect("downloads");

    let paths = server.paths();
    let first_tile = paths
        .iter()
        .position(|path| path.ends_with(".mvt"))
        .expect("a tile was fetched");
    let last_asset = paths
        .iter()
        .rposition(|path| !path.ends_with(".mvt"))
        .expect("an asset was fetched");
    assert!(
        last_asset < first_tile,
        "every asset before every tile: {paths:?}"
    );
    assert_eq!(paths[0], "/style.json", "the style first of all");
}

/// Cancelling stops where it is and keeps what it got.
///
/// A country at street zoom is hours over a connection that will drop. Discarding the work on
/// an interruption means the user starts again, which for a large region means never finishing.
#[test]
fn cancelling_keeps_what_it_already_stored() {
    let server = tile_server::Server::start(routes()).expect("binds");
    let origin = server.origin();
    let cache = SqliteCache::in_memory_with_capacity(1_000_000).expect("opens");
    let region = region(&origin);
    let id = cache.create_region(&region, None, NOW).expect("creates");

    let cancel = AtomicBool::new(false);
    let mut count = 0u64;
    let summary = Download {
        cache: &cache,
        files: &HttpFileSource::default(),
        region: id,
        definition: &region,
        now: NOW,
    }
    .all(&style(&origin), &cancel, &mut |_| {
        count += 1;
        if count == 3 {
            cancel.store(true, Ordering::Relaxed);
        }
    })
    .expect("stops cleanly");

    assert!(summary.cancelled);
    assert_eq!(summary.progress.completed_resources, 3);
    assert!(summary.progress.fraction() < Some(1.0));
    assert_eq!(
        cache
            .region_progress(id)
            .expect("reads")
            .completed_resources,
        3
    );
}

/// Resuming fetches only what is missing.
#[test]
fn resuming_pays_only_for_the_remainder() {
    let server = tile_server::Server::start(routes()).expect("binds");
    let origin = server.origin();
    let cache = SqliteCache::in_memory_with_capacity(1_000_000).expect("opens");
    let region = region(&origin);
    let id = cache.create_region(&region, None, NOW).expect("creates");

    let cancel = AtomicBool::new(false);
    let mut count = 0u64;
    Download {
        cache: &cache,
        files: &HttpFileSource::default(),
        region: id,
        definition: &region,
        now: NOW,
    }
    .all(&style(&origin), &cancel, &mut |_| {
        count += 1;
        if count == 4 {
            cancel.store(true, Ordering::Relaxed);
        }
    })
    .expect("stops");

    let after_cancel = server.requests();
    let summary = Download {
        cache: &cache,
        files: &HttpFileSource::default(),
        region: id,
        definition: &region,
        now: NOW,
    }
    .all(&style(&origin), &AtomicBool::new(false), &mut |_| {})
    .expect("resumes");

    assert!(!summary.cancelled);
    assert_eq!(summary.progress.completed_resources, EXPECTED);
    assert_eq!(summary.fetched, EXPECTED - 4, "only the remainder");
    assert_eq!(
        server.requests() - after_cancel,
        EXPECTED - 4,
        "and only that many round trips"
    );
}

/// A tile the origin does not have is an edge, not a failure.
///
/// A source's coverage is not a rectangle. A region over a coastline asks for tiles that are
/// sea, and a download that treated 404 as an error could never complete one.
#[test]
fn a_missing_tile_does_not_fail_the_region() {
    // A source that only has tiles at zoom 4, so the zoom-5 tile 404s.
    let routes = routes().tiles(TILE.to_vec(), Some((4, 4)));
    let server = tile_server::Server::start(routes).expect("binds");
    let origin = server.origin();
    let cache = SqliteCache::in_memory_with_capacity(1_000_000).expect("opens");
    let region = region(&origin);
    let id = cache.create_region(&region, None, NOW).expect("creates");

    let summary = Download {
        cache: &cache,
        files: &HttpFileSource::default(),
        region: id,
        definition: &region,
        now: NOW,
    }
    .all(&style(&origin), &AtomicBool::new(false), &mut |_| {})
    .expect("completes despite the hole");

    assert_eq!(summary.missing, 1);
    assert_eq!(
        summary.progress.fraction(),
        Some(1.0),
        "a region over the sea still reaches a hundred percent"
    );

    // And a second run does not ask the sea again.
    let before = server.requests();
    Download {
        cache: &cache,
        files: &HttpFileSource::default(),
        region: id,
        definition: &region,
        now: NOW,
    }
    .all(&style(&origin), &AtomicBool::new(false), &mut |_| {})
    .expect("resumes");
    assert_eq!(server.requests(), before, "nothing left to ask for");
}

/// A server error is not an absence, and is not recorded as one.
///
/// Silently marking a 500 as done leaves the region permanently short of a resource it will
/// never retry — a map with a hole the user cannot fix by downloading again.
#[test]
fn a_server_error_stops_the_download() {
    let routes = routes().at_status("/sprite.png", 500);
    let server = tile_server::Server::start(routes).expect("binds");
    let origin = server.origin();
    let cache = SqliteCache::in_memory_with_capacity(1_000_000).expect("opens");
    let region = region(&origin);
    let id = cache.create_region(&region, None, NOW).expect("creates");

    let error = Download {
        cache: &cache,
        files: &HttpFileSource::default(),
        region: id,
        definition: &region,
        now: NOW,
    }
    .all(&style(&origin), &AtomicBool::new(false), &mut |_| {})
    .expect_err("reports the status");
    assert!(matches!(error, DownloadError::Status { status: 500, .. }));

    // And what did arrive is kept, so retrying is short.
    assert!(
        cache
            .region_progress(id)
            .expect("reads")
            .completed_resources
            > 0
    );
}

/// A region over ground the user has already seen downloads less.
#[test]
fn a_download_claims_what_the_ambient_cache_holds() {
    let server = tile_server::Server::start(routes()).expect("binds");
    let origin = server.origin();
    let cache = SqliteCache::in_memory_with_capacity(1_000_000).expect("opens");
    let region = region(&origin);

    // The user looked at this tile before deciding to download the area.
    let seen = format!("{origin}/4/8/5.mvt");
    let response = tessella_storage::source::FileSource::fetch(&HttpFileSource::default(), &seen)
        .expect("fetches");
    cache.put(&seen, &response, NOW).expect("stores");
    let before = server.requests();

    let id = cache.create_region(&region, None, NOW).expect("creates");
    let summary = Download {
        cache: &cache,
        files: &HttpFileSource::default(),
        region: id,
        definition: &region,
        now: NOW,
    }
    .all(&style(&origin), &AtomicBool::new(false), &mut |_| {})
    .expect("downloads");

    assert_eq!(summary.fetched, EXPECTED - 1, "the held tile was claimed");
    assert_eq!(server.requests() - before, EXPECTED - 1);
    assert_eq!(summary.progress.completed_resources, EXPECTED);
}

/// A user is shown the size before the download starts, and the same plan is what runs.
///
/// This is the shape the product needs: pick an area, see what it costs, decline or accept.
/// Planning and running against separately-derived lists would make the number shown a
/// different number from the one paid.
#[test]
fn a_plan_can_be_shown_before_it_is_run() {
    let server = tile_server::Server::start(routes()).expect("binds");
    let origin = server.origin();
    let cache = SqliteCache::in_memory_with_capacity(1_000_000).expect("opens");
    let region = region(&origin);
    let id = cache
        .create_region(&region, Some("Berlin"), NOW)
        .expect("creates");

    let download = Download {
        cache: &cache,
        files: &HttpFileSource::default(),
        region: id,
        definition: &region,
        now: NOW,
    };

    let plan = download.plan(&style(&origin)).expect("plans");
    assert!(plan.complete, "a number worth showing a user");
    assert_eq!(plan.len() as u64 + 1, EXPECTED);
    let requests_to_plan = server.requests();

    let summary = download
        .run(&plan, &AtomicBool::new(false), &mut |_| {})
        .expect("runs");
    assert_eq!(summary.progress.required_resources, EXPECTED);
    assert_eq!(summary.progress.completed_resources, EXPECTED);
    assert_eq!(
        server.requests() - requests_to_plan,
        EXPECTED,
        "what was shown is what was paid"
    );
}

/// A region the user declines costs nothing but the planning.
#[test]
fn declining_a_plan_downloads_nothing() {
    let server = tile_server::Server::start(routes()).expect("binds");
    let origin = server.origin();
    let cache = SqliteCache::in_memory_with_capacity(1_000_000).expect("opens");
    let region = region(&origin);
    let id = cache.create_region(&region, None, NOW).expect("creates");

    Download {
        cache: &cache,
        files: &HttpFileSource::default(),
        region: id,
        definition: &region,
        now: NOW,
    }
    .plan(&style(&origin))
    .expect("plans");

    // Sources are inline here, so planning needed no network at all.
    assert_eq!(server.requests(), 0);
    assert_eq!(cache.region_size().expect("reads"), 0);
}

/// A downloaded region is served without asking the origin, however old it gets.
///
/// This is what the user actually bought. Deferring to `max-age` here undoes it twice over:
/// with no network the map goes blank the first time an hour runs out, which is precisely the
/// situation the region exists for; with a network every tile costs a revalidation round trip,
/// so a download taken to avoid a metered connection puts the user straight back on it.
#[test]
fn a_downloaded_region_outlives_its_cache_headers() {
    set_clock(NOW);
    let routes = routes().cache_control("max-age=60");
    let server = tile_server::Server::start(routes).expect("binds");
    let origin = server.origin();

    let region = region(&origin);
    let caching = CachingFileSource::with_clock(
        HttpFileSource::default(),
        SqliteCache::in_memory_with_capacity(1_000_000).expect("opens"),
        clock,
    );
    let id = caching
        .cache()
        .create_region(&region, Some("Berlin"), NOW)
        .expect("creates");

    Download {
        cache: caching.cache(),
        files: &HttpFileSource::default(),
        region: id,
        definition: &region,
        now: NOW,
    }
    .all(&style(&origin), &AtomicBool::new(false), &mut |_| {})
    .expect("downloads");

    // A week later, long past the minute the origin allowed.
    set_clock(NOW + 7 * 24 * 3600);
    let before = server.requests();
    let tile = format!("{origin}/4/8/5.mvt");
    let response = tessella_storage::source::FileSource::fetch(&caching, &tile).expect("serves");

    assert_eq!(response.status, 200);
    assert_eq!(response.body, TILE);
    assert_eq!(
        server.requests(),
        before,
        "no round trip: the user already paid for this"
    );
}

/// An ambient copy of the same age does go back to the origin.
///
/// The contrast is the point — pinning changes the answer, rather than the cache having simply
/// stopped honouring `max-age`.
#[test]
fn an_unclaimed_copy_still_revalidates() {
    set_clock(NOW);
    let routes = routes().cache_control("max-age=60");
    let server = tile_server::Server::start(routes).expect("binds");
    let origin = server.origin();

    let caching = CachingFileSource::with_clock(
        HttpFileSource::default(),
        SqliteCache::in_memory_with_capacity(1_000_000).expect("opens"),
        clock,
    );
    let tile = format!("{origin}/4/8/5.mvt");
    tessella_storage::source::FileSource::fetch(&caching, &tile).expect("fetches");

    set_clock(NOW + 7 * 24 * 3600);
    let before = server.requests();
    tessella_storage::source::FileSource::fetch(&caching, &tile).expect("fetches");
    assert!(
        server.requests() > before,
        "an ordinary cached tile still asks"
    );
}
