//! Cover hysteresis at an integer zoom boundary (§13.2).

use tessella_tile::cover::ZoomLatch;

/// A pinch wobbling across a boundary changes level once, not once per frame.
///
/// The case §13.2 names. A crossing is a burst — new cover, fetch, decode, layout, buffer
/// creation, possibly on four views at once — and a hand holding still near an integer zoom
/// crosses on whichever frames it happens to wobble across. Without a dead band that is a level
/// transition per frame for as long as the user does not move, and nothing looks wrong while it
/// happens: the tiles are all in the store, so the map is correct and the device is not idle.
#[test]
fn a_wobble_across_the_boundary_does_not_change_level() {
    let mut latch = ZoomLatch::new(13.5);
    assert_eq!(latch.level(), 13);

    // Sixty frames of a hand shaking either side of 14.0 by less than the band.
    let mut changes = 0;
    let mut previous = latch.level();
    for frame in 0..60 {
        let zoom = if frame % 2 == 0 { 13.96 } else { 14.04 };
        let level = latch.update(zoom);
        if level != previous {
            changes += 1;
            previous = level;
        }
    }
    assert_eq!(changes, 0, "a wobble rebuilt the cover {changes} times");
    assert_eq!(latch.level(), 13, "and it held the level it started on");
}

/// Deliberate zooming past the band does change level.
///
/// The companion: a latch that never moved would pass the test above and show one level forever.
#[test]
fn passing_the_band_changes_level() {
    let mut latch = ZoomLatch::new(13.5);

    assert_eq!(latch.update(14.0), 13, "at the boundary, still held");
    assert_eq!(latch.update(14.09), 13, "inside the band, still held");
    assert_eq!(latch.update(14.10), 14, "past the band, moved");
}

/// Coming back down holds the higher level until the zoom is below the band.
///
/// The hysteresis proper: the level that a rise settled on is not given up at the same zoom that
/// produced it, or the boundary is exactly as sharp as it was and the wobble returns one level up.
#[test]
fn falling_back_holds_the_level_it_reached() {
    let mut latch = ZoomLatch::with_margin(13.5, 0.1);
    assert_eq!(latch.update(14.2), 14, "risen");

    assert_eq!(latch.update(13.95), 14, "just below the boundary, still 14");
    assert_eq!(latch.update(13.91), 14, "inside the band, still 14");
    assert_eq!(latch.update(13.89), 13, "past it, back to 13");
}

/// A long jump lands where it was aimed rather than stepping.
///
/// The band is checked against the level held, not against distance travelled, so a fly-to is
/// not taxed for passing through levels it never rendered.
#[test]
fn a_jump_snaps_to_its_destination() {
    let mut latch = ZoomLatch::new(5.0);
    assert_eq!(latch.update(14.3), 14, "up nine levels in one frame");

    let mut down = ZoomLatch::new(14.0);
    assert_eq!(down.update(3.2), 3, "and back down");
}

/// Zero margin is the behaviour that existed before, so the type can express it.
#[test]
fn a_zero_margin_is_a_plain_floor() {
    let mut latch = ZoomLatch::with_margin(13.5, 0.0);
    assert_eq!(latch.update(13.99), 13);
    assert_eq!(latch.update(14.0), 14);
    assert_eq!(latch.update(13.99), 13);
}

/// A negative margin is treated as none rather than inverting the band.
///
/// A band below zero would mean the level changes *before* the boundary and then changes back,
/// which is the oscillation this exists to stop, arriving through a typo.
#[test]
fn a_negative_margin_is_clamped() {
    let mut latch = ZoomLatch::with_margin(13.5, -1.0);
    assert_eq!(latch.update(14.0), 14);
    assert_eq!(latch.update(13.99), 13);
}
