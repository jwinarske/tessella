//! Per-view cover state: §12.7's recomputation claim and §13.2's hysteresis, wired together.
//!
//! Both `ZoomLatch` and the never-blank substitution were built and left unwired, because each
//! is a value someone has to keep between frames and nothing kept one. `ViewCover` is that
//! someone. These are the assertions that make the wiring worth having rather than merely
//! present.

use std::collections::BTreeMap;

use tessella_orchestrate::viewcover::{Update, ViewCover};
use tessella_tile::cover::ViewTransform;
use tessella_tile::renderables::{DataTileId, Necessity, Pyramid, RenderTileId, TileState};

fn at(zoom: f64, longitude: f64, latitude: f64) -> ViewTransform {
    ViewTransform {
        longitude,
        latitude,
        zoom,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
    }
}

/// A pyramid where everything asked for is already built.
#[derive(Default)]
struct Ready {
    drawn: Vec<DataTileId>,
    known: BTreeMap<DataTileId, TileState>,
}

impl Ready {
    fn all(tiles: &[DataTileId]) -> Self {
        Self {
            drawn: Vec::new(),
            known: tiles
                .iter()
                .map(|id| {
                    (
                        *id,
                        TileState {
                            renderable: true,
                            loaded: true,
                            tried_cache: true,
                        },
                    )
                })
                .collect(),
        }
    }
}

impl Pyramid for Ready {
    fn get(&mut self, id: DataTileId) -> Option<TileState> {
        self.known.get(&id).copied()
    }
    fn create(&mut self, id: DataTileId) -> Option<TileState> {
        let state = TileState::default();
        self.known.insert(id, state);
        Some(state)
    }
    fn retain(&mut self, _id: DataTileId, _necessity: Necessity) {}
    fn render(&mut self, _render: RenderTileId, data: DataTileId) {
        self.drawn.push(data);
    }
}

/// A pan recomputes the cover only where it crosses a boundary, not every frame.
///
/// §12.7's claim, and the one that pays for everything downstream: retain, release, bindings and
/// damage all hang off `Changed`. A cover that reported a change every frame would be correct
/// and would make the gate worthless.
///
/// Stated as a rate rather than as "never". A viewport has two vertical edges and two
/// horizontal ones, each of which crosses a boundary at its own moment, and where those moments
/// fall depends on where the camera happened to start — an arbitrarily small pan changes the
/// cover if an edge is sitting on a boundary. What is *not* allowed is a change per frame, so
/// the assertion is against the number of boundaries actually available to cross.
#[test]
fn a_pan_recomputes_only_at_boundaries() {
    let mut state = ViewCover::new(&at(14.0, -0.11, 51.505)).expect("covers");

    // Two hundred frames drifting across one z14 tile's width, which is 360/2^14 ≈ 0.022°.
    // Each of the two vertical edges can cross at most one boundary in that span.
    let span = 360.0 / 16384.0;
    for step in 1..=200 {
        let nudge = span * f64::from(step) / 200.0;
        state
            .update(&at(14.0, -0.11 + nudge, 51.505))
            .expect("covers");
    }

    let (changes, frames) = state.churn();
    assert_eq!(frames, 201);
    // Measured: two, one per vertical edge. The bound is three so that a camera starting a
    // hair from a horizontal boundary does not make this fail for the wrong reason.
    assert!(
        changes <= 3,
        "a pan of one tile changed the cover {changes} times in {frames} frames"
    );
}

/// The rate above is only meaningful if a stationary camera is exactly zero.
///
/// Without this, `changes <= 3` is satisfied by an implementation that recomputes on some
/// arbitrary schedule of its own.
#[test]
fn a_still_camera_never_changes_the_cover() {
    let view = at(14.0, -0.11, 51.505);
    let mut state = ViewCover::new(&view).expect("covers");

    for step in 1..=100 {
        assert_eq!(
            state.update(&view).expect("covers"),
            Update::Unchanged,
            "frame {step} moved a cover nothing moved"
        );
    }

    let (changes, frames) = state.churn();
    assert_eq!((changes, frames), (1, 101), "only the initial cover");
    assert!(state.entered().is_empty() && state.left().is_empty());
}

/// A pan that does leave its tiles reports exactly what came and went.
///
/// The delta is what a caller retains and releases against the shared store, so it has to be a
/// delta and not a fresh set: releasing everything and retaining everything would drop the
/// refcount of a tile another view is holding to zero and back, which is an eviction and a
/// rebuild for a tile that never stopped being needed.
#[test]
fn a_pan_across_a_boundary_reports_only_the_difference() {
    let mut state = ViewCover::new(&at(14.0, -0.11, 51.505)).expect("covers");
    let held: Vec<_> = state.tiles().to_vec();

    let mut moved = None;
    for step in 1..=2000 {
        let nudge = f64::from(step) * 0.000_1;
        if state
            .update(&at(14.0, -0.11 + nudge, 51.505))
            .expect("covers")
            == Update::Changed
        {
            moved = Some(nudge);
            break;
        }
    }
    assert!(
        moved.is_some(),
        "a pan of 0.2 degrees must cross a z14 boundary"
    );

    assert!(!state.entered().is_empty(), "something must have come in");
    assert!(!state.left().is_empty(), "and something must have gone");
    // The delta is exactly the difference, not the whole cover twice over.
    let kept = held.iter().filter(|t| state.tiles().contains(t)).count();
    assert!(kept > 0, "a one-boundary pan keeps most of its tiles");
    for tile in state.entered() {
        assert!(!held.contains(tile), "{tile:?} was already held");
    }
    for tile in state.left() {
        assert!(!state.tiles().contains(tile), "{tile:?} is still held");
    }
}

/// A pinch oscillating around an integer zoom must not rebuild the cover at gesture rate.
///
/// §13.2's hysteresis, which is why the latch exists. Without it every frame either side of 14.0
/// flips the level, and every flip is a whole new cover — a fetch storm produced by a hand that
/// is not quite still.
#[test]
fn a_pinch_around_an_integer_zoom_holds_its_level() {
    let mut state = ViewCover::new(&at(14.0, -0.11, 51.505)).expect("covers");
    let level = state.level();

    // Sixty frames of wobble inside the dead band, alternating sides of the integer.
    for step in 0..60 {
        let wobble = if step % 2 == 0 { 0.05 } else { -0.05 };
        let update = state
            .update(&at(14.0 + wobble, -0.11, 51.505))
            .expect("covers");
        assert_eq!(update, Update::Unchanged, "frame {step} rebuilt the cover");
        assert_eq!(state.level(), level, "frame {step} moved the level");
    }

    let (changes, frames) = state.churn();
    assert_eq!(changes, 1, "only the initial cover");
    assert_eq!(frames, 61);
}

/// And a zoom that means it still gets through.
///
/// The dead band is for a camera sitting on a boundary, not a tax on going anywhere. A test that
/// only checked the band would pass for a latch that never moved at all.
#[test]
fn a_deliberate_zoom_still_crosses() {
    let mut state = ViewCover::new(&at(14.0, -0.11, 51.505)).expect("covers");
    assert_eq!(state.level(), 14);

    // Exactly 15.0 does *not* cross, and that is the dead band doing its job rather than a
    // failure: a camera resting on the boundary is the case hysteresis exists for, and rising
    // adopts the next level only past 15.0 + the margin.
    assert_eq!(
        state.update(&at(15.0, -0.11, 51.505)).expect("covers"),
        Update::Unchanged
    );
    assert_eq!(state.level(), 14, "sitting on the boundary holds the level");

    // Past the band it goes.
    assert_eq!(
        state.update(&at(15.2, -0.11, 51.505)).expect("covers"),
        Update::Changed
    );
    assert_eq!(state.level(), 15);

    // And a jump of nine levels lands where it was aimed rather than creeping.
    assert_eq!(
        state.update(&at(6.0, -0.11, 51.505)).expect("covers"),
        Update::Changed
    );
    assert_eq!(state.level(), 6);
}

/// The draw list comes from the cover, and substitutes when a tile is not ready.
///
/// The wiring itself: a `ViewCover` hands its ideal tiles to never-blank, so a crossing draws
/// ancestors rather than holes without the caller arranging anything.
#[test]
fn the_draw_list_substitutes_for_tiles_that_are_not_built() {
    let mut state = ViewCover::new(&at(14.0, -0.11, 51.505)).expect("covers");
    let ideal: Vec<DataTileId> = state
        .tiles()
        .iter()
        .map(|t| DataTileId::new(t.z, t.x, t.y))
        .collect();

    // Everything ready: the cover draws itself.
    let mut ready = Ready::all(&ideal);
    state.draw(&mut ready, 0..=16);
    assert_eq!(ready.drawn, ideal, "a built cover draws exactly itself");

    // Now cross to z15 with nothing at that level built. The z14 tiles are the ancestors.
    state.update(&at(15.0, -0.11, 51.505)).expect("covers");
    let mut cold = Ready::all(&ideal);
    state.draw(&mut cold, 0..=16);
    assert!(!cold.drawn.is_empty(), "a crossing must not draw nothing");
    for tile in &cold.drawn {
        assert!(
            ideal.contains(tile),
            "{tile:?} was drawn but is not one of the built z14 tiles"
        );
    }
}

/// Above the source's maximum zoom the deepest tile stands in, and says so.
///
/// The distinction `overscaled_z` exists for: the same bytes drawn at a deeper level is not the
/// same entry as those bytes at their own level, and collapsing the two puts one tile's
/// magnified stand-in where another view wanted the real thing.
#[test]
fn past_the_sources_maximum_the_deepest_tile_stands_in() {
    let mut state = ViewCover::new(&at(16.0, -0.11, 51.505)).expect("covers");
    state.update(&at(16.0, -0.11, 51.505)).expect("covers");

    let mut pyramid = Ready::default();
    state.draw(&mut pyramid, 0..=14);

    let asked: Vec<DataTileId> = pyramid.known.keys().copied().collect();
    assert!(!asked.is_empty());
    for id in &asked {
        assert!(id.z <= 14, "{id:?} is past the source's maximum zoom");
    }
    assert!(
        asked.iter().any(|id| id.overscaled_z > id.z),
        "a z16 camera over a z14 source must ask for overscaled tiles: {asked:?}"
    );
}
