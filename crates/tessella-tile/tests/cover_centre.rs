//! The cover contains the tile the camera is pointed at.
//!
//! Cheap to state and worth stating: a rendered frame that is subtly in the wrong place looks
//! like a projection bug, and the first thing to rule out is that the wrong tiles were asked for.
//! When the frame was found to be vertically mirrored, this is the test that said the cover was
//! not the reason -- which is what moved the search to the consumer, where the flip was.

use tessella_tile::cover::{ViewTransform, cover};

#[test]
fn the_cover_contains_the_tile_the_centre_falls_in() {
    let view = ViewTransform {
        longitude: 13.3777,
        latitude: 52.5163,
        zoom: 14.0,
        width: 900.0,
        height: 700.0,
        bearing: 0.0,
        pitch: 0.0,
    };

    // Straight from the Mercator definition rather than from anything under test.
    let n = f64::from(1u32 << 14);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let x = ((view.longitude + 180.0) / 360.0 * n).floor() as u32;
    let lat = view.latitude.to_radians();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let y = ((1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / core::f64::consts::PI) / 2.0 * n).floor()
        as u32;

    let tiles = cover(&view).expect("a cover for a viewport with area");
    assert!(
        tiles
            .iter()
            .any(|tile| tile.x == x && tile.y == y && tile.wrap == 0),
        "cover {tiles:?} should contain the centre tile z14 {x},{y}"
    );
}
