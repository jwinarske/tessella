//! Symbol fades, and the silence they have to settle into.
//!
//! The arithmetic is mbgl's `OpacityState`. What is asserted beyond it is §6.5's guarantee: a
//! map whose camera has stopped and whose symbols have arrived must go completely quiet, and a
//! fade that never quite reaches 1.0 would keep it awake forever.

use tessella_place::fade::{DEFAULT_FADE_SECONDS, Fades, Joint, Opacity, increment};

/// A symbol appearing fades in from nothing.
#[test]
fn a_new_symbol_starts_transparent() {
    let state = Opacity::new(true, false);
    assert_eq!(state.opacity, 0.0);
    assert!(state.placed);
    assert!(!state.is_hidden(), "it is on its way in, not hidden");
    assert!(!state.is_settled());
}

/// A symbol that scrolled in from just outside the viewport appears at once.
///
/// It was never visible, so there is nothing to fade from. Fading it in would show a label
/// materialising where one had simply scrolled into view.
#[test]
fn a_symbol_entering_from_offscreen_skips_the_fade() {
    let state = Opacity::new(true, true);
    assert_eq!(state.opacity, 1.0);
    assert!(state.is_settled());

    // Not placed, so there is nothing to skip to.
    assert_eq!(Opacity::new(false, true).opacity, 0.0);
}

/// Opacity climbs to one and stops there.
#[test]
fn a_fade_in_reaches_one_and_stays() {
    let mut state = Opacity::new(true, false);
    for _ in 0..4 {
        state = state.step(0.3, true);
    }
    assert_eq!(state.opacity, 1.0, "clamped, not overshooting");
    assert!(state.is_settled());

    // Another step changes nothing, which is what lets a settled map go quiet.
    assert_eq!(state.step(0.3, true), state);
}

/// The direction of a fade comes from the previous frame, not the current one.
///
/// mbgl steps by `prev.placed ? +increment : -increment` and only then stores the new placement.
/// So the frame a symbol loses its collision, its opacity still rises; it falls from the next.
/// That lag is what stops a label flickering when a collision result oscillates.
#[test]
fn a_fade_turns_around_one_frame_late() {
    let start = Opacity::new(true, false).step(0.25, true);
    assert_eq!(start.opacity, 0.25);

    // The symbol loses its collision. Opacity still rises, because the *previous* frame had it
    // placed — but the placement is recorded as false.
    let losing = start.step(0.25, false);
    assert_eq!(losing.opacity, 0.5, "still rising on the turn-around frame");
    assert!(!losing.placed);

    // From here it falls.
    let falling = losing.step(0.25, false);
    assert_eq!(falling.opacity, 0.25);
}

/// A symbol that has faded out is hidden; one on its way out is not.
#[test]
fn hidden_means_gone_rather_than_going() {
    let mut state = Opacity::new(true, true);
    assert!(!state.is_hidden());

    state = state.step(0.5, false); // turn-around frame: rises, records unplaced
    assert!(!state.is_hidden(), "still drawing");

    state = state.step(1.0, false);
    assert_eq!(state.opacity, 0.0);
    assert!(state.is_hidden(), "faded out and not coming back");
}

/// Text and icon fade independently.
///
/// `text-optional` and `icon-optional` exist precisely so one can be dropped while the other
/// stays, so a shared opacity would make one channel's collision hide the other.
#[test]
fn text_and_icon_fade_independently() {
    let mut joint = Joint::new(true, true, true);
    assert!(joint.is_settled());

    // The icon loses its collision; the text keeps it.
    joint = joint.step(0.5, true, false);
    joint = joint.step(0.5, true, false);

    assert_eq!(joint.text.opacity, 1.0, "the text is unaffected");
    assert!(joint.icon.opacity < 1.0, "the icon is on its way out");
    assert!(!joint.is_settled());
}

/// A whole frame of fades settles, and then stays settled.
///
/// §6.5's still-frame guarantee, stated where it is decided. The counter reaching zero is what
/// lets the producer stop sending.
#[test]
fn a_frame_of_fades_settles_and_goes_quiet() {
    let mut fades = Fades::new();
    let placements: Vec<(u32, bool, bool)> = (0..5).map(|id| (id, true, true)).collect();

    // Nothing has faded yet, so everything is moving.
    fades.step(0.25, placements.clone(), false);
    assert_eq!(fades.len(), 5);
    assert_eq!(fades.fading(), 5);
    assert!(!fades.settled());

    // Four frames at a quarter each reaches opaque.
    for _ in 0..4 {
        fades.step(0.25, placements.clone(), false);
    }
    assert_eq!(fades.fading(), 0, "every symbol has arrived");
    assert!(fades.settled());

    // And stepping again keeps it settled, which is the part that matters: a fade that never
    // quite reached one would keep the map awake forever.
    for _ in 0..10 {
        fades.step(0.25, placements.clone(), false);
        assert!(fades.settled(), "a settled frame started moving again");
    }
}

/// An empty map is settled.
#[test]
fn nothing_to_fade_is_settled() {
    let fades = Fades::new();
    assert!(fades.settled());
    assert_eq!(fades.fading(), 0);
    assert!(fades.is_empty());
}

/// A symbol that has faded away is forgotten.
///
/// Keeping it would grow the map for the life of the process, one entry per label that ever
/// scrolled off the screen.
#[test]
fn a_faded_symbol_is_dropped() {
    let mut fades = Fades::new();
    fades.step(1.0, [(1u32, true, true)], true);
    assert_eq!(fades.len(), 1);

    // Turn-around frame, then out.
    fades.step(1.0, [(1u32, false, false)], false);
    assert_eq!(fades.len(), 1, "still drawing on the turn-around frame");
    fades.step(1.0, [(1u32, false, false)], false);
    assert_eq!(fades.len(), 0, "faded out and dropped");
}

/// A symbol not in this frame's placements is gone, not faded.
///
/// Its tile was released, so there is nothing left to draw it from — fading it would draw
/// geometry that no longer exists.
#[test]
fn a_symbol_whose_tile_went_away_is_dropped() {
    let mut fades = Fades::new();
    fades.step(0.5, [(1u32, true, true), (2, true, true)], false);
    assert_eq!(fades.len(), 2);

    fades.step(0.5, [(1u32, true, true)], false);
    assert_eq!(fades.len(), 1);
    assert!(fades.get(2).is_none());
    assert!(fades.get(1).is_some());
}

/// A symbol keeps its opacity across a frame, which is what stops a pop at a zoom crossing.
///
/// The state is keyed by cross-tile id, so the same label arriving in a new tile is the same
/// label. §13.3 asks for zero symbol pops, and re-fading a label that never left is one.
#[test]
fn a_symbol_keeps_its_opacity_across_frames() {
    let mut fades = Fades::new();
    fades.step(0.25, [(7u32, true, true)], false);
    fades.step(0.25, [(7u32, true, true)], false);

    let held = fades.get(7).expect("tracked");
    assert_eq!(held.text.opacity, 0.25, "one step in, not restarted");
}

/// With transitions off, everything snaps.
///
/// A duration of zero must not be divided by: the infinity it produces propagates into every
/// opacity and turns them all into NaN on the next clamp.
#[test]
fn transitions_off_means_an_increment_of_one() {
    assert_eq!(increment(0.016, 0.0), 1.0);
    assert!(increment(0.016, DEFAULT_FADE_SECONDS).is_finite());

    // A sixtieth of a second against mbgl's 300 ms.
    let step = increment(1.0 / 60.0, DEFAULT_FADE_SECONDS);
    assert!((step - 0.0555).abs() < 1e-3, "{step}");

    let mut state = Opacity::new(true, false);
    state = state.step(increment(0.016, 0.0), true);
    assert_eq!(state.opacity, 1.0, "snapped straight to opaque");
}
