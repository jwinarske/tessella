//! What a downloaded region has to survive: the ambient cache filling up around it.

#![cfg(feature = "cache")]

use tessella_storage::cache::SqliteCache;
use tessella_storage::offline::{Area, Region};
use tessella_storage::source::Response;
use tessella_tile::cover::Bounds;

fn berlin() -> Region {
    Region {
        style_url: "https://host/style.json".into(),
        area: Area::Box(Bounds::new(13.0, 52.3, 13.8, 52.7)),
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
    route.area = Area::Box(Bounds::new(13.4, 52.4, 14.5, 52.9));
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

/// A shape round-trips through the store, rings and holes intact.
///
/// A resumed download re-enumerates from what was stored, so a shape that came back as its
/// bounding box would quietly start fetching the sea the user declined the first time.
#[test]
fn a_shape_round_trips() {
    let cache = SqliteCache::in_memory().expect("opens");
    let shape = tessella_tile::polygon::Polygon::new(vec![
        [13.0, 52.3],
        [13.8, 52.3],
        [13.8, 52.7],
        [13.0, 52.7],
        [13.0, 52.3],
    ])
    .with_hole(vec![
        [13.3, 52.4],
        [13.5, 52.4],
        [13.5, 52.6],
        [13.3, 52.6],
        [13.3, 52.4],
    ]);
    let mut region = berlin();
    region.area = Area::Shape(vec![shape]);

    cache
        .create_region(&region, Some("Berlin, less the lake"), 1_000)
        .expect("creates");

    let stored = cache.regions().expect("lists");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].region, region, "the rings came back");
    assert_eq!(
        stored[0].region.area.tile_count(11),
        region.area.tile_count(11)
    );
}

/// A stored shape covers less than the box around it.
///
/// The whole reason shapes exist. A region drawn around a coastal city and stored as its
/// bounding box is a download of the sea.
#[test]
fn a_shape_costs_less_than_its_box() {
    let triangle = tessella_tile::polygon::Polygon::new(vec![
        [13.0, 52.3],
        [13.8, 52.3],
        [13.0, 52.7],
        [13.0, 52.3],
    ]);
    let mut shaped = berlin();
    shaped.area = Area::Shape(vec![triangle]);

    let boxed = berlin();
    let z = 12;
    assert!(
        shaped.area.tile_count(z) * 3 < boxed.area.tile_count(z) * 2,
        "{} against {}",
        shaped.area.tile_count(z),
        boxed.area.tile_count(z)
    );
}

/// A region written before shapes existed reads back as the box it was.
#[test]
fn an_older_region_reads_as_a_box() {
    let directory = tempfile::tempdir().expect("a temp dir");
    let path = directory.path().join("cache.sqlite");
    {
        let cache = SqliteCache::open(&path).expect("opens");
        cache
            .create_region(&berlin(), Some("Berlin"), 1)
            .expect("creates");
    }
    // Drop the column, as a database from before this feature would have.
    {
        let connection = rusqlite::Connection::open(&path).expect("opens");
        connection
            .execute_batch("ALTER TABLE regions DROP COLUMN geometry")
            .expect("drops");
    }

    let cache = SqliteCache::open(&path).expect("reopens and migrates");
    let stored = cache.regions().expect("lists");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].region.area, berlin().area);
}

// --- Reclaiming space, which SQLite does not do on its own. ---

/// Deleting a region and its bytes eventually returns the space to the filesystem.
///
/// The failure this closes: on a device with no room left, a user deletes a download to make
/// space and finds they have not made any, because SQLite marks pages free inside the file and
/// never shrinks it.
#[test]
fn packing_returns_space_to_the_filesystem() {
    let directory = tempfile::tempdir().expect("a temp dir");
    let path = directory.path().join("cache.sqlite");
    let cache = SqliteCache::with_capacity(&path, 200 * 1024 * 1024).expect("opens");
    let id = cache.create_region(&berlin(), None, 0).expect("creates");

    for tile in 0..200 {
        cache
            .put_region_resource(
                id,
                &format!("https://host/z12/{tile}.mvt"),
                &body(8 * 1024),
                100,
            )
            .expect("stores");
    }
    let full = cache.file_size().expect("reads");
    assert!(full > 1_000_000, "the file grew: {full}");

    cache.delete_region(id).expect("deletes");
    for tile in 0..200 {
        cache
            .remove(&format!("https://host/z12/{tile}.mvt"))
            .expect("removes");
    }

    // Measured just before the pack, because deleting the region already reclaimed some.
    let before_pack = cache.file_size().expect("reads");
    let packed = cache.pack().expect("packs");
    let after = cache.file_size().expect("reads");
    assert_eq!(
        before_pack - after,
        packed,
        "the report matches what the file did"
    );
    assert!(
        after * 4 < full,
        "most of the file came back: {after} of {full}"
    );
}

/// A cache with nothing to reclaim is not shrunk further, and does not fail.
#[test]
fn packing_an_empty_cache_is_harmless() {
    let cache = SqliteCache::in_memory().expect("opens");
    assert_eq!(cache.pack().expect("packs"), 0);
    assert_eq!(cache.pack().expect("packs again"), 0);
}

/// Deleting a region frees rows but not file space, until someone asks.
///
/// Packing costs time proportional to what *survives*, so doing it on every delete would make
/// removing one small region from a large cache rewrite every region the user kept.
#[test]
fn deleting_a_region_does_not_pack_by_itself() {
    let directory = tempfile::tempdir().expect("a temp dir");
    let path = directory.path().join("cache.sqlite");
    let cache = SqliteCache::with_capacity(&path, 200 * 1024 * 1024).expect("opens");
    let id = cache.create_region(&berlin(), None, 0).expect("creates");

    for tile in 0..200 {
        cache
            .put_region_resource(
                id,
                &format!("https://host/z12/{tile}.mvt"),
                &body(8 * 1024),
                100,
            )
            .expect("stores");
    }
    let full = cache.file_size().expect("reads");

    for tile in 0..200 {
        cache
            .remove(&format!("https://host/z12/{tile}.mvt"))
            .expect("removes");
    }
    cache.delete_region(id).expect("deletes");

    assert_eq!(
        cache.file_size().expect("reads"),
        full,
        "the file is still the size it was"
    );
    assert!(
        cache.free_bytes().expect("reads") * 4 > full * 3,
        "but nearly all of it is free space inside"
    );

    // Asking is what returns it.
    assert!(cache.pack().expect("packs") > 0);
    assert!(cache.file_size().expect("reads") * 4 < full);
}

/// Packing a database this never configured still works.
///
/// Cache files written by earlier versions, and files another tool made, are ordinary SQLite
/// databases. `VACUUM` needs nothing set up in advance, which is part of why it is the right
/// tool here — an incremental scheme would have needed a full rebuild first just to become
/// possible.
#[test]
fn a_foreign_database_packs_too() {
    let directory = tempfile::tempdir().expect("a temp dir");
    let path = directory.path().join("cache.sqlite");

    {
        let connection = rusqlite::Connection::open(&path).expect("opens");
        connection
            .execute_batch("CREATE TABLE ballast (id INTEGER, data BLOB)")
            .expect("prepares");
        for id in 0..500 {
            connection
                .execute(
                    "INSERT INTO ballast VALUES (?1, ?2)",
                    rusqlite::params![id, vec![0u8; 4096]],
                )
                .expect("fills");
        }
        connection
            .execute_batch("DELETE FROM ballast")
            .expect("empties");
    }

    let cache = SqliteCache::with_capacity(&path, 200 * 1024 * 1024).expect("opens");
    let before = cache.file_size().expect("reads");
    assert!(before > 1_000_000, "the ballast is still in the file");

    assert!(cache.pack().expect("packs") > 0);
    assert!(cache.file_size().expect("reads") * 4 < before);
    assert_eq!(cache.pack().expect("packs again"), 0, "nothing left");
}

/// Packing leaves everything that is still wanted.
///
/// `VACUUM` rewrites the file. A pack that lost a pinned region, or reset the pin counts that
/// keep it, would free exactly the space the user was paying to keep.
#[test]
fn packing_keeps_what_is_still_claimed() {
    let directory = tempfile::tempdir().expect("a temp dir");
    let cache =
        SqliteCache::with_capacity(&directory.path().join("cache.sqlite"), 200 * 1024 * 1024)
            .expect("opens");
    let id = cache
        .create_region(&berlin(), Some("Berlin"), 7)
        .expect("creates");
    for tile in 0..40 {
        cache
            .put_region_resource(
                id,
                &format!("https://host/keep/{tile}.mvt"),
                &body(4096),
                100,
            )
            .expect("stores");
    }
    // And some ambient traffic that is then dropped, to give the pack something to do.
    for tile in 0..200 {
        cache
            .put(&format!("https://host/drop/{tile}.mvt"), &body(4096), 100)
            .expect("stores");
        cache
            .remove(&format!("https://host/drop/{tile}.mvt"))
            .expect("removes");
    }

    let held = cache.region_progress(id).expect("reads");
    assert!(cache.pack().expect("packs") > 0);

    assert_eq!(cache.regions().expect("lists").len(), 1);
    assert_eq!(cache.region_progress(id).expect("reads"), held);
    assert!(
        cache
            .get("https://host/keep/0.mvt", 200)
            .expect("reads")
            .is_some()
    );

    // And the pins survived, so ambient pressure still cannot take the region.
    for tile in 0..500 {
        cache
            .put(&format!("https://host/after/{tile}.mvt"), &body(4096), 300)
            .expect("stores");
    }
    assert_eq!(cache.region_progress(id).expect("reads"), held);
}
