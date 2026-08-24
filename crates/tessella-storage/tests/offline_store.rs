//! What a downloaded region has to survive: the ambient cache filling up around it.

#![cfg(feature = "cache")]

use tessella_storage::cache::SqliteCache;
use tessella_storage::offline::Region;
use tessella_storage::source::Response;
use tessella_tile::cover::Bounds;

fn berlin() -> Region {
    Region {
        style_url: "https://host/style.json".into(),
        bounds: Bounds::new(13.0, 52.3, 13.8, 52.7),
        min_zoom: 0.0,
        max_zoom: 12.0,
        pixel_ratio: 2.0,
        include_ideographs: false,
    }
}

fn body(bytes: usize) -> Response {
    Response {
        status: 200,
        body: vec![0xab; bytes],
        ..Response::default()
    }
}

/// A region round-trips through the store with the numbers it was given.
///
/// The zoom range and pixel ratio are what a resumed download re-enumerates from, so a region
/// that came back subtly different would fetch a different set of tiles the second time.
#[test]
fn a_region_round_trips() {
    let cache = SqliteCache::in_memory().expect("opens");
    let id = cache
        .create_region(&berlin(), Some("Berlin"), 1_000)
        .expect("creates");

    let stored = cache.regions().expect("lists");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].id, id);
    assert_eq!(stored[0].region, berlin());
    assert_eq!(stored[0].description.as_deref(), Some("Berlin"));
    assert_eq!(stored[0].created, 1_000);
}

/// A region exists before it has anything, so a download can be resumed and cancelled.
#[test]
fn a_new_region_is_empty_rather_than_absent() {
    let cache = SqliteCache::in_memory().expect("opens");
    let id = cache.create_region(&berlin(), None, 0).expect("creates");
    let progress = cache.region_progress(id).expect("reads");
    assert_eq!(progress.completed_resources, 0);
    assert_eq!(progress.completed_bytes, 0);
}

/// Driving for a week does not cost the user the city they downloaded.
///
/// This is the whole point of pinning. The ambient cache is deliberately small; a region is
/// deliberately not. If eviction ranked region resources merely last they would still go once
/// the region outgrew the bound — which is the case downloading a region is *for*.
#[test]
fn ambient_pressure_does_not_evict_a_region() {
    let cache = SqliteCache::in_memory_with_capacity(4_000).expect("opens");
    let id = cache.create_region(&berlin(), None, 0).expect("creates");

    cache
        .put_region_resource(
            id,
            "https://host/berlin/12/2200/1343.mvt",
            &body(3_000),
            100,
        )
        .expect("stores");

    // Now drive: fill the ambient cache many times over.
    for tile in 0..20 {
        cache
            .put(&format!("https://host/road/{tile}.mvt"), &body(1_000), 200)
            .expect("stores");
    }

    assert!(
        cache
            .get("https://host/berlin/12/2200/1343.mvt", 300)
            .expect("reads")
            .is_some(),
        "the downloaded tile survived"
    );
    assert_eq!(
        cache.region_progress(id).expect("reads").completed_bytes,
        3_000
    );
}

/// A region is not bounded by the ambient cache size.
///
/// A download that silently dropped its own tail to stay under a fifty-megabyte limit would
/// report success and produce a map with holes.
#[test]
fn a_region_may_exceed_the_ambient_bound() {
    let cache = SqliteCache::in_memory_with_capacity(1_000).expect("opens");
    let id = cache.create_region(&berlin(), None, 0).expect("creates");

    for tile in 0..10 {
        cache
            .put_region_resource(id, &format!("https://host/z12/{tile}.mvt"), &body(500), 100)
            .expect("stores");
    }

    let progress = cache.region_progress(id).expect("reads");
    assert_eq!(progress.completed_resources, 10);
    assert_eq!(progress.completed_bytes, 5_000, "five times the bound");
    assert_eq!(cache.region_size().expect("reads"), 5_000);
}

/// Two regions that overlap share the bytes, and neither can pull them from the other.
///
/// Overlapping downloads are the normal case — a user picks their city, then the route out of
/// it. Storing the shared tiles twice would double the disk for no benefit; letting the first
/// deletion take them would leave the second region with holes it reports as complete.
#[test]
fn overlapping_regions_share_and_neither_frees_the_other() {
    let cache = SqliteCache::in_memory_with_capacity(1_000_000).expect("opens");
    let city = cache.create_region(&berlin(), None, 0).expect("creates");
    let mut route = berlin();
    route.bounds = Bounds::new(13.4, 52.4, 14.5, 52.9);
    let route = cache.create_region(&route, None, 0).expect("creates");

    let shared = "https://host/z12/shared.mvt";
    cache
        .put_region_resource(city, shared, &body(2_000), 100)
        .expect("stores");
    cache.claim(route, shared).expect("claims");

    assert_eq!(
        cache.region_size().expect("reads"),
        2_000,
        "stored once, claimed twice"
    );
    assert_eq!(
        cache
            .region_progress(city)
            .expect("reads")
            .completed_resources,
        1
    );
    assert_eq!(
        cache
            .region_progress(route)
            .expect("reads")
            .completed_resources,
        1
    );

    cache.delete_region(city).expect("deletes");
    assert!(
        cache.get(shared, 200).expect("reads").is_some(),
        "the route still holds it"
    );
    assert_eq!(
        cache
            .region_progress(route)
            .expect("reads")
            .completed_resources,
        1
    );
}

/// Removing a region gives its space back over time, not its content immediately.
///
/// A user who removes a downloaded city and then looks at it still sees it. The bytes go when
/// something needs the room, which is the difference between a responsive delete and a stall.
#[test]
fn deleting_a_region_unpins_rather_than_erases() {
    let cache = SqliteCache::in_memory_with_capacity(10_000).expect("opens");
    let id = cache.create_region(&berlin(), None, 0).expect("creates");
    cache
        .put_region_resource(id, "https://host/z12/a.mvt", &body(2_000), 100)
        .expect("stores");

    cache.delete_region(id).expect("deletes");
    assert!(cache.regions().expect("lists").is_empty());
    assert_eq!(cache.region_size().expect("reads"), 0, "no longer claimed");
    assert!(
        cache
            .get("https://host/z12/a.mvt", 200)
            .expect("reads")
            .is_some(),
        "still there, now ordinary cache"
    );

    // And now it is ordinary cache, ambient pressure reclaims it.
    for tile in 0..10 {
        cache
            .put(&format!("https://host/road/{tile}.mvt"), &body(2_000), 300)
            .expect("stores");
    }
    assert!(
        cache
            .get("https://host/z12/a.mvt", 400)
            .expect("reads")
            .is_none(),
        "reclaimed once the room was needed"
    );
}

/// A resource already cached is claimed rather than refetched.
///
/// Which is why a region covering somewhere the user has been downloads less than one covering
/// somewhere they have not.
#[test]
fn a_download_claims_what_is_already_held() {
    let cache = SqliteCache::in_memory_with_capacity(100_000).expect("opens");
    let url = "https://host/z12/already.mvt";
    cache.put(url, &body(1_500), 100).expect("stores");

    let id = cache.create_region(&berlin(), None, 200).expect("creates");
    cache.claim(id, url).expect("claims");

    let progress = cache.region_progress(id).expect("reads");
    assert_eq!(progress.completed_resources, 1);
    assert_eq!(progress.completed_bytes, 1_500);
}

/// A claim whose body is gone counts as still owed, not as done.
///
/// Otherwise a region whose tiles were removed out from under it would read complete and render
/// nothing, which is the one failure a progress bar must not hide.
#[test]
fn a_claim_without_a_body_is_not_progress() {
    let cache = SqliteCache::in_memory().expect("opens");
    let id = cache.create_region(&berlin(), None, 0).expect("creates");
    let url = "https://host/z12/vanishes.mvt";
    cache
        .put_region_resource(id, url, &body(800), 100)
        .expect("stores");
    assert_eq!(
        cache
            .region_progress(id)
            .expect("reads")
            .completed_resources,
        1
    );

    cache.remove(url).expect("removes");
    assert_eq!(
        cache
            .region_progress(id)
            .expect("reads")
            .completed_resources,
        0,
        "owed again"
    );
}

/// A claim can outrun the body it names, and the body arrives pinned.
///
/// A download claims as it enumerates and stores as responses come back, so the two orders both
/// happen. If a body that arrived after its claim were born unpinned, the next ambient fill
/// would take it out from under the region that owns it — a hole in a download that reported
/// itself complete.
#[test]
fn a_body_arriving_after_its_claim_is_still_pinned() {
    let cache = SqliteCache::in_memory_with_capacity(4_000).expect("opens");
    let id = cache.create_region(&berlin(), None, 0).expect("creates");
    let url = "https://host/z12/late.mvt";

    cache.claim(id, url).expect("claims ahead of the body");
    cache.put(url, &body(3_000), 100).expect("stores");

    for tile in 0..20 {
        cache
            .put(&format!("https://host/road/{tile}.mvt"), &body(1_000), 200)
            .expect("stores");
    }
    assert!(
        cache.get(url, 300).expect("reads").is_some(),
        "claimed before it arrived, and still held"
    );
}

/// Claiming the same resource twice from one region pins it once.
///
/// A resumed download re-walks tiles it already has. If each pass incremented, the count would
/// never reach zero on delete and the space would never come back.
#[test]
fn a_repeated_claim_does_not_double_pin() {
    let cache = SqliteCache::in_memory_with_capacity(4_000).expect("opens");
    let id = cache.create_region(&berlin(), None, 0).expect("creates");
    let url = "https://host/z12/twice.mvt";

    cache
        .put_region_resource(id, url, &body(3_000), 100)
        .expect("stores");
    cache.claim(id, url).expect("claims again");
    cache
        .put_region_resource(id, url, &body(3_000), 100)
        .expect("stores again");

    cache.delete_region(id).expect("deletes");
    assert_eq!(
        cache.region_size().expect("reads"),
        0,
        "one delete released it"
    );
}
