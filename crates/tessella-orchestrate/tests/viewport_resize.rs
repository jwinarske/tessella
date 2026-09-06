//! A window resize is not a new map.
//!
//! # What this replaces
//!
//! Nothing, which was the problem. `width` and `height` were settable only at construction, so a
//! consumer whose surface changed had one option: destroy the map and build another. In the
//! Filament producer that is exactly what happened -- `ResizeOnThread` calls `Release()`, which
//! calls `DestroyView()`, which detaches the view extension and takes the map with it. Every tile
//! refetched, every bucket rebuilt, every glyph re-shaped, for a change that moves no camera.
//!
//! # The half that is easy to miss
//!
//! A resize has to *register*. Nothing else in the camera key moves when a window is resized --
//! the centre, zoom, bearing, pitch and scale are all unchanged -- so a map that merely accepted
//! a new size would report a settled camera and go on drawing through the matrices of the old
//! viewport. The viewport is part of the key for that reason, and the second test is what holds
//! it there.

use tessella_orchestrate::frame::camera_key_of;
use tessella_tile::camera;
use tessella_tile::cover::ViewTransform;

fn view(width: f64, height: f64) -> ViewTransform {
    camera::settled(&ViewTransform {
        longitude: 13.404,
        latitude: 52.52,
        zoom: 14.0,
        width,
        height,
        bearing: 0.0,
        pitch: 0.0,
    })
}

/// A resized camera is a different camera.
///
/// Every other field is identical across these two, which is the point: if the viewport were not
/// in the key, the frame after a resize would send nothing and the projection would be a viewport
/// out of date.
#[test]
fn a_resize_is_a_camera_change() {
    let before = camera_key_of(&view(1024.0, 768.0));
    let after = camera_key_of(&view(800.0, 768.0));
    assert!(
        !before.same_as(&after),
        "a width change did not register, so the frame after a resize sends no new matrices"
    );

    let taller = camera_key_of(&view(1024.0, 900.0));
    assert!(!before.same_as(&taller), "a height change did not register");

    // And a camera that did not move still does not, so the gate this widens has not been opened
    // to everything: the point of the key is that a settled view says nothing.
    assert!(
        before.same_as(&camera_key_of(&view(1024.0, 768.0))),
        "the same viewport reports a changed camera, so a settled map now emits every tick"
    );
}

/// A resize keeps the camera it was pointed at.
#[test]
fn a_resize_moves_nothing_but_the_viewport() {
    let before = view(1024.0, 768.0);
    let after = camera::settled(&camera::constrained(&ViewTransform {
        width: 800.0,
        height: 600.0,
        ..before
    }));
    assert!((after.zoom - before.zoom).abs() < 1e-12, "the zoom moved");
    assert!(
        (after.longitude - before.longitude).abs() < 1e-9
            && (after.latitude - before.latitude).abs() < 1e-9,
        "the centre moved"
    );
    assert!((after.width - 800.0).abs() < 1e-12 && (after.height - 600.0).abs() < 1e-12);
}

/// Shrinking a viewport still cannot leave it looking past the pole.
///
/// The constraint's floor is a function of the viewport's height, so a resize is a camera change
/// that can violate it: a view that was legal at 768 pixels tall is not necessarily legal at 200.
/// A resize that skipped the constraint would be the one way left to reach the state
/// `camera::constrained` exists to make unreachable.
#[test]
fn a_resize_is_constrained_like_any_other_camera_change() {
    // Zoom zero in a tall viewport: the world is 512 pixels and the viewport is more, so the
    // floor binds and the constraint has something to do.
    let asked = ViewTransform {
        height: 1024.0,
        ..view(1024.0, 768.0)
    };
    let asked = ViewTransform { zoom: 0.0, ..asked };
    let got = camera::constrained(&asked);
    assert!(
        got.zoom > asked.zoom,
        "a resize into a viewport taller than the world left the zoom below the floor: {} \
         against an asked {}",
        got.zoom,
        asked.zoom
    );
}
