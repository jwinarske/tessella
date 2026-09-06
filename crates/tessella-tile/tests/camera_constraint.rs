//! The camera a map accepts keeps the world over the viewport.
//!
//! # What it is
//!
//! mbgl's `TransformState::constrain`, which its Transform applies to every camera change and
//! states its purpose in one line: "Constrain scale to avoid zooming out far enough to show
//! off-world areas on the Y axis". Mercator ends at the poles and `util::tileCover` has no tiles
//! past them, so a frame aimed past the edge paints the background there and nothing else.
//!
//! # What it caught
//!
//! A quad panning to zoom zero under pitch, with a strip of background across the top of the
//! northern panes. The renderer was right — the same camera through `mbgl-render`, which does not
//! constrain, produces that strip to the pixel — and the camera was the thing nothing was
//! checking.
//!
//! # Why the pitch term
//!
//! `constrain` reads the viewport's height as the ground it covers, which is true flat and false
//! under pitch: the top of the screen looks further than the bottom by an amount set by the pitch
//! and the field of view. mbgl leaves that open. The first test below is what pins this to
//! mbgl's arithmetic where mbgl's arithmetic applies.

use tessella_tile::camera::{self, constrained, mercator_fraction};
use tessella_tile::cover::ViewTransform;

fn view(zoom: f64, latitude: f64, pitch: f64, height: f64) -> ViewTransform {
    ViewTransform {
        longitude: 0.0,
        latitude,
        zoom,
        width: 960.0,
        height,
        bearing: 0.0,
        pitch,
    }
}

/// Flat and on the equator, this is `constrain`'s own clause: `scale >= height / tileSize`.
#[test]
fn a_flat_camera_takes_mbgls_own_floor() {
    // A viewport twice the world's height at zoom zero, so the floor binds and its value is
    // known independently: scale 2, which is zoom 1.
    let out = constrained(&view(0.0, 0.0, 0.0, 1024.0));
    assert!(
        (out.zoom - 1.0).abs() < 1e-9,
        "zoom {} is not mbgl's floor of 1 for a 1024px viewport",
        out.zoom
    );
    // And a viewport the world already covers is left alone.
    let loose = view(0.0, 0.0, 0.0, 256.0);
    assert!(
        (constrained(&loose).zoom - loose.zoom).abs() < 1e-12,
        "a camera that shows no off-world area was moved"
    );
}

/// An ordinary camera is not touched at all.
#[test]
fn a_working_camera_is_returned_as_given() {
    let held = view(14.25, 47.6062, 15.0, 359.0);
    let out = constrained(&held);
    assert!((out.zoom - held.zoom).abs() < 1e-12, "the zoom moved");
    assert!(
        (out.latitude - held.latitude).abs() < 1e-12,
        "the latitude moved"
    );
}

/// Pitched at a northern latitude, the frustum is brought back inside the world.
///
/// The quad's own case: Seattle at zoom zero in a 359-pixel pane at fifteen degrees. Flat, the
/// world covers that viewport and `constrain` would pass it; pitched, the top of the screen
/// reaches past the pole.
#[test]
fn a_pitched_camera_is_pulled_back_inside_the_world() {
    let held = view(0.0, 47.6062, 15.0, 359.0);
    let out = constrained(&held);

    // The extents this has to satisfy, computed the way the constraint does.
    let half_fov = camera::DEFAULT_FOV / 2.0;
    let pitch = out.pitch.to_radians();
    let above = camera::camera_to_center_distance(out.height) * pitch.cos();
    let centre = above * pitch.tan();
    let north = above * (pitch + half_fov).tan() - centre;
    let south = centre - above * (pitch - half_fov).tan();

    let world = camera::world_size(out.zoom);
    let from_north = world * mercator_fraction(out.latitude);
    assert!(
        from_north >= north - 1e-6,
        "the top of the screen still reaches {} pixels past the north edge",
        north - from_north
    );
    assert!(
        world - from_north >= south - 1e-6,
        "the bottom of the screen still reaches {} pixels past the south edge",
        south - (world - from_north)
    );

    // Flat, the same camera needs nothing: this is the gap mbgl's arithmetic leaves.
    let flat = constrained(&view(0.0, 47.6062, 0.0, 359.0));
    assert!(
        (flat.zoom - 0.0).abs() < 1e-12,
        "the flat camera was moved, so the pitched case proves nothing"
    );
}

/// A camera past the horizon is returned rather than fabricated.
#[test]
fn a_camera_with_no_ground_under_its_edge_is_left_alone() {
    let held = view(0.0, 0.0, 89.0, 359.0);
    let out = constrained(&held);
    assert!((out.zoom - held.zoom).abs() < 1e-12, "the zoom moved");
    let empty = view(0.0, 0.0, 15.0, 0.0);
    assert!(
        (constrained(&empty).zoom - empty.zoom).abs() < 1e-12,
        "a viewport with no height was given a constraint"
    );
}
