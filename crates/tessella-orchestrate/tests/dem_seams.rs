//! A DEM tile's border is replaced by its neighbors' real pixels once they land.
//!
//! # What this is checking
//!
//! A DEM is stored one pixel wider on every side, and `Dem::new` fills that border by repeating
//! the tile's own edge -- a guess, so a tile can be shaded before any neighbor exists. A
//! hillshade reads the border, so the guess draws a line of slope along every tile edge that
//! nothing on the ground has: the seams the whole of this file exists to close.
//!
//! The guess is only replaced if something asks. `Dem::backfill_border` has been here since the
//! decoder was written and nothing in the running map ever called it, which is invisible in a
//! unit test -- the function is correct -- and visible in every frame of real terrain.
//!
//! Checked through the store rather than by calling the backfill directly: what was missing was
//! the wiring, so the test drives the tiles in over the transport and reads what the frame would
//! read.

#![cfg(feature = "image")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use tessella_orchestrate::boot::BootError;
use tessella_orchestrate::cache::TileCache;
use tessella_orchestrate::deferred::TileTransport;
use tessella_orchestrate::map::Tiles;
use tessella_orchestrate::pool::Pool;
use tessella_orchestrate::source::{Readiness, TileSource};
use tessella_orchestrate::tile::{Content, TileId};
use tessella_storage::source::{Coalescing, FetchError, FileSource, Response};
use tessella_tile::cover::{TileCoord, ViewTransform};
use tessella_tile::store::Surface;

const TILE_PNG: &[u8] = include_bytes!("../../../tests/image-fixtures/tile.png");

/// Serves one image at every DEM coordinate.
///
/// One image is enough, and is the sharper test: every tile then holds the *same* elevation, so
/// the column a neighbor contributes is the fixture's first column while the repeated guess is
/// its last. The two disagree -- asserted below, so this cannot pass vacuously -- and which of
/// them is in the border says whether the backfill ran.
struct Origin;

impl FileSource for Origin {
    fn fetch(&self, url: &str) -> Result<Response, FetchError> {
        if !url.contains("/dem/") {
            return Ok(Response {
                status: 404,
                ..Response::default()
            });
        }
        Ok(Response {
            status: 200,
            body: TILE_PNG.to_vec(),
            ..Response::default()
        })
    }
}

const STYLE: &str = r#"{"version": 8,
     "sources": {"dem": {"type": "raster-dem",
         "tiles": ["https://o/dem/{z}/{x}/{y}.png"], "tileSize": 512}},
     "layers": [{"id": "shade", "type": "hillshade", "source": "dem"}]}"#;

fn view() -> ViewTransform {
    ViewTransform {
        longitude: 0.0,
        latitude: 0.0,
        zoom: 1.0,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
        ground_below: 0.0,
    }
}

/// Ticks until `done` or the deadline. `async_source`'s, for its reason: nothing lands unless
/// something drains, and the reconciliation this tests is started by a drain as well.
fn settle<D: TileTransport + 'static>(
    source: &Arc<TileSource<D>>,
    mut done: impl FnMut() -> bool,
) -> bool {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        source.drain();
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

/// The elevation a tile's hillshade was built from.
fn dem<D: TileTransport + 'static>(
    source: &Arc<TileSource<D>>,
    tile: TileId,
) -> Option<Arc<tessella_source::dem::Dem>> {
    source
        .buckets(tile)?
        .iter()
        .find_map(|bucket| match &bucket.content {
            Content::Hillshade(hillshade) => Some(Arc::clone(&hillshade.dem)),
            _ => None,
        })
}

/// The whole world at z1: four tiles, each bordering the other three.
fn cover() -> Vec<TileCoord> {
    (0..2)
        .flat_map(|x| {
            (0..2).map(move |y| TileCoord {
                z: 1,
                x,
                y,
                wrap: 0,
            })
        })
        .collect()
}

/// A landed tile's border carries its neighbor's edge, not a repeat of its own.
#[test]
fn a_dem_tile_takes_its_border_from_the_tile_next_to_it() {
    let files = Arc::new(Coalescing::new(Origin));
    let cache: Arc<TileCache<BootError>> = Arc::new(TileCache::new(64));
    let source = TileSource::new(STYLE.to_string(), files, cache, Pool::shared(), 1);

    let cover = cover();
    source.want(&view(), &cover, &[], Surface::Plane);
    assert!(
        settle(&source, || source.readiness() == Readiness::Ready),
        "never resolved"
    );
    source.want(&view(), &cover, &[], Surface::Plane);

    let (west, east) = (TileId::new(1, 0, 0), TileId::new(1, 1, 0));
    assert!(
        settle(&source, || dem(&source, west).is_some()
            && dem(&source, east).is_some()),
        "the tiles never landed"
    );

    // The precondition. Both tiles hold this same image, so the border to the west tile's east
    // is either the fixture's first column -- its neighbor's -- or a repeat of its last, and the
    // assertion below says nothing unless those differ.
    let sample = dem(&source, west).expect("the west tile landed");
    // From the tile, not from the style: `tileSize` says how the quad is covered, and the
    // border is indexed in the decoded image's own pixels.
    #[allow(clippy::cast_possible_wrap)]
    let dim = sample.dim() as i32;
    let (first, last) = (
        sample.elevation_exact(0, 0).expect("inside the tile"),
        sample.elevation_exact(dim - 1, 0).expect("inside the tile"),
    );
    assert!(
        (first - last).abs() > 0.5,
        "the fixture's two edges agree, so this test cannot tell a backfill from a repeat"
    );

    // And the property: reconciliation runs on the pool, off the store's lock, so it lands a
    // tick or two after the tiles themselves.
    let matched = settle(&source, || {
        dem(&source, west)
            .and_then(|west| Some((west.elevation_exact(dim, 0)?, dem(&source, east)?)))
            .and_then(|(border, east)| Some((border, east.elevation_exact(0, 0)?)))
            .is_some_and(|(border, neighbor)| (border - neighbor).abs() < 0.001)
    });
    assert!(
        matched,
        "the east border of {west:?} is still a repeat of its own edge: \
         {:?} against the neighbor's {first}",
        dem(&source, west).and_then(|west| west.elevation_exact(dim, 0)),
    );

    // Both directions. A border belongs to a pair, and fixing only the tile that arrived last
    // leaves the other half of every seam in the map.
    let east = dem(&source, east).expect("the east tile landed");
    assert_eq!(
        east.elevation_exact(-1, 0),
        Some(last),
        "the west border of the east tile did not take its neighbor's last column"
    );
}

/// And the north-south pair, which is the direction that is easy to get backwards.
///
/// Worth its own test rather than another line in the one above: a column is contributed by a
/// neighbor found through a wrapping coordinate and a row by one found through a bounded one, so
/// the two directions fail separately.
#[test]
fn a_dem_tile_takes_its_border_from_the_tile_below_it() {
    let files = Arc::new(Coalescing::new(Origin));
    let cache: Arc<TileCache<BootError>> = Arc::new(TileCache::new(64));
    let source = TileSource::new(STYLE.to_string(), files, cache, Pool::shared(), 1);

    let cover = cover();
    source.want(&view(), &cover, &[], Surface::Plane);
    assert!(
        settle(&source, || source.readiness() == Readiness::Ready),
        "never resolved"
    );
    source.want(&view(), &cover, &[], Surface::Plane);

    let (north, south) = (TileId::new(1, 0, 0), TileId::new(1, 0, 1));
    assert!(
        settle(&source, || dem(&source, north).is_some()
            && dem(&source, south).is_some()),
        "the tiles never landed"
    );

    let sample = dem(&source, north).expect("the north tile landed");
    #[allow(clippy::cast_possible_wrap)]
    let dim = sample.dim() as i32;
    let (first, last) = (
        sample.elevation_exact(0, 0).expect("inside the tile"),
        sample.elevation_exact(0, dim - 1).expect("inside the tile"),
    );
    assert!(
        (first - last).abs() > 0.5,
        "the fixture's two edges agree, so this test cannot tell a backfill from a repeat"
    );

    let matched = settle(&source, || {
        dem(&source, north)
            .and_then(|north| north.elevation_exact(0, dim))
            .is_some_and(|border| (border - first).abs() < 0.001)
    });
    assert!(
        matched,
        "the south border of the north tile is still a repeat of its own edge: {:?} against {first}",
        dem(&source, north).and_then(|north| north.elevation_exact(0, dim)),
    );

    let south = dem(&source, south).expect("the south tile landed");
    assert_eq!(
        south.elevation_exact(0, -1),
        Some(last),
        "the north border of the south tile did not take its neighbor's last row"
    );
}

/// The slope field is re-derived from the reconciled elevation, not left as first prepared.
#[test]
fn the_slope_field_is_prepared_again_from_the_reconciled_elevation() {
    let files = Arc::new(Coalescing::new(Origin));
    let cache: Arc<TileCache<BootError>> = Arc::new(TileCache::new(64));
    let source = TileSource::new(STYLE.to_string(), files, cache, Pool::shared(), 1);
    let cover = cover();
    source.want(&view(), &cover, &[], Surface::Plane);
    assert!(
        settle(&source, || source.readiness() == Readiness::Ready),
        "never resolved"
    );
    source.want(&view(), &cover, &[], Surface::Plane);
    let west = TileId::new(1, 0, 0);
    assert!(
        settle(&source, || dem(&source, west).is_some()),
        "never landed"
    );
    let ok = settle(&source, || {
        let Some(buckets) = source.buckets(west) else {
            return false;
        };
        buckets.iter().any(|bucket| match &bucket.content {
            Content::Hillshade(h) => h.prepared.pixels == h.dem.prepare(1).pixels,
            _ => false,
        })
    });
    assert!(
        ok,
        "the slope field does not match the elevation it is stored beside"
    );
}
