//! Measurements of the cache read path, not assertions about it.
//!
//! Run with `cargo test -p tessella-storage --features cache --release --test cache_bench -- --ignored --nocapture`.

#![cfg(feature = "cache")]
#![allow(clippy::print_stdout)]

use std::time::Instant;

use tessella_storage::cache::SqliteCache;
use tessella_storage::source::Response;

const TILE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/real-world-0-0-0.mvt");
const TILES: usize = 20;
const ROUNDS: u32 = 20;

#[test]
#[ignore = "a measurement, not a test"]
fn read_path_costs() {
    let directory = tempfile::tempdir().expect("a temp dir");

    // A cache holding twenty realistic tiles.
    let cache =
        SqliteCache::with_capacity(&directory.path().join("cache.sqlite"), 200 * 1024 * 1024)
            .expect("opens");
    let response = Response {
        status: 200,
        body: TILE.to_vec(),
        ..Response::default()
    };
    for index in 0..TILES {
        cache
            .put(&format!("https://host/{index}"), &response, 1_000)
            .expect("stores");
    }
    let bytes = TILE.len() * TILES;
    println!(
        "{TILES} tiles of {} KiB = {} KiB total",
        TILE.len() / 1024,
        bytes / 1024
    );

    // The same bytes as plain files, for a floor.
    for index in 0..TILES {
        std::fs::write(directory.path().join(format!("{index}.mvt")), TILE).expect("writes");
    }

    let mut sqlite = std::time::Duration::MAX;
    let mut files = std::time::Duration::MAX;
    for _ in 0..ROUNDS {
        let start = Instant::now();
        let mut total = 0usize;
        for index in 0..TILES {
            let entry = cache
                .get(&format!("https://host/{index}"), 1_000)
                .expect("queries")
                .expect("present");
            total += entry.response.body.len();
        }
        assert_eq!(total, bytes);
        sqlite = sqlite.min(start.elapsed());

        let start = Instant::now();
        let mut total = 0usize;
        for index in 0..TILES {
            total += std::fs::read(directory.path().join(format!("{index}.mvt")))
                .expect("reads")
                .len();
        }
        assert_eq!(total, bytes);
        files = files.min(start.elapsed());
    }

    // A floor: the blob and nothing else. `get` also reads six other columns and builds an
    // `Entry`, so this is not the same query minus the bookkeeping — it is the least SQLite
    // could possibly do for this workload.
    let mut read_only = std::time::Duration::MAX;
    {
        use rusqlite::Connection;
        let connection = Connection::open(directory.path().join("cache.sqlite")).expect("opens");
        for _ in 0..ROUNDS {
            let start = Instant::now();
            let mut total = 0usize;
            for index in 0..TILES {
                let body: Vec<u8> = connection
                    .query_row(
                        "SELECT data FROM responses WHERE url = ?1",
                        rusqlite::params![format!("https://host/{index}")],
                        |row| row.get(0),
                    )
                    .expect("queries");
                total += body.len();
            }
            assert_eq!(total, bytes);
            read_only = read_only.min(start.elapsed());
        }
    }

    let rate = |d: std::time::Duration| (bytes as f64 / 1_048_576.0) / d.as_secs_f64();
    println!(
        "  sqlite get():   {sqlite:>10.3?}  ({:.0} MiB/s)",
        rate(sqlite)
    );
    println!(
        "  fs::read():     {files:>10.3?}  ({:.0} MiB/s)",
        rate(files)
    );
    println!(
        "  blob only:      {read_only:>10.3?}  ({:.0} MiB/s)",
        rate(read_only)
    );
    println!(
        "  sqlite get() is {:.2}x a plain file read, {:.2}x the blob-only floor",
        sqlite.as_secs_f64() / files.as_secs_f64(),
        sqlite.as_secs_f64() / read_only.as_secs_f64()
    );
}

/// What pinning a large region costs the ambient write path.
///
/// Eviction runs after every ambient write and has to exclude whatever regions claim. A user
/// who downloads a country pins hundreds of thousands of URLs, and if the exclusion is walked
/// per write then every tile the map fetches afterwards pays for the download that already
/// finished.
///
/// With `url NOT IN (SELECT url FROM region_resources)` this measured 238 us at zero claims,
/// 2.9 ms at ten thousand and 33 ms at a hundred thousand — linear, and past a frame. With a
/// `pinned` count on the row and the `responses_evictable` index it is flat at about 150 us,
/// which is also faster than the original because eviction now walks only the rows it may take.
#[test]
#[ignore = "a measurement, not a test"]
fn eviction_cost_against_a_large_region() {
    let directory = tempfile::tempdir().expect("a temp dir");
    let cache = SqliteCache::with_capacity(&directory.path().join("cache.sqlite"), 4 * 1024 * 1024)
        .expect("opens");

    // Small bodies: the point is the number of claims, not the bytes.
    let small = Response {
        status: 200,
        body: vec![0u8; 256],
        ..Response::default()
    };
    let region = cache
        .create_region(
            &tessella_storage::offline::Region {
                style_url: "https://host/style.json".into(),
                area: tessella_storage::offline::Area::Box(tessella_tile::cover::Bounds::new(
                    -5.0, 41.0, 9.0, 51.0,
                )),
                min_zoom: 0.0,
                max_zoom: 14.0,
                pixel_ratio: 1.0,
                include_ideographs: false,
            },
            Some("a country"),
            0,
        )
        .expect("creates");

    for pins in [0usize, 10_000, 100_000] {
        // Top the region up to `pins` claims.
        let existing: usize = usize::try_from(
            cache
                .region_progress(region)
                .expect("reads")
                .completed_resources,
        )
        .unwrap_or(0);
        for index in existing..pins {
            cache
                .put_region_resource(
                    region,
                    &format!("https://host/pinned/{index}"),
                    &small,
                    1_000,
                )
                .expect("stores");
        }

        // Fill the ambient side so eviction actually has work to do.
        let ambient = Response {
            status: 200,
            body: TILE.to_vec(),
            ..Response::default()
        };
        for index in 0..40 {
            cache
                .put(&format!("https://host/warm/{index}"), &ambient, 2_000)
                .expect("stores");
        }

        let started = Instant::now();
        for index in 0..ROUNDS {
            cache
                .put(&format!("https://host/hot/{index}"), &ambient, 3_000)
                .expect("stores");
        }
        let each = started.elapsed() / ROUNDS;
        println!("{pins:>7} pinned: {each:?} per ambient write");
    }
}

/// Incremental auto-vacuum against plain `VACUUM`, on both the write and the reclaim.
///
/// The comparison that chose `SqliteCache::pack`'s strategy. Rounds alternate the order because
/// they do not otherwise measure the same thing: the first database built in a process pays
/// page-cache costs the second does not, and reading that as a difference between the two
/// settings is exactly the mistake this alternation exists to prevent. An earlier version of
/// this ran the two arms as separate parallel tests and reported a sixty-one per cent write
/// penalty that does not exist.
#[test]
#[ignore = "a measurement, not a test"]
fn autovacuum_against_vacuum() {
    const TILES: usize = 200;

    for round in 0..3 {
        let order: [&str; 2] = if round % 2 == 0 {
            ["NONE", "INCREMENTAL"]
        } else {
            ["INCREMENTAL", "NONE"]
        };
        for setting in order {
            let directory = tempfile::tempdir().expect("a temp dir");
            let path = directory.path().join("cache.sqlite");
            let connection = rusqlite::Connection::open(&path).expect("opens");
            connection
                .execute_batch(&format!("PRAGMA auto_vacuum={setting}"))
                .expect("sets");
            connection
                .query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))
                .expect("wal");
            connection
                .execute_batch("CREATE TABLE r (url TEXT PRIMARY KEY, data BLOB)")
                .expect("creates");

            let started = Instant::now();
            for index in 0..TILES {
                connection
                    .execute(
                        "INSERT INTO r VALUES (?1, ?2)",
                        rusqlite::params![index.to_string(), TILE],
                    )
                    .expect("stores");
            }
            let per_write = started.elapsed() / u32::try_from(TILES).expect("fits");

            let mode: i64 = connection
                .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))
                .expect("reads");
            let before: i64 = connection
                .query_row("PRAGMA page_count", [], |row| row.get(0))
                .expect("reads");
            connection.execute_batch("DELETE FROM r").expect("empties");

            let started = Instant::now();
            if mode == 2 {
                // Stepped to the end: one page per row, and `execute_batch` would step once.
                let mut statement = connection
                    .prepare("PRAGMA incremental_vacuum")
                    .expect("prepares");
                let mut rows = statement.query([]).expect("runs");
                while rows.next().expect("steps").is_some() {}
            } else {
                connection.execute_batch("VACUUM").expect("vacuums");
            }
            let reclaim = started.elapsed();
            let after: i64 = connection
                .query_row("PRAGMA page_count", [], |row| row.get(0))
                .expect("reads");

            println!(
                "round {round} auto_vacuum={setting:<12} write {per_write:?}/tile  \
                 reclaim {reclaim:?}  pages {before} -> {after}"
            );
        }
    }
}
