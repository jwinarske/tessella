//! The camera at a pitch, which nothing checked until something was drawn at one.
//!
//! # Why there is no oracle here
//!
//! The golden dump is captured unpitched, so the rotated path has never been compared to mbgl
//! bit for bit and cannot be. What replaces that is transcription plus the properties below: the
//! unrotated case is *unchanged* — the orientation quaternion is the identity at zero pitch and
//! zero bearing, so every golden still holds — and the rotated case is checked against what a
//! perspective must do to a square on the ground.
//!
//! Both faults these catch were live, and both were invisible without a picture. The camera
//! never left the point directly above the map's centre, however the view was rotated; and the
//! pitch was read as radians when it is documented, and passed, in degrees.

use tessella_tile::camera::{self, camera_position, pitch_radians};
use tessella_tile::cover::ViewTransform;

fn view(pitch: f64, bearing: f64) -> ViewTransform {
    camera::settled(&ViewTransform {
        longitude: 13.4,
        latitude: 52.5,
        zoom: 14.0,
        width: 900.0,
        height: 700.0,
        bearing,
        pitch,
    })
}

/// Pitch is degrees on the way in and radians on the way out.
///
/// `MAX_PITCH` is a radian constant, so a clamp against it with degrees in hand compared a
/// number of degrees to 1.558 and let anything above about a degree and a half through as
/// 89.25°. Zero is zero either way, which is exactly why the goldens never noticed.
#[test]
fn pitch_is_degrees_in_and_radians_out() {
    assert_eq!(pitch_radians(&view(0.0, 0.0)), 0.0);
    assert!((pitch_radians(&view(45.0, 0.0)) - core::f64::consts::FRAC_PI_4).abs() < 1e-12);
    assert!((pitch_radians(&view(60.0, 0.0)) - core::f64::consts::FRAC_PI_3).abs() < 1e-12);
    // Past the horizon angle it clamps, rather than asking the projection for every tile there is.
    assert!(pitch_radians(&view(120.0, 0.0)) <= camera::MAX_PITCH);
}

/// The camera orbits the centre; it does not hover over it.
///
/// mbgl moves it back along its own forward direction by the centre distance. Staying overhead
/// while the view rotated put a tile several viewports away and mirrored in x — a map that had
/// simply swung out of frame.
#[test]
fn the_camera_orbits_rather_than_hovers() {
    let flat = camera_position(&view(0.0, 0.0));
    let pitched = camera_position(&view(55.0, 0.0));

    // Straight down: the camera is over the centre, at the centre distance.
    assert!(
        (pitched[0] - flat[0]).abs() < 1e-12,
        "no sideways swing at bearing zero"
    );
    assert!(
        pitched[1] > flat[1],
        "a pitched camera moves back from the centre: {} vs {}",
        pitched[1],
        flat[1]
    );
    assert!(
        pitched[2] < flat[2],
        "and drops as it does: {} vs {}",
        pitched[2],
        flat[2]
    );

    // Its distance from the centre is unchanged: it swings on a sphere, and `forward` is a unit
    // vector, so the orbit trades height for reach and nothing else.
    let distance = |p: [f64; 3]| {
        let (dx, dy, dz) = (p[0] - flat[0], p[1] - flat[1], p[2]);
        (dx * dx + dy * dy + dz * dz).sqrt()
    };
    assert!(
        (distance(pitched) - distance(flat)).abs() < 1e-9,
        "the orbit changed the camera's distance: {} vs {}",
        distance(pitched),
        distance(flat)
    );
}

/// Bearing swings the camera around the centre rather than only turning it.
#[test]
fn bearing_swings_the_camera_too() {
    let north = camera_position(&view(55.0, 0.0));
    let east = camera_position(&view(55.0, 90.0));
    assert!(
        (north[0] - east[0]).abs() > 1e-9,
        "a bearing left the camera where it was"
    );
    assert!(
        (north[2] - east[2]).abs() < 1e-9,
        "turning changed the camera's height"
    );
}

/// A square of ground becomes a trapezoid, near edge wider and lower.
///
/// The property that says the projection is a perspective at all, and the one that catches a
/// camera on the wrong side: a mirrored view puts the x order backwards, and a view along the
/// plane collapses the two edges together.
#[test]
fn a_tile_projects_as_a_trapezoid() {
    let view = view(55.0, 0.0);
    let matrix = camera::proj_matrix(&view).expect("a matrix");
    let tile = camera::multiply(
        &matrix,
        &camera::matrix_for_tile(14, 8802, 5373, 0, view.zoom),
    );

    let project = |x: f64, y: f64| -> [f64; 2] {
        let w = tile[3] * x + tile[7] * y + tile[15];
        [
            (tile[0] * x + tile[4] * y + tile[12]) / w,
            (tile[1] * x + tile[5] * y + tile[13]) / w,
        ]
    };
    let e = camera::EXTENT;
    let (far_left, far_right) = (project(0.0, 0.0), project(e, 0.0));
    let (near_left, near_right) = (project(0.0, e), project(e, e));

    assert!(
        far_left[0] < far_right[0] && near_left[0] < near_right[0],
        "x runs backwards, so the camera is on the wrong side of the map"
    );
    let far_width = far_right[0] - far_left[0];
    let near_width = near_right[0] - near_left[0];
    assert!(
        near_width > far_width * 1.05,
        "the near edge is not wider: {near_width} vs {far_width}"
    );
    // Clip space is y-up; a raster row index is not. So the *near* edge — the one a viewer sees
    // at the bottom of the screen — is the one with the smaller clip y.
    assert!(
        near_left[1] < far_left[1],
        "the near edge is not below the far one: {} vs {}",
        near_left[1],
        far_left[1]
    );
}

/// And at zero pitch it is a square again, which is what keeps every golden holding.
#[test]
fn an_unpitched_tile_is_still_square() {
    let view = view(0.0, 0.0);
    let matrix = camera::proj_matrix(&view).expect("a matrix");
    let tile = camera::multiply(
        &matrix,
        &camera::matrix_for_tile(14, 8802, 5373, 0, view.zoom),
    );
    let project = |x: f64, y: f64| -> [f64; 2] {
        let w = tile[3] * x + tile[7] * y + tile[15];
        [
            (tile[0] * x + tile[4] * y + tile[12]) / w,
            (tile[1] * x + tile[5] * y + tile[13]) / w,
        ]
    };
    let e = camera::EXTENT;
    let (far_left, far_right) = (project(0.0, 0.0), project(e, 0.0));
    let (near_left, near_right) = (project(0.0, e), project(e, e));
    assert!(((far_right[0] - far_left[0]) - (near_right[0] - near_left[0])).abs() < 1e-9);
    assert!((far_left[1] - far_right[1]).abs() < 1e-9);
}
