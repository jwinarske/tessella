//! Reading a real PMTiles archive, checked against what `pmtiles serve` returns for it.

#![cfg(feature = "pmtiles")]

use tessella_storage::pmtiles::{Archive, Compression, Header, tile_id};

/// The archive the live tests use, when it is present.
///
/// Two hundred megabytes is not something to vendor, so the tests that need it skip when it is
/// absent rather than failing — and the format tests below need no archive at all, which is
/// where most of the coverage is.
fn archive_path() -> Option<std::path::PathBuf> {
    let path = std::path::Path::new("/mnt/dev/maplibre-frontend/tileserver/berlin_z15.pmtiles");
    path.exists().then(|| path.to_path_buf())
}

/// The tile ids the PMTiles reference implementation produces.
///
/// Transcribed from the spec rather than derived. A Hilbert curve has several conventions that
/// differ in which corner they start from, and choosing the wrong one gives ids that are
/// plausible, contiguous, and address the wrong tiles — which reads as an archive full of the
/// right map in the wrong places.
#[test]
fn tile_ids_match_the_reference() {
    // The spec's own worked examples: the single tile at zoom 0, then the four at zoom 1 in
    // Hilbert rather than raster order.
    assert_eq!(tile_id(0, 0, 0), Some(0));
    assert_eq!(tile_id(1, 0, 0), Some(1));
    assert_eq!(tile_id(1, 0, 1), Some(2));
    assert_eq!(tile_id(1, 1, 1), Some(3));
    assert_eq!(tile_id(1, 1, 0), Some(4));

    // Zoom 2 begins after zoom 0 and 1, so after 1 + 4 tiles.
    assert_eq!(tile_id(2, 0, 0), Some(5));
    // And zoom 3 after 1 + 4 + 16.
    assert_eq!(tile_id(3, 0, 0), Some(21));
}

/// Every tile of a level has a distinct id, and they fill the level's range exactly.
///
/// The property that makes the directory's run-length encoding work: ids within a level are a
/// permutation of a contiguous range, so consecutive tiles on the curve are consecutive ids.
#[test]
fn a_level_is_a_permutation_of_its_range() {
    for z in 0..=6u8 {
        let side = 1u32 << z;
        let base = tile_id(z, 0, 0).expect("in range");
        let mut seen: Vec<u64> = Vec::with_capacity((side * side) as usize);
        for x in 0..side {
            for y in 0..side {
                seen.push(tile_id(z, x, y).expect("in range"));
            }
        }
        seen.sort_unstable();
        let expected: Vec<u64> = (base..base + u64::from(side) * u64::from(side)).collect();
        assert_eq!(seen, expected, "zoom {z}");
    }
}

/// Neighbours on the curve are neighbours on the map.
///
/// The reason the format uses a Hilbert curve at all: a viewport is a small number of contiguous
/// reads rather than nine scattered ones. A raster-order id would satisfy the permutation test
/// above and lose this entirely.
#[test]
fn consecutive_ids_are_adjacent_tiles() {
    let z = 6u8;
    let side = 1u32 << z;
    let mut by_id: Vec<((u32, u32), u64)> = Vec::new();
    for x in 0..side {
        for y in 0..side {
            by_id.push(((x, y), tile_id(z, x, y).expect("in range")));
        }
    }
    by_id.sort_by_key(|(_, id)| *id);

    for pair in by_id.windows(2) {
        let ((ax, ay), _) = pair[0];
        let ((bx, by), _) = pair[1];
        let step = ax.abs_diff(bx) + ay.abs_diff(by);
        assert_eq!(step, 1, "({ax},{ay}) then ({bx},{by}) is not a single step");
    }
}

/// A file that is not an archive is refused rather than read as one.
#[test]
fn a_non_archive_is_refused() {
    let bytes: &[u8] = b"this is not a PMTiles file, it is a sentence";
    assert!(Archive::open(bytes).is_err());

    // Right magic, wrong version: v1 and v2 have a different directory format entirely, so
    // reading one as v3 would produce plausible offsets into the wrong bytes.
    let mut v2 = [0u8; Header::LEN];
    v2[..7].copy_from_slice(b"PMTiles");
    v2[7] = 2;
    let slice: &[u8] = &v2;
    assert!(matches!(
        Archive::open(slice),
        Err(tessella_storage::pmtiles::PmtilesError::UnsupportedVersion(
            2
        ))
    ));
}

/// A truncated archive is refused rather than read past its end.
#[test]
fn a_truncated_archive_is_refused() {
    let short: &[u8] = b"PMTiles\x03";
    assert!(Archive::open(short).is_err());
}

/// The header of a real archive says what the file says.
#[test]
fn a_real_header_parses() {
    let Some(path) = archive_path() else {
        return;
    };
    let file = std::fs::File::open(&path).expect("opens");
    let archive = Archive::open(file).expect("reads");
    let header = archive.header();

    assert_eq!(header.min_zoom, 0);
    assert_eq!(header.max_zoom, 15);
    assert_eq!(header.internal_compression, Compression::Gzip);
    assert_eq!(header.tile_compression, Compression::Gzip);
    assert!(header.tiles.1 > 90_000_000, "a ninety-megabyte tile region");

    // The metadata is the tileset's own description, and it decompresses to JSON.
    let metadata = archive.metadata().expect("reads metadata");
    assert!(metadata.starts_with(b"{"), "metadata is a JSON document");
}

/// Tiles read out of the archive decode as MVT, at several zooms.
///
/// Reaching zoom 14 matters: an archive this size has leaf directories, so a tile there is found
/// by following a run-length-zero entry into a second directory rather than in the root.
#[test]
fn real_tiles_decode() {
    let Some(path) = archive_path() else {
        return;
    };
    let file = std::fs::File::open(&path).expect("opens");
    let archive = Archive::open(file).expect("reads");

    for (z, x, y) in [
        (0u8, 0u32, 0u32),
        (5, 17, 10),
        (12, 2200, 1343),
        (14, 8802, 5373),
    ] {
        let bytes = archive
            .tile(z, x, y)
            .unwrap_or_else(|error| panic!("{z}/{x}/{y}: {error}"))
            .unwrap_or_else(|| panic!("{z}/{x}/{y} is not in the archive"));
        let tile = tessella_source::mvt::Tile::decode(&bytes)
            .unwrap_or_else(|error| panic!("{z}/{x}/{y} did not decode: {error}"));
        assert!(!tile.layers.is_empty(), "{z}/{x}/{y} decoded to nothing");
    }
}

/// A tile outside the archive's coverage is absent rather than an error.
///
/// A tileset's coverage is not a rectangle, and asking for a tile outside it is how the edge is
/// found — the same reason a 404 from an origin is a response.
#[test]
fn a_missing_tile_is_absent() {
    let Some(path) = archive_path() else {
        return;
    };
    let file = std::fs::File::open(&path).expect("opens");
    let archive = Archive::open(file).expect("reads");

    // Berlin's archive, asked for a tile over the Pacific.
    assert!(archive.tile(14, 100, 100).expect("no error").is_none());
    // And a zoom the archive does not carry.
    assert!(archive.tile(20, 0, 0).expect("no error").is_none());
}

/// A coordinate off the edge of its zoom must not resolve to a tile that is on it.
///
/// The Hilbert walk masks one bit at a time against `s < 2^z`, so an `x` with a bit above that
/// range loses it and walks exactly as `x` without it does. Before this was checked, `z1/x2/y0`
/// returned the id of `z1/x0/y0` — not an error and not an empty result, but a different
/// tile's bytes, which is the kind of wrong that gets drawn rather than reported.
#[test]
fn an_out_of_range_coordinate_has_no_id() {
    assert_eq!(tile_id(1, 0, 0), Some(1));
    assert_eq!(tile_id(1, 2, 0), None);
    assert_eq!(tile_id(1, 0, 2), None);
    assert_eq!(tile_id(0, 1, 0), None);

    // The last zoom whose level fits a u64, and the first that does not. `1 << (2 * z)` is a
    // shift past the width for `z > 31`: a panic in a debug build, a wrap in a release one.
    assert!(tile_id(31, 0, 0).is_some());
    assert_eq!(tile_id(32, 0, 0), None);
    assert_eq!(tile_id(255, 0, 0), None);
}

/// And the archive answers "no such tile" rather than handing back its aliased neighbour.
#[test]
fn an_out_of_range_coordinate_reads_no_tile() {
    let Some(path) = archive_path() else {
        return;
    };
    let archive = Archive::open(std::fs::File::open(path).expect("open")).expect("archive");
    assert!(archive.tile(0, 0, 0).expect("read").is_some());
    assert!(archive.tile(0, 1, 0).expect("read").is_none());
    assert!(archive.tile(32, 0, 0).expect("read").is_none());
}
