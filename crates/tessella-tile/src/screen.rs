// SPDX-License-Identifier: BSD-2-Clause
//! Between the map and the viewport: `TransformState`'s two coordinate conversions.
//!
//! Everything else in this crate goes one way. A cover projects a camera onto the plane, a tile
//! matrix places geometry in clip space, and nothing asks where a screen pixel landed. The
//! location indicator is what asks: mbgl sizes a puck by measuring *one screen pixel* in world
//! pixels at the puck's own position, and shifts its shadow along the direction "up the screen"
//! expressed back in world pixels. Both are a round trip through the camera.
//!
//! # Why the inverse is a line and not a matrix
//!
//! A screen coordinate is two numbers and the map is a plane in three. Inverting the projection
//! gives a *ray*, and where the ray meets `z = 0` is the answer -- which is why
//! [`from_screen`] unprojects the point twice, at the near and far plane, and interpolates
//! between them. mbgl does exactly this in `screenCoordinateToTileCoordinate`, and the `t` it
//! computes is the parameter along that ray.
//!
//! A ray that never reaches the plane -- a pitched camera looking at or above the horizon --
//! has no answer, and mbgl's `z0 <= z1 ? 0 : ...` answers with the near point instead. That is
//! transcribed rather than corrected: the two renderers have to agree about a degenerate camera
//! as much as about an ordinary one, and a caller that wants to refuse one can ask the pitch.
//!
//! # y grows *upward*, from the bottom edge
//!
//! Not the top. World pixels run south as y grows, `world_to_camera` negates y so that north
//! reaches the top of clip space, mbgl's pixel matrix negates it again to give pixels from the
//! top -- and then `latLngToScreenCoordinate` subtracts the result from the height, which turns
//! it over a third time. At zoom 14 over Berlin, a point 0.03 degrees north of a 768-pixel
//! viewport's center comes back at y 1533 and one the same distance south at -764.
//!
//! Checked against mbgl rather than derived: `TransformState::latLngToScreenCoordinate` answers
//! `512, 1533.2315` and `512, -764.4470` for those two points, which is this to eight figures.
//!
//! This is `TransformState`'s convention and not the one a caller is likely to expect, because
//! mbgl's *public* pair is the other one -- `Transform::latLngToScreenCoordinate` takes the
//! state's answer and subtracts it from the height again, and `Map::pixelForLatLng` is that.
//! Anything working in viewport pixels, y down from the top left, is one `height - y` away from
//! here, and callers that transcribe mbgl code have to know which of the two that code is in.
//! The location indicator is the example: it declares its own flipped pair beside the state's
//! and works in viewport coordinates throughout, which is why its "moving it to bottom" comment
//! means the bottom even though `y = height - 1` is the top of this space.

use crate::camera::{self, CameraError, Mat4};
use crate::cover::ViewTransform;
use crate::projection::{self, TILE_SIZE};

/// The matrix that takes a point in world pixels over [`TILE_SIZE`] to viewport pixels.
///
/// mbgl's `coordinatePointMatrix`: the projection, a scale back up by the tile size, and the
/// pixel matrix over both. The scale and the division on the way in cancel; they are kept
/// because the matrix itself is what `getInvertedMatrix` inverts, and an inverse of a different
/// matrix is a different answer.
///
/// # Errors
///
/// [`CameraError`] when the view has no area, which is [`camera::proj_matrix`]'s.
pub fn coord_matrix(view: &ViewTransform) -> Result<Mat4, CameraError> {
    let projection = camera::proj_matrix(view)?;
    let scaled = camera::scale(&projection, TILE_SIZE, TILE_SIZE, 1.0);
    Ok(camera::multiply(&pixel_matrix(view), &scaled))
}

/// mbgl's `getPixelMatrix`: clip space to viewport pixels, y already flipped.
fn pixel_matrix(view: &ViewTransform) -> Mat4 {
    let mut matrix = camera::identity();
    matrix = camera::scale(&matrix, view.width / 2.0, -view.height / 2.0, 1.0);
    camera::translate_in_place(&mut matrix, 1.0, -1.0, 0.0);
    matrix
}

/// Where a coordinate lands on the screen, in pixels from the bottom left -- see the module note,
/// which is the one thing about this worth reading twice.
///
/// `None` when the view has no area. A point behind the camera comes back with a negative `w`
/// and so with coordinates that are a reflection rather than a position; mbgl returns them
/// too, and a caller that cares has to test the pitch or the result itself.
#[must_use]
pub fn to_screen(view: &ViewTransform, longitude: f64, latitude: f64) -> Option<[f64; 2]> {
    let matrix = coord_matrix(view).ok()?;
    let world = projection::project(longitude, latitude, camera::world_size(view.zoom));
    let point = [world[0] / TILE_SIZE, world[1] / TILE_SIZE, 0.0, 1.0];
    let out = transform(&matrix, point);
    Some([out[0] / out[3], view.height - out[1] / out[3]])
}

/// Which coordinate a screen pixel is over, as longitude then latitude.
///
/// The ray through the pixel, met with the map plane. See the module note for what happens when
/// it does not meet it.
///
/// `None` when the view has no area or its projection will not invert.
#[must_use]
pub fn from_screen(view: &ViewTransform, point: [f64; 2]) -> Option<[f64; 2]> {
    let inverted = camera::invert(&coord_matrix(view).ok()?)?;

    // The y mbgl flips back before unprojecting, which is the second half of the pair the module
    // note describes.
    let flipped = view.height - point[1];
    let near = transform(&inverted, [point[0], flipped, 0.0, 1.0]);
    let far = transform(&inverted, [point[0], flipped, 1.0, 1.0]);

    let (w0, w1) = (near[3], far[3]);
    if w0 == 0.0 || w1 == 0.0 {
        return None;
    }
    let p0 = [near[0] / w0, near[1] / w0];
    let p1 = [far[0] / w1, far[1] / w1];
    let z0 = near[2] / w0;
    let z1 = far[2] / w1;
    // mbgl's own guard, and its own answer: a ray that does not descend toward the plane stops
    // at the near point rather than being refused.
    let t = if z0 <= z1 {
        0.0
    } else {
        (0.0 - z0) / (z1 - z0)
    };

    // `interpolate(p0, p1, t) / scale`, which leaves a fraction of the world rather than a
    // position in it -- and a fraction is what an unprojection over a unit world takes.
    let scale = view.zoom.exp2();
    let fraction = [
        (p0[0] + (p1[0] - p0[0]) * t) / scale,
        (p0[1] + (p1[1] - p0[1]) * t) / scale,
    ];
    let (longitude, latitude) = projection::unproject(fraction, 1.0);
    Some([longitude, latitude])
}

/// How many world pixels one screen pixel covers at a coordinate.
///
/// mbgl's `pixelSizeToWorldSizeH`, measured horizontally: the coordinate, and the coordinate one
/// screen pixel to its left, projected and subtracted. Under pitch this varies up the screen --
/// which is the whole reason it is measured at the puck rather than taken from the zoom.
///
/// `None` rather than one when the view will not project. One is not a neutral answer: it is the
/// answer for a flat camera, so a caller that took it would size a puck as though the map were
/// unpitched and have nothing to say why.
#[must_use]
pub fn world_pixels_per_screen_pixel(
    view: &ViewTransform,
    longitude: f64,
    latitude: f64,
) -> Option<f64> {
    let screen = to_screen(view, longitude, latitude)?;
    let left = from_screen(view, [screen[0] - 1.0, screen[1]])?;
    let world = camera::world_size(view.zoom);
    let here = projection::project(longitude, latitude, world);
    let there = projection::project(left[0], left[1], world);
    Some((here[0] - there[0]).hypot(here[1] - there[1]))
}

/// A 4-vector through a matrix, no divide.
fn transform(matrix: &Mat4, point: [f64; 4]) -> [f64; 4] {
    core::array::from_fn(|row| {
        matrix[row] * point[0]
            + matrix[4 + row] * point[1]
            + matrix[8 + row] * point[2]
            + matrix[12 + row] * point[3]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(pitch: f64, bearing: f64) -> ViewTransform {
        ViewTransform {
            longitude: 13.405,
            latitude: 52.52,
            zoom: 14.0,
            width: 1024.0,
            height: 768.0,
            bearing,
            pitch,
        }
    }

    /// The center of the map is the center of the screen, at every camera.
    #[test]
    fn the_center_is_the_center() {
        for (pitch, bearing) in [(0.0, 0.0), (60.0, 0.0), (0.0, 38.0), (45.0, 200.0)] {
            let view = view(pitch, bearing);
            let screen = to_screen(&view, view.longitude, view.latitude).expect("a screen point");
            assert!(
                (screen[0] - view.width / 2.0).abs() < 1e-6,
                "{pitch} {bearing} {screen:?}"
            );
            assert!(
                (screen[1] - view.height / 2.0).abs() < 1e-6,
                "{pitch} {bearing} {screen:?}"
            );
        }
    }

    /// And a screen pixel unprojects to the coordinate that projects back onto it.
    ///
    /// Round-tripped rather than compared to a constant: the pair is an inverse or it is not,
    /// and a constant would only pin one of the two.
    #[test]
    fn the_two_directions_invert_each_other() {
        for (pitch, bearing) in [(0.0, 0.0), (60.0, 0.0), (0.0, 38.0), (45.0, 200.0)] {
            let view = view(pitch, bearing);
            // Below the center on a pitched camera, which is the half that reaches the plane.
            for point in [[512.0, 400.0], [200.0, 700.0], [900.0, 500.0]] {
                let here = from_screen(&view, point).expect("a coordinate");
                let back = to_screen(&view, here[0], here[1]).expect("a screen point");
                assert!(
                    (back[0] - point[0]).abs() < 1e-6 && (back[1] - point[1]).abs() < 1e-6,
                    "{pitch} {bearing} {point:?} -> {here:?} -> {back:?}"
                );
            }
        }
    }

    /// North-up and flat, a screen pixel is a world pixel. It is the pitched camera that makes
    /// the measurement worth taking.
    #[test]
    fn a_flat_camera_has_one_world_pixel_per_screen_pixel() {
        let flat = world_pixels_per_screen_pixel(&view(0.0, 0.0), 13.405, 52.52).expect("a size");
        assert!((flat - 1.0).abs() < 1e-6, "{flat}");
    }

    /// Under pitch the ground stretches away from the camera, so a screen pixel nearer the
    /// horizon covers more ground than one under the viewer's feet.
    ///
    /// Walking y *upward*, which is away from the camera -- see the module note. Measured through
    /// the coordinates those pixels are over rather than asserted about a number, since the ratio
    /// is the camera's and not a constant worth writing down.
    #[test]
    fn a_pitched_camera_stretches_toward_the_horizon() {
        let view = view(60.0, 0.0);
        let sizes = [300.0, 400.0, 500.0, 600.0].map(|y| {
            let at = from_screen(&view, [512.0, y]).expect("a coordinate");
            world_pixels_per_screen_pixel(&view, at[0], at[1]).expect("a size")
        });
        for pair in sizes.windows(2) {
            assert!(pair[1] > pair[0], "{sizes:?}");
        }
        // And at the center it is the flat camera's, because that is where the camera points.
        let middle =
            world_pixels_per_screen_pixel(&view, view.longitude, view.latitude).expect("a size");
        assert!((middle - 1.0).abs() < 1e-6, "{middle}");
    }

    /// North is up, which is the one thing the module note's three flips have to come out to.
    #[test]
    fn north_is_up_the_screen() {
        let flat = view(0.0, 0.0);
        let north = to_screen(&flat, flat.longitude, 52.55).expect("a screen point");
        let south = to_screen(&flat, flat.longitude, 52.49).expect("a screen point");
        assert!(north[1] > flat.height / 2.0, "{north:?}");
        assert!(south[1] < flat.height / 2.0, "{south:?}");
        // And east is to the right, which no amount of y flipping touches.
        let east = to_screen(&flat, 13.45, flat.latitude).expect("a screen point");
        assert!(east[0] > flat.width / 2.0, "{east:?}");
    }

    /// A bearing turns the screen about the center and nothing else: a point due north of the
    /// center lands where the bearing puts it, at the same distance.
    #[test]
    fn a_bearing_turns_the_screen_about_the_center() {
        let north = 52.55;
        let flat = view(0.0, 0.0);
        let straight = to_screen(&flat, flat.longitude, north).expect("a screen point");
        let radius = (straight[0] - flat.width / 2.0).hypot(straight[1] - flat.height / 2.0);

        let turned_view = view(0.0, 90.0);
        let turned = to_screen(&turned_view, flat.longitude, north).expect("a screen point");
        let other = (turned[0] - flat.width / 2.0).hypot(turned[1] - flat.height / 2.0);
        assert!((radius - other).abs() < 1e-6, "{radius} {other}");
        // A bearing of 90 puts north to the left of the screen.
        assert!(turned[0] < flat.width / 2.0, "{turned:?}");
        assert!((turned[1] - flat.height / 2.0).abs() < 1e-6, "{turned:?}");
    }
}
