//! Bringing a downloaded region up to date, against a real origin that changes.

#![cfg(all(feature = "cache", feature = "http"))]

use std::sync::atomic::AtomicBool;

use tessella_storage::cache::{RegionId, SqliteCache};
use tessella_storage::download::Download;
use tessella_storage::http::HttpFileSource;
use tessella_storage::offline::{Area, Plan, Region};
use tessella_tile::cover::Bounds;

const TILE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/real-world-0-0-0.mvt");
const NOW: i64 = 1_000_000;
const LATER: i64 = NOW + 7 * 24 * 3600;

fn style(origin: &str) -> tessella_style::Style {
    serde_json::from_str(&format!(
        r##"{{
          "version": 8,
          "sources": {{
            "base": {{ "type": "vector",
                       "tiles": ["{origin}/{{z}}/{{x}}/{{y}}.mvt"],
                       "minzoom": 0, "maxzoom": 14 }}
          }},
          "layers": []
        }}"##
    ))
    .expect("a style")
}

fn region(origin: &str, max_zoom: f64) -> Region {
    Region {
        style_url: format!("{origin}/style.json"),
        area: Area::Box(Bounds::new(13.40, 52.51, 13.41, 52.52)),
        min_zoom: 4.0,
        max_zoom,
        pixel_ratio: 1.0,
        include_ideographs: false,
    }
}

fn routes() -> tile_server::Routes {
    tile_server::Routes::new()
        .at("/style.json", "application/json", b"{}".to_vec())
        .tiles(TILE.to_vec(), Some((0, 14)))
}

struct Fixture {
    server: tile_server::Server,
    cache: SqliteCache,
    files: HttpFileSource,
    region: Region,
    id: RegionId,
    plan: Plan,
}

impl Fixture {
    fn new(max_zoom: f64) -> Self {
        let server = tile_server::Server::start(routes()).expect("binds");
        let origin = server.origin();
        let cache = SqliteCache::in_memory_with_capacity(8_000_000).expect("opens");
        let files = HttpFileSource::default();
        let region = region(&origin, max_zoom);
        let id = cache
            .create_region(&region, Some("Berlin"), NOW)
            .expect("creates");
        let plan = Download {
            cache: &cache,
            files: &files,
            region: id,
            definition: &region,
            now: NOW,
        }
        .plan(&style(&origin))
        .expect("plans");
        Self {
            server,
            cache,
            files,
            region,
            id,
            plan,
        }
    }

    fn download(&self) -> tessella_storage::download::Summary {
        self.at(NOW)
            .run(&self.plan, &AtomicBool::new(false), &mut |_| {})
            .expect("downloads")
    }

    fn refresh(&self, now: i64) -> tessella_storage::download::Summary {
        self.at(now)
            .refresh(&self.plan, &AtomicBool::new(false), &mut |_| {})
            .expect("refreshes")
    }

    fn at(&self, now: i64) -> Download<'_> {
        Download {
            cache: &self.cache,
            files: &self.files,
            region: self.id,
            definition: &self.region,
            now,
        }
    }
}

/// A refresh of an unchanged region transfers nothing.
///
/// The whole reason a refresh is affordable. Re-downloading a region costs its size again, over
/// exactly the connection the region exists to avoid needing; revalidating costs a round trip
/// per resource and no bytes.
#[test]
fn an_unchanged_region_costs_round_trips_and_no_bytes() {
    let fixture = Fixture::new(6.0);
    let downloaded = fixture.download();
    assert!(downloaded.fetched > 3, "something to refresh");
    let held = fixture
        .cache
        .region_progress(fixture.id)
        .expect("reads")
        .completed_bytes;

    let refreshed = fixture.refresh(LATER);

    assert_eq!(refreshed.fetched, 0, "nothing was re-transferred");
    assert_eq!(
        refreshed.unchanged,
        downloaded.fetched + downloaded.missing,
        "every resource was confirmed"
    );
    assert_eq!(
        fixture
            .cache
            .region_progress(fixture.id)
            .expect("reads")
            .completed_bytes,
        held,
        "and the region holds exactly what it did"
    );
}

/// A download run twice would not have noticed. A refresh does.
///
/// This is the gap. mbgl's offline download treats a held resource as done, so re-running it
/// fills holes and changes nothing else — a region stays a snapshot of the day it was taken.
#[test]
fn a_download_is_a_snapshot_and_a_refresh_is_not() {
    let fixture = Fixture::new(6.0);
    fixture.download();

    // The origin re-cuts its tiles. A different length gives a different etag.
    let mut changed = TILE.to_vec();
    changed.extend_from_slice(b"and then some");
    fixture
        .server
        .set_routes(routes().tiles(changed.clone(), Some((0, 14))));

    // Running the download again notices nothing.
    let again = fixture
        .at(LATER)
        .run(&fixture.plan, &AtomicBool::new(false), &mut |_| {})
        .expect("runs");
    assert_eq!(again.fetched, 0, "a download accepts what it holds");
    let tile = &fixture.plan.tiles[0];
    assert_eq!(
        fixture
            .cache
            .get(tile, LATER)
            .expect("reads")
            .expect("held")
            .response
            .body,
        TILE,
        "still the old body"
    );

    // Refreshing does.
    let refreshed = fixture.refresh(LATER);
    assert!(refreshed.fetched > 0, "it re-fetched what changed");
    assert_eq!(
        fixture
            .cache
            .get(tile, LATER)
            .expect("reads")
            .expect("held")
            .response
            .body,
        changed,
        "and replaced the body"
    );
}

/// A resource the origin has dropped stops being held.
///
/// The uncomfortable half. Keeping the old body would be easier and would leave a user seeing a
/// road that has been removed, with nothing to tell them their map is out of date.
#[test]
fn a_resource_the_origin_dropped_is_dropped() {
    let fixture = Fixture::new(6.0);
    fixture.download();
    let tile = fixture.plan.tiles[0].clone();
    let path = tile
        .strip_prefix(&fixture.server.origin())
        .expect("a path")
        .to_string();
    assert!(
        !fixture
            .cache
            .get(&tile, NOW)
            .expect("reads")
            .expect("held")
            .response
            .body
            .is_empty()
    );

    fixture.server.set_routes(
        routes()
            .tiles(TILE.to_vec(), Some((0, 14)))
            .at_status(&path, 404),
    );

    let refreshed = fixture.refresh(LATER);
    assert!(refreshed.missing > 0);
    assert!(
        fixture
            .cache
            .get(&tile, LATER)
            .expect("reads")
            .expect("still a row")
            .response
            .body
            .is_empty(),
        "the body is gone, and the absence is recorded"
    );
}

/// A refresh whose plan shrank releases what it no longer needs.
///
/// A style drops a layer, a source lowers its maximum zoom, a user redraws an area smaller.
/// Without this those resources stay pinned for the life of the region — outside the ambient
/// bound, never evicted, never used. A leak with a user-visible size.
#[test]
fn a_shrunken_plan_releases_its_orphans() {
    let fixture = Fixture::new(8.0);
    fixture.download();
    let before = fixture
        .cache
        .region_progress(fixture.id)
        .expect("reads")
        .completed_resources;

    // The same region, asked for fewer zooms.
    let mut smaller = fixture.region.clone();
    smaller.max_zoom = 5.0;
    let smaller_plan = Download {
        cache: &fixture.cache,
        files: &fixture.files,
        region: fixture.id,
        definition: &smaller,
        now: LATER,
    }
    .plan(&style(&fixture.server.origin()))
    .expect("plans");
    assert!(smaller_plan.tiles.len() < fixture.plan.tiles.len());

    let refreshed = Download {
        cache: &fixture.cache,
        files: &fixture.files,
        region: fixture.id,
        definition: &smaller,
        now: LATER,
    }
    .refresh(&smaller_plan, &AtomicBool::new(false), &mut |_| {})
    .expect("refreshes");

    assert!(refreshed.released > 0, "orphans were released");
    let after = fixture
        .cache
        .region_progress(fixture.id)
        .expect("reads")
        .completed_resources;
    assert_eq!(after, smaller_plan.len() as u64 + 1);
    assert!(after < before);
    assert_eq!(before - after, refreshed.released);
}

/// A cancelled refresh releases nothing.
///
/// It has not visited every URL, so what looks orphaned may simply not have been reached.
/// Pruning there would turn an interrupted refresh into a partial delete — which for a region
/// downloaded over hours is the worst thing that could happen to it.
#[test]
fn a_cancelled_refresh_does_not_prune() {
    let fixture = Fixture::new(8.0);
    fixture.download();
    let before = fixture
        .cache
        .region_progress(fixture.id)
        .expect("reads")
        .completed_resources;

    let mut smaller = fixture.region.clone();
    smaller.max_zoom = 5.0;
    let smaller_plan = Download {
        cache: &fixture.cache,
        files: &fixture.files,
        region: fixture.id,
        definition: &smaller,
        now: LATER,
    }
    .plan(&style(&fixture.server.origin()))
    .expect("plans");

    let cancel = AtomicBool::new(false);
    let mut seen = 0u64;
    let refreshed = Download {
        cache: &fixture.cache,
        files: &fixture.files,
        region: fixture.id,
        definition: &smaller,
        now: LATER,
    }
    .refresh(&smaller_plan, &cancel, &mut |_| {
        seen += 1;
        if seen == 2 {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    })
    .expect("stops cleanly");

    assert!(refreshed.cancelled);
    assert_eq!(refreshed.released, 0, "nothing was pruned");
    assert_eq!(
        fixture
            .cache
            .region_progress(fixture.id)
            .expect("reads")
            .completed_resources,
        before,
        "the region still holds everything it did"
    );
}

/// A resource the plan gained since the download is fetched, not revalidated.
#[test]
fn a_new_resource_is_fetched() {
    let fixture = Fixture::new(5.0);
    fixture.download();

    let mut larger = fixture.region.clone();
    larger.max_zoom = 7.0;
    let larger_plan = Download {
        cache: &fixture.cache,
        files: &fixture.files,
        region: fixture.id,
        definition: &larger,
        now: LATER,
    }
    .plan(&style(&fixture.server.origin()))
    .expect("plans");
    let added = larger_plan.tiles.len() - fixture.plan.tiles.len();
    assert!(added > 0);

    let refreshed = Download {
        cache: &fixture.cache,
        files: &fixture.files,
        region: fixture.id,
        definition: &larger,
        now: LATER,
    }
    .refresh(&larger_plan, &AtomicBool::new(false), &mut |_| {})
    .expect("refreshes");

    assert_eq!(refreshed.fetched as usize, added, "only the new ones");
    assert_eq!(refreshed.released, 0, "and nothing orphaned");
}
