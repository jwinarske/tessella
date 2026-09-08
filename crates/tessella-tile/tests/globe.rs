//! The globe projection, checked against identities rather than an oracle.
//!
//! MapLibre Native has no globe, so there is nothing to render against. What can be pinned is what
//! the geometry has to be true of regardless of implementation: the poles, the round trip, the
//! antimeridian, and the horizon's tangent.

use tessella_tile::camera;
use tessella_tile::globe;

/// Everything the projection produces is on the unit sphere.
#[test]
fn every_point_is_on_the_sphere() {
    for &longitude in &[-180.0, -90.0, -1.0, 0.0, 1.0, 90.0, 179.9] {
        for &latitude in &[-85.0, -45.0, 0.0, 45.0, 85.0] {
            let p = globe::sphere_point(longitude, latitude);
            let length = globe::dot(p, p).sqrt();
            assert!(
                (length - 1.0).abs() < 1e-12,
                "({longitude}, {latitude}) has length {length}"
            );
        }
    }
}

/// The origin faces `+z`, the poles are on `∓y`, and east is `+x`.
///
/// GL JS's convention, and the reason `y` is negated: it is the screen's axis, not the Earth's.
#[test]
fn the_axes_are_where_gl_js_puts_them() {
    let origin = globe::sphere_point(0.0, 0.0);
    assert!((origin[0]).abs() < 1e-12 && (origin[1]).abs() < 1e-12);
    assert!((origin[2] - 1.0).abs() < 1e-12, "(0,0) is +z: {origin:?}");

    let north = globe::sphere_point(0.0, 90.0);
    assert!(
        (north[1] + 1.0).abs() < 1e-12,
        "the north pole is -y: {north:?}"
    );
    let south = globe::sphere_point(0.0, -90.0);
    assert!(
        (south[1] - 1.0).abs() < 1e-12,
        "the south pole is +y: {south:?}"
    );

    let east = globe::sphere_point(90.0, 0.0);
    assert!((east[0] - 1.0).abs() < 1e-12, "90°E is +x: {east:?}");
}

/// A pole is one point however you spell its longitude.
#[test]
fn the_pole_is_one_place() {
    let a = globe::sphere_point(0.0, 90.0);
    for &longitude in &[-180.0, -37.0, 55.0, 180.0] {
        let b = globe::sphere_point(longitude, 90.0);
        for axis in 0..3 {
            assert!(
                (a[axis] - b[axis]).abs() < 1e-12,
                "the north pole moved at {longitude}: {a:?} against {b:?}"
            );
        }
    }
}

/// The antimeridian's two spellings are the same point.
#[test]
fn the_antimeridian_meets_itself() {
    let west = globe::sphere_point(-180.0, 20.0);
    let east = globe::sphere_point(180.0, 20.0);
    for axis in 0..3 {
        assert!(
            (west[axis] - east[axis]).abs() < 1e-12,
            "-180 and 180 differ: {west:?} against {east:?}"
        );
    }
}

/// Going through Mercator and back returns the latitude it started from.
///
/// The bend takes tile geometry, which is Mercator, so this is the composition that actually runs.
#[test]
fn mercator_round_trips_through_the_sphere() {
    for &latitude in &[-80.0, -45.0, -0.5, 0.0, 0.5, 45.0, 80.0] {
        let y = camera::mercator_fraction(latitude);
        let direct = globe::sphere_point(30.0, latitude);
        let via_mercator = globe::sphere_point_from_mercator((30.0 + 180.0) / 360.0, y);
        for axis in 0..3 {
            assert!(
                (direct[axis] - via_mercator[axis]).abs() < 1e-9,
                "latitude {latitude} bent differently through Mercator: \
                 {direct:?} against {via_mercator:?}"
            );
        }
    }
}

/// Mercator has no poles, so a tile edge past its limit clamps rather than diverging.
#[test]
fn past_the_mercator_limit_clamps_to_the_pole() {
    let over = globe::sphere_point_from_mercator(0.5, -0.25);
    let north = globe::sphere_point(0.0, camera::latitude_of(0.0));
    for axis in 0..3 {
        assert!(
            (over[axis] - north[axis]).abs() < 1e-12,
            "a vertex above the world did not clamp: {over:?} against {north:?}"
        );
    }
}

/// A point exactly on the horizon is exactly on the horizon.
///
/// For a unit sphere seen from `d` radii out, the tangent grazes at `cos θ = 1/d`. So a camera two
/// radii out sees exactly the cap within sixty degrees of the point beneath it.
#[test]
fn the_horizon_is_where_the_tangent_grazes() {
    let toward = [0.0, 0.0, 1.0];
    let distance = 2.0;

    let just_inside = globe::sphere_point(59.9, 0.0);
    let just_outside = globe::sphere_point(60.1, 0.0);
    assert!(
        globe::faces_camera(just_inside, toward, distance),
        "59.9° from the sub-camera point is visible at two radii"
    );
    assert!(
        !globe::faces_camera(just_outside, toward, distance),
        "60.1° is not"
    );

    // And the further out the camera, the more of the sphere it sees.
    let far = globe::sphere_point(80.0, 0.0);
    assert!(!globe::faces_camera(far, toward, 2.0));
    assert!(
        globe::faces_camera(far, toward, 100.0),
        "a distant camera sees 80°"
    );
}

/// The camera is always outside the sphere, and closer as the zoom rises.
#[test]
fn the_camera_backs_off_as_the_zoom_falls() {
    let mut previous = f64::INFINITY;
    for zoom in 0..12 {
        let distance = globe::camera_distance(f64::from(zoom));
        assert!(
            distance > 1.0,
            "the camera is outside the surface at z{zoom}"
        );
        assert!(
            distance < previous,
            "z{zoom} is not nearer than the zoom before it: {distance} against {previous}"
        );
        previous = distance;
    }
}
