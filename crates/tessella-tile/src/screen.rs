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
//!
//! # The sphere's pair, which has no oracle and no near-point answer
//!
//! A map drawn on a globe is the same camera over a different surface, so the pair above is not
//! the pair for it: the plane repeats and a sphere hides half of itself, and the inverse is a ray
//! met with a ball rather than with a plane. [`to_screen_on_sphere`] and [`from_screen_on_sphere`]
//! are that pair, over [`crate::globe`]'s unit sphere and through the same pixel matrix, so the
//! two projections answer in one convention.
//!
//! Two things differ, and both are the surface's rather than a choice. There is no oracle --
//! `mbgl-render` has no globe, so GL JS's `VerticalPerspectiveTransform` is the reference and the
//! checks are round trips and identities, as in [`crate::globe`]. And a pixel beside the globe's
//! disc has no coordinate *at all*, where the plane always has one: a ray that misses a ball is
//! not a near point, so these two return [`NoAnswer`] rather than mbgl's degenerate answer.

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
/// too, and a caller that cares asks [`to_screen_detail`].
#[must_use]
pub fn to_screen(view: &ViewTransform, longitude: f64, latitude: f64) -> Option<[f64; 2]> {
    Some(to_screen_detail(view, longitude, latitude)?.point)
}

/// What [`to_screen`] answers, with the thing it folds away.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Projected {
    /// The pixel, in [`to_screen`]'s convention: from the bottom left.
    pub point: [f64; 2],
    /// Whether the coordinate is in front of the camera.
    ///
    /// False behind it, where `point` is a reflection rather than a position -- the projection
    /// divides by a negative `w`, so a coordinate beyond a pitched camera's horizon lands on the
    /// screen mirrored through its center. mbgl answers those too, which is why [`to_screen`]
    /// keeps them; something being *placed* on the screen wants to know rather than be put in the
    /// wrong half of it.
    pub in_front: bool,
}

/// [`to_screen`], and whether the coordinate was in front of the camera.
///
/// `None` when the view has no area, which is the only thing that leaves a coordinate with no
/// pixel at all.
#[must_use]
pub fn to_screen_detail(view: &ViewTransform, longitude: f64, latitude: f64) -> Option<Projected> {
    let matrix = coord_matrix(view).ok()?;
    let world = projection::project(longitude, latitude, camera::world_size(view.zoom));
    let point = [world[0] / TILE_SIZE, world[1] / TILE_SIZE, 0.0, 1.0];
    let out = transform(&matrix, point);
    Some(Projected {
        point: [out[0] / out[3], view.height - out[1] / out[3]],
        in_front: out[3] > 0.0,
    })
}

/// Which coordinate a screen pixel is over, as longitude then latitude.
///
/// The ray through the pixel, met with the map plane. See the module note for what happens when
/// it does not meet it, and [`from_screen_detail`] to be told that it did not.
///
/// `None` when the view has no area or its projection will not invert.
#[must_use]
pub fn from_screen(view: &ViewTransform, point: [f64; 2]) -> Option<[f64; 2]> {
    Some(from_screen_detail(view, point)?.coordinate)
}

/// What [`from_screen`] answers, with the thing it folds away.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Unprojected {
    /// The coordinate, longitude then latitude.
    pub coordinate: [f64; 2],
    /// Whether the ray through the pixel descended to the plane at all.
    ///
    /// False at or above the horizon under pitch, where `coordinate` is mbgl's answer for that
    /// case -- the near point -- and so is a coordinate on the plane rather than the one the pixel
    /// is over. There is no clamped answer that would be better: the pixel is looking at the sky.
    pub met_the_plane: bool,
}

/// [`from_screen`], and whether the ray reached the plane.
///
/// `None` when the view has no area or its projection will not invert.
#[must_use]
pub fn from_screen_detail(view: &ViewTransform, point: [f64; 2]) -> Option<Unprojected> {
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
    Some(Unprojected {
        coordinate: [longitude, latitude],
        met_the_plane: z0 > z1,
    })
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

/// Why a conversion between the screen and a sphere has no answer.
///
/// The plane's pair answers for every pixel and every coordinate, because a plane is unbounded
/// and mbgl has a degenerate answer for the rest. A sphere has neither property.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoAnswer {
    /// The view has no area, or its matrix will not invert.
    ///
    /// A property of the camera and not of the point: every point this frame has the same answer.
    NoView,
    /// The point is not on the part of the surface this camera can see.
    ///
    /// A pixel whose ray passes beside the globe, or a coordinate on its far side. Half the world
    /// is behind the planet at any moment and it still has a pixel the projection would hand back
    /// -- one in front of the ocean it is under.
    NotOnTheSurface,
}

/// Where a coordinate lands on the screen with the map drawn on a sphere, in [`to_screen`]'s
/// convention: pixels from the bottom left.
///
/// [`NoAnswer::NotOnTheSurface`] for a coordinate on the globe's far side, by the same horizon
/// test the cover culls tiles with -- [`crate::globe::faces_camera`].
pub fn to_screen_on_sphere(
    view: &ViewTransform,
    longitude: f64,
    latitude: f64,
) -> Result<[f64; 2], NoAnswer> {
    let point = crate::globe::sphere_point(longitude, latitude);
    let toward = crate::globe::sphere_point(view.longitude, view.latitude);
    let distance = crate::globe::camera_distance(view.zoom, view.latitude, view.height);
    if !crate::globe::faces_camera(point, toward, distance) {
        return Err(NoAnswer::NotOnTheSurface);
    }
    let clip = crate::globe::project_point(&crate::globe::clip_matrix(view), point)
        .ok_or(NoAnswer::NotOnTheSurface)?;
    // `project_point` has already divided through, so this enters the pixel matrix as a position
    // with `w` of one -- which is what it is, the matrix being affine in x and y.
    let out = transform(&pixel_matrix(view), [clip[0], clip[1], clip[2], 1.0]);
    if out[3] == 0.0 || !out[0].is_finite() || !out[1].is_finite() {
        return Err(NoAnswer::NoView);
    }
    Ok([out[0] / out[3], view.height - out[1] / out[3]])
}

/// Which coordinate a screen pixel is over with the map drawn on a sphere, as longitude then
/// latitude.
///
/// The ray through the pixel, met with the ball. Where the plane's inverse solves for the one `z`
/// that is the surface, this solves a quadratic and takes the nearer root, which is the front of
/// the globe; a ray that misses it has no coordinate and says so.
///
/// # Why the two points on the ray are the clip-space near and far planes
///
/// [`from_screen`] takes mbgl's `0` and `1`, which are two points on the pixel's ray and not the
/// frustum's ends -- a perspective's near plane is at `z = -1`. That costs the plane nothing,
/// because it solves for a parameter rather than measuring along the ray. Here the parameter's
/// sign is the thing that says whether the globe is in front of the camera, so the ray is taken
/// between the planes it actually spans.
pub fn from_screen_on_sphere(view: &ViewTransform, point: [f64; 2]) -> Result<[f64; 2], NoAnswer> {
    let matrix = camera::multiply(&pixel_matrix(view), &crate::globe::clip_matrix(view));
    let inverted = camera::invert(&matrix).ok_or(NoAnswer::NoView)?;

    // The same flip the plane's inverse undoes, for the same reason.
    let flipped = view.height - point[1];
    let near = transform(&inverted, [point[0], flipped, -1.0, 1.0]);
    let far = transform(&inverted, [point[0], flipped, 1.0, 1.0]);
    if near[3] == 0.0 || far[3] == 0.0 {
        return Err(NoAnswer::NoView);
    }
    let from = [near[0] / near[3], near[1] / near[3], near[2] / near[3]];
    let to = [far[0] / far[3], far[1] / far[3], far[2] / far[3]];
    let along = [to[0] - from[0], to[1] - from[1], to[2] - from[2]];

    // |from + t·along| = 1, the unit sphere being the surface every globe formula here is on.
    let a = crate::globe::dot(along, along);
    let b = 2.0 * crate::globe::dot(from, along);
    let c = crate::globe::dot(from, from) - 1.0;
    let discriminant = b * b - 4.0 * a * c;
    if a == 0.0 || discriminant < 0.0 {
        return Err(NoAnswer::NotOnTheSurface);
    }
    let root = discriminant.sqrt();
    // The nearer root is the front of the globe. Both negative means the ball is behind the near
    // plane, which a camera outside the sphere cannot produce and a degenerate one can.
    let t = [(-b - root) / (2.0 * a), (-b + root) / (2.0 * a)]
        .into_iter()
        .find(|t| *t >= 0.0)
        .ok_or(NoAnswer::NotOnTheSurface)?;
    let hit = [
        from[0] + along[0] * t,
        from[1] + along[1] * t,
        from[2] + along[2] * t,
    ];
    if !hit[0].is_finite() || !hit[1].is_finite() || !hit[2].is_finite() {
        return Err(NoAnswer::NoView);
    }
    // `sphere_point` inverted: `y` is negative sine of the latitude and the longitude is the angle
    // in the x-z plane measured from +z, which is (0, 0).
    let latitude = (-hit[1].clamp(-1.0, 1.0)).asin().to_degrees();
    let longitude = hit[0].atan2(hit[2]).to_degrees();
    Ok([longitude, latitude])
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
            ground_below: 0.0,
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

    /// A pixel the camera is looking at the sky through says so rather than answering with a
    /// coordinate that is merely on the plane.
    ///
    /// Walking *up* the screen at high pitch, which is away from the camera: somewhere between the
    /// middle and the top edge the ray stops descending, and every pixel above that is sky.
    #[test]
    fn a_ray_that_never_reaches_the_plane_says_so() {
        let pitched = view(75.0, 0.0);
        let met: Vec<bool> = [100.0, 300.0, 500.0, 700.0, 760.0]
            .iter()
            .map(|y| {
                from_screen_detail(&pitched, [512.0, *y])
                    .expect("a coordinate")
                    .met_the_plane
            })
            .collect();
        assert_eq!(met, vec![true, true, true, false, false], "{met:?}");
        // And a flat camera has no sky on it at all.
        let flat = view(0.0, 0.0);
        for y in [1.0, 384.0, 767.0] {
            assert!(
                from_screen_detail(&flat, [512.0, y])
                    .expect("a coordinate")
                    .met_the_plane,
                "{y}"
            );
        }
    }

    /// A coordinate behind a pitched camera projects to a pixel that is a reflection, and
    /// [`to_screen_detail`] is how a caller finds out.
    ///
    /// Behind means *south* here: at a bearing of zero the camera sits south of the center looking
    /// north, so it is the southern half of the plane that passes under it. A point 280 km south
    /// of the center lands at y 697 of a 768-pixel viewport -- on the screen, in the half where
    /// distant northern ground is drawn -- so its position cannot be what tells a caller to drop
    /// it. The same pixel read the other way says it is sky, by the test above: either alone could
    /// be an arithmetic slip, the two agreeing is the geometry.
    #[test]
    fn a_coordinate_behind_the_camera_is_a_reflection() {
        let pitched = view(75.0, 0.0);
        let behind = to_screen_detail(&pitched, pitched.longitude, 50.0).expect("a screen point");
        assert!(!behind.in_front, "{behind:?}");
        assert!(
            behind.point[1] > 0.0 && behind.point[1] < pitched.height,
            "{behind:?}"
        );
        assert!(
            !from_screen_detail(&pitched, behind.point)
                .expect("a coordinate")
                .met_the_plane,
            "{behind:?}"
        );
        // And a coordinate the camera is looking at is in front of it.
        let ahead = to_screen_detail(&pitched, pitched.longitude, pitched.latitude)
            .expect("a screen point");
        assert!(ahead.in_front, "{ahead:?}");
    }

    fn globe_view(zoom: f64, pitch: f64, bearing: f64) -> ViewTransform {
        ViewTransform {
            zoom,
            pitch,
            bearing,
            ..view(0.0, 0.0)
        }
    }

    /// The center of the screen is the coordinate the camera is over, on a sphere as on a plane.
    #[test]
    fn the_center_of_the_screen_is_under_the_camera_on_a_sphere() {
        for zoom in [0.0, 2.0, 6.0, 14.0] {
            let view = globe_view(zoom, 0.0, 0.0);
            let under = from_screen_on_sphere(&view, [view.width / 2.0, view.height / 2.0])
                .expect("a coordinate");
            assert!(
                (under[0] - view.longitude).abs() < 1e-6 && (under[1] - view.latitude).abs() < 1e-6,
                "{zoom} {under:?}"
            );
        }
    }

    /// And the sphere's two directions invert each other, which is the only check available: there
    /// is no globe oracle to compare a number with.
    #[test]
    fn the_sphere_pair_inverts_itself() {
        for (zoom, pitch, bearing) in [
            (0.0, 0.0, 0.0),
            (2.0, 0.0, 38.0),
            (6.0, 45.0, 0.0),
            (14.0, 60.0, 200.0),
        ] {
            let view = globe_view(zoom, pitch, bearing);
            for point in [[512.0, 384.0], [600.0, 420.0], [430.0, 330.0]] {
                let here = from_screen_on_sphere(&view, point).expect("a coordinate");
                let back = to_screen_on_sphere(&view, here[0], here[1]).expect("a screen point");
                assert!(
                    (back[0] - point[0]).abs() < 1e-4 && (back[1] - point[1]).abs() < 1e-4,
                    "{zoom} {pitch} {bearing} {point:?} -> {here:?} -> {back:?}"
                );
            }
        }
    }

    /// A pixel beside the globe is over nothing. The plane has no such pixel, which is the one
    /// way the two pairs differ in kind rather than in arithmetic.
    #[test]
    fn a_pixel_beside_the_globe_is_over_nothing() {
        let view = globe_view(0.0, 0.0, 0.0);
        // The ball at zoom zero does not fill a 1024x768 viewport, so a corner misses it.
        assert_eq!(
            from_screen_on_sphere(&view, [2.0, 2.0]),
            Err(NoAnswer::NotOnTheSurface)
        );
        // And the plane answers for that very pixel.
        assert!(from_screen(&view, [2.0, 2.0]).is_some());
    }

    /// The far side of the globe has a pixel the projection would hand back and no position on
    /// the screen, which is what the horizon test is for.
    #[test]
    fn the_far_side_of_the_globe_has_no_pixel() {
        let view = globe_view(2.0, 0.0, 0.0);
        // The antipode of Berlin.
        assert_eq!(
            to_screen_on_sphere(&view, view.longitude - 180.0, -view.latitude),
            Err(NoAnswer::NotOnTheSurface)
        );
        assert!(to_screen_on_sphere(&view, view.longitude, view.latitude).is_ok());
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
