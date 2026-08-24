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
