//! A pitched cover stops short of the target zoom rather than asking for a tile per pixel.
//!
//! # What goes wrong without it
//!
//! Tilting a camera puts the top of the screen near the horizon, where one pixel covers an
//! unbounded amount of ground. Descending the quadtree to the target zoom everywhere then asks
//! for tiles that occupy a pixel each, and the count runs away with the angle: on a 1920×1080
//! view at z15 it is forty-two tiles at 55° and nine hundred and ninety-two at 70°. Past about
//! seventy-five it passed `MAX_TILES` and the cover failed outright, so the map went blank at
//! exactly the angles a driving view uses.
//!
//! Drawing it coarser is the answer rather than drawing less of it: a tile near the horizon is a
//! few pixels tall whatever its zoom, so its parent looks the same and costs a quarter as much.
//!
//! # Sixty degrees, and why mbgl never reaches it
//!
//! mbgl gates this on `tileLodPitchThreshold`, sixty degrees — which is also its
//! `DEFAULT_PITCH_MAX`. Its camera stops exactly where the mechanism would start, so with stock
//! settings mbgl never runs the code it carries. This build clamps to the horizon angle instead
//! (89.25°, mbgl's `maxMercatorHorizonAngle`), so it reaches the angles the threshold was
//! written for and needs the mechanism that answers them.

use std::collections::BTreeMap;

use tessella_tile::camera;
use tessella_tile::cover::{self, ViewTransform};

/// The product view: a 1920×1080 automotive screen at street zoom.
fn view(pitch: f64) -> ViewTransform {
    camera::settled(&ViewTransform {
        longitude: -100.222778,
        latitude: 25.666285,
        zoom: 15.0,
        width: 1920.0,
        height: 1080.0,
        bearing: 0.0,
        pitch,
    })
}

fn levels(pitch: f64) -> BTreeMap<u8, usize> {
    let mut counts = BTreeMap::new();
    for tile in cover::cover(&view(pitch)).expect("the cover resolves") {
        *counts.entry(tile.z).or_insert(0) += 1;
    }
    counts
}

/// At or below the threshold the cover is exactly what it was: one level, every tile.
///
/// The product view sits at 55°, so this is the assertion that the change costs nothing where
/// it is actually used. Sixty itself is included because mbgl compares with `>` and the
/// boundary is the easiest thing to get wrong by one.
#[test]
fn a_view_within_sixty_degrees_is_untouched() {
    for pitch in [0.0, 30.0, 45.0, 55.0, 60.0] {
        let levels = levels(pitch);
        assert_eq!(
            levels.len(),
            1,
            "pitch {pitch} mixed levels below the threshold: {levels:?}"
        );
        assert!(
            levels.contains_key(&15),
            "pitch {pitch} covered something other than the target zoom: {levels:?}"
        );
    }
}

/// Past it the cover mixes levels, and the far ones are the coarse ones.
#[test]
fn a_steeper_view_mixes_levels() {
    let levels = levels(70.0);
    assert!(
        levels.len() > 1,
        "a seventy degree view stayed on one level: {levels:?}"
    );
    // The target zoom is still there: coarsening the horizon must not coarsen the middle of
    // the screen, which is the part being looked at.
    assert!(
        levels.get(&15).copied().unwrap_or(0) > 0,
        "nothing was left at the target zoom: {levels:?}"
    );
}

/// The angles that used to fail now resolve, and cheaply.
///
/// Nine hundred and ninety-two tiles at seventy, and outright failure past seventy-five. The
/// bound asserted here is loose on purpose — it is a statement that the count no longer tracks
/// the angle, not a golden of mbgl's arithmetic.
#[test]
fn the_steepest_views_resolve_and_stay_bounded() {
    for pitch in [65.0, 70.0, 80.0, 89.25] {
        let tiles = cover::cover(&view(pitch))
            .unwrap_or_else(|error| panic!("pitch {pitch} did not cover: {error:?}"));
        assert!(
            tiles.len() < 200,
            "pitch {pitch} still asks for {} tiles",
            tiles.len()
        );
    }

    // Steeper costs more, but not the way it used to: an unbounded descent went 105, 992, fail
    // across these three, where stopping short keeps them within a factor of two of each other.
    let steep = cover::cover(&view(89.25)).expect("covers").len();
    let moderate = cover::cover(&view(65.0)).expect("covers").len();
    assert!(
        steep < moderate * 3,
        "the angle still drives the count: {moderate} at 65 degrees, {steep} at 89"
    );
}

/// Every tile the cover names is one a source could serve.
///
/// A mixed-level cover holds tiles whose row index is bounded by their *own* zoom. Testing them
/// all against the deepest level would let a row through at every level above it — a tile that
/// does not exist, requested once per frame.
#[test]
fn every_tile_is_addressable_at_its_own_level() {
    for pitch in [55.0, 65.0, 70.0, 89.25] {
        for tile in cover::cover(&view(pitch)).expect("covers") {
            let span = 1u32 << tile.z;
            assert!(
                tile.y < span,
                "pitch {pitch}: {}/{}/{} is outside a z{} world of {span} rows",
                tile.z,
                tile.x,
                tile.y,
                tile.z
            );
            assert!(tile.z <= 15, "pitch {pitch}: {} is past the target", tile.z);
        }
    }
}
