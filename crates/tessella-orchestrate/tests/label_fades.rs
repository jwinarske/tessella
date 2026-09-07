//! A label fades rather than switching off.
//!
//! # What was wrong
//!
//! The fade increment was the constant one. `Fade::step` moves an opacity by that much per frame
//! and clamps, so one means a label reaches full opacity or full transparency in a single step:
//! there is no crossfade at all, and a label that stops being placed vanishes between two frames.
//!
//! That is not a bug against the oracle, which is why every parity render agreed. `mbgl-render`
//! runs in static map mode, and `Placement::symbolFadeChange` short-circuits to exactly one there.
//! Both sides were drawing a still picture with instant fades, and both were right.
//!
//! It is wrong for a map somebody is looking at. During a zoom the set of placed anchors changes
//! constantly -- a line label has an anchor every `symbol-spacing` along its road, and which of
//! them win depends on what else is on screen -- so a name stops being drawn at one anchor and
//! starts at another further along. With a crossfade that reads as one label yielding to another.
//! With none, it reads as the text having *flown* down the road.
//!
//! # What is asserted
//!
//! That time entering the map changes the rate, that the rate is mbgl's, and that a map told
//! nothing keeps the still-picture behaviour every capture depends on.

use tessella_orchestrate::frame::{FADE_DURATION_MILLIS, PlacementState};
use tessella_place::fade::Opacity;

/// The duration is mbgl's `DEFAULT_TRANSITION_DURATION`.
#[test]
fn the_fade_takes_mbgls_three_hundred_milliseconds() {
    assert!(
        (FADE_DURATION_MILLIS - 300.0).abs() < f64::EPSILON,
        "the fade duration is not mbgl's 300 ms"
    );
}

/// A frame's worth of time is a fraction of a fade, not all of it.
#[test]
fn a_frame_of_time_moves_a_fade_part_way() {
    let mut state = PlacementState::new();
    state.advance(1000.0 / 60.0);

    // One vsync at sixty is a twentieth of 300 ms, so a label needs about eighteen frames to
    // arrive rather than one.
    let step = state.increment();
    assert!(
        step > 0.0 && step < 0.1,
        "a 16.7 ms frame moved a fade by {step}, which is not a fade"
    );

    // And the ramp that step drives really is a ramp.
    // Stepped before the test, not after: a hidden fade that is not placed is already settled,
    // so a loop that checks first counts zero frames and proves nothing.
    let mut fade = Opacity::new(false, false);
    let mut frames = 0;
    loop {
        fade = fade.step(step, true);
        frames += 1;
        if fade.is_settled() || frames >= 1000 {
            break;
        }
    }
    assert!(
        (15..=25).contains(&frames),
        "a label took {frames} frames to fade in, against the eighteen 300 ms at sixty asks for"
    );
}

/// A map told nothing keeps the still-picture behaviour the captures compare.
#[test]
fn an_untold_map_fades_in_one_step() {
    let state = PlacementState::new();
    assert!(
        (state.increment() - 1.0).abs() < f32::EPSILON,
        "the default is not the static-mode one, so every parity render just changed"
    );

    let mut told = PlacementState::new();
    told.advance(16.7);
    told.settle_at_once();
    assert!(
        (told.increment() - 1.0).abs() < f32::EPSILON,
        "a map cannot be put back into still-picture mode"
    );
}

/// Time that is not a number does not stop the fades dead.
#[test]
fn a_nonsense_elapsed_falls_back_to_instant() {
    let mut state = PlacementState::new();
    state.advance(f64::NAN);
    assert!(
        (state.increment() - 1.0).abs() < f32::EPSILON,
        "a NaN elapsed left the increment somewhere a clamp cannot reach, so every fade stalls"
    );
    state.advance(-5.0);
    assert!(
        state.increment() >= 0.0,
        "time running backwards drove the fades backwards"
    );
}
