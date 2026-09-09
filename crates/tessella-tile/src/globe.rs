//! Where a point on the map lands on a sphere — plan.md §13.4's consumer half, first piece.
//!
//! # Why this is Rust and not a shader
//!
//! The globe is drawn by bending Mercator geometry per vertex, which is the consumer's material's
//! job. But three other things need the same arithmetic and none of them is a shader: the horizon
//! cull that decides whether a tile is worth subdividing at all, the symbol placement that has to
//! put a label on a curved surface, and the tests. Written once here, they cannot drift from each
//! other the way a shader and a CPU copy of it do.
//!
//! # No oracle, and what stands in for one
//!
//! MapLibre Native has no globe, so `mbgl-render` cannot answer any question on this page --
//! §13.4 says so and it is why this side went last. The reference is MapLibre GL JS, whose
//! `latLngToECEF` this transcribes, and the checks are geometric identities rather than a
//! comparison: the poles are the poles, a round trip through Mercator returns the latitude it
//! started from, the antimeridian's two spellings are one point, and a point exactly on the
//! horizon is exactly on the horizon. Identities are weaker than an oracle and they are what
//! there is.

/// How many tiles a zoom level has to a side, with the shift kept inside its width.
///
/// `1u32 << z` panics in a debug build for `z >= 32` and shifts by `z % 32` in a release one, which
/// is worse: at `z = 32` it answers one tile to a side and a caller reads a tile covering the whole
/// world. `cover::MAX_ZOOM` is the ceiling every cover already clamps to, and DR-9's posture is
/// that a camera is not necessarily trustworthy -- so this clamps rather than trusting.
fn tiles_across(z: u8) -> f64 {
    f64::from(1u32 << z.min(crate::cover::MAX_ZOOM))
}

/// A point on the unit sphere, in MapLibre GL JS's ECEF convention.
///
/// `latLngToECEF`: `x = cos(lat) sin(lng)`, `y = -sin(lat)`, `z = cos(lat) cos(lng)`. So `+z`
/// points at (0°, 0°), `+x` east of it, and `+y` *down* — the sign on `y` is the screen's, not the
/// Earth's, and it is kept because every formula downstream of it in GL JS assumes it.
#[must_use]
pub fn sphere_point(longitude: f64, latitude: f64) -> [f64; 3] {
    let lat = latitude.to_radians();
    let lon = longitude.to_radians();
    [lat.cos() * lon.sin(), -lat.sin(), lat.cos() * lon.cos()]
}

/// The same, from a normalised Mercator position: `x` and `y` each in `0..1` across the world.
///
/// This is the form a tile's geometry arrives in -- a tile at `z/x/y` covers a known square of it
/// -- so it is the one the vertex bend will use. `y` outside `0..1` is past the Mercator limit and
/// clamps to the pole rather than diverging, which is what a tile whose edge runs off the top of
/// the world needs.
#[must_use]
pub fn sphere_point_from_mercator(x: f64, y: f64) -> [f64; 3] {
    let longitude = x * 360.0 - 180.0;
    let latitude = crate::camera::latitude_of(y.clamp(0.0, 1.0));
    sphere_point(longitude, latitude)
}

/// How far from the sphere's centre a camera sits, in sphere radii, for a viewport `height` tall.
///
/// One radius is the surface, so this is always greater than one: at zoom zero the world is a small
/// ball a long way off, and the camera closes on the surface as the zoom rises. The height is not
/// optional -- `camera_to_center_distance` is proportional to it, and passing a stand-in put the
/// camera 1.018 radii out at z0, where the visible cap is ten degrees wide and the cull removed the
/// entire world. The test that counts what the cull removes is what found that.
#[must_use]
pub fn camera_distance(zoom: f64, height: f64) -> f64 {
    // The world spans `world_size(zoom)` pixels and the sphere's circumference is the same world,
    // so the radius in pixels is `world_size / 2π`. A camera `camera_to_center_distance` pixels
    // from the centre of the screen therefore sits that many radii out.
    let radius = crate::camera::world_size(zoom) / (2.0 * core::f64::consts::PI);
    if radius <= 0.0 {
        return f64::INFINITY;
    }
    1.0 + crate::camera::camera_to_center_distance(height) / radius
}

/// Whether a surface point faces a camera on the `+direction` axis, at `distance` radii out.
///
/// The horizon is where the line of sight grazes the sphere, and for a unit sphere that is exactly
/// `dot(point, direction) == 1 / distance` -- the cosine of the angle subtended by the tangent.
/// Nearer the camera than that and the point is on the visible cap; beyond it the sphere itself is
/// in the way.
///
/// §13.4 measured what this saves and it is not much: tiles behind the sphere are a third to a half
/// of the cover between z1 and z2.5 and *none* outside it. It is here because the consumer skips a
/// subdivision as well as a draw, and because a tile drawn on the far side of the planet is wrong
/// rather than merely wasteful.
#[must_use]
pub fn faces_camera(point: [f64; 3], direction: [f64; 3], distance: f64) -> bool {
    if distance <= 1.0 {
        // The camera is on or inside the surface: everything in front of it faces it. A `NaN`
        // distance takes the tangent branch below and answers "not visible" there, which is the
        // safe end to fail at -- a tile that is wrongly skipped is a hole, and one wrongly drawn
        // is on the far side of the planet.
        return dot(point, direction) > 0.0;
    }
    dot(point, direction) > 1.0 / distance
}

/// The dot product of two vectors.
#[must_use]
pub fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// Whether any part of a tile faces a camera over `(longitude, latitude)` at `zoom`.
///
/// The cull §13.4 leaves to the consumer: one test per tile before it subdivides, which removes the
/// draw as well as the work. Sampled at the tile's four corners and its centre rather than solved,
/// because a tile is a curved patch and the extremum is at a corner for every tile a Mercator cover
/// produces -- and because being wrong in the safe direction matters more than being tight. A tile
/// wrongly kept costs a subdivision; a tile wrongly culled is a hole in the planet.
///
/// Conservative for the same reason: it answers yes if *any* sample faces the camera.
#[must_use]
pub fn tile_faces_camera(
    z: u8,
    x: u32,
    y: u32,
    longitude: f64,
    latitude: f64,
    zoom: f64,
    height: f64,
) -> bool {
    let toward = sphere_point(longitude, latitude);
    let distance = camera_distance(zoom, height);
    let span = 1.0 / tiles_across(z);
    #[allow(clippy::cast_lossless)]
    let (x0, y0) = (f64::from(x) * span, f64::from(y) * span);
    let samples = [
        (x0, y0),
        (x0 + span, y0),
        (x0, y0 + span),
        (x0 + span, y0 + span),
        (x0 + span / 2.0, y0 + span / 2.0),
    ];
    samples
        .iter()
        .any(|&(sx, sy)| faces_camera(sphere_point_from_mercator(sx, sy), toward, distance))
}

/// How many segments a tile edge needs so the flat chord stays within `tolerance` pixels of the
/// sphere it is approximating.
///
/// # Why an error bound and not a table
///
/// GL JS carries a granularity expression -- a base value halved per zoom, floored at a minimum --
/// and the numbers in it are chosen rather than derived. A bound can be checked: split an arc of
/// angle `θ` into `n` pieces and the chord of each sits `R(1 - cos(θ/2n))` inside the arc, so the
/// segment count that keeps that under a pixel is arithmetic with an answer. It also adapts on its
/// own to the thing a table has to be re-tuned for -- a taller viewport, a different field of view
/// -- because the radius it divides is measured in the same pixels the tolerance is.
///
/// `z` is the tile's own zoom, which sets how much of the world it spans; `zoom` is the camera's,
/// which sets how large the sphere is in pixels. One segment is the floor: a tile always has its
/// own corners.
///
/// The viewport's height is deliberately not a parameter. It scales `camera_to_center_distance`
/// and so the camera's distance, but the sphere's radius in pixels is `world_size(zoom) / 2π` and
/// has nothing to do with it -- a taller screen sees more of the same globe rather than a bigger
/// one. It was a parameter here first, unused, which is the kind of thing a signature says and the
/// body does not.
#[must_use]
pub fn edge_segments(z: u8, zoom: f64, tolerance: f64) -> u32 {
    let radius = crate::camera::world_size(zoom) / (2.0 * core::f64::consts::PI);
    if !tolerance.is_finite() || tolerance <= 0.0 || radius <= 0.0 {
        return 1;
    }
    // The arc one tile edge subtends at the sphere's centre.
    let arc = 2.0 * core::f64::consts::PI / tiles_across(z);
    // `1 - cos(x) ≈ x²/2` for the small angles this always lands in, so `n ≈ θ/2 · √(R/2t)`.
    // Solved rather than iterated, then checked below, because the approximation is only good
    // while the segments are small and the check costs nothing.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let mut n = ((arc / 2.0) * (radius / (2.0 * tolerance)).sqrt())
        .ceil()
        .max(1.0) as u32;
    // The exact test, in case the approximation undershot at a coarse zoom.
    while n < MAX_EDGE_SEGMENTS && radius * (1.0 - (arc / (2.0 * f64::from(n))).cos()) > tolerance {
        n += 1;
    }
    n.min(MAX_EDGE_SEGMENTS)
}

/// The most segments an edge is ever given.
///
/// A ceiling rather than a computed bound: the arithmetic above is monotone in the sphere's radius
/// and nothing stops it asking for thousands at a low zoom on a tall screen. Squared, that is the
/// vertex count of one tile, so the ceiling is what keeps a globe from subdividing itself out of a
/// frame budget. `128` is the same order as GL JS's base granularity.
pub const MAX_EDGE_SEGMENTS: u32 = 128;

/// How far the flat chord of one segment sits from the sphere, in pixels.
///
/// The quantity [`edge_segments`] bounds, exposed so a test can check the bound rather than trust
/// it, and so a caller choosing its own tolerance can see what it bought.
#[must_use]
pub fn chord_error(z: u8, zoom: f64, segments: u32) -> f64 {
    if segments == 0 {
        return f64::INFINITY;
    }
    let radius = crate::camera::world_size(zoom) / (2.0 * core::f64::consts::PI);
    let arc = 2.0 * core::f64::consts::PI / tiles_across(z);
    radius * (1.0 - (arc / (2.0 * f64::from(segments))).cos())
}

/// The matrix that takes a point on the unit sphere to clip space.
///
/// Built the way GL JS builds its globe matrix: turn the world so the point under the camera faces
/// it, back the camera off along that axis, then project. Composed rather than a look-at because
/// the two rotations are the camera's own longitude and latitude and reading them back out of a
/// look-at is harder than writing them down.
///
/// The `y` axis points *down* in [`sphere_point`]'s convention, so the latitude rotation is the
/// negative of what a right-handed Earth would take. The tests pin the composition rather than the
/// derivation: the point under the camera lands at the centre of the screen, and a point a quarter
/// turn away lands off it in the direction it should.
#[must_use]
pub fn clip_matrix(view: &crate::cover::ViewTransform) -> crate::camera::Mat4 {
    let distance = camera_distance(view.zoom, view.height);
    // Turn (longitude, latitude) onto the +z axis, which is where the camera is.
    // Longitude first, then latitude -- and that means writing them the other way round, because
    // these post-multiply: `rotate_y(rotate_x(I, lat), lon)` is `Rx · Ry`, which applies `Ry` to
    // the point first. Composed the intuitive way the two turns happen in the wrong order and only
    // a camera on the equator or the prime meridian lands right.
    let turned = crate::camera::rotate_y(
        &crate::camera::rotate_x(&crate::camera::identity(), -view.latitude.to_radians()),
        -view.longitude.to_radians(),
    );
    // Then back the camera off along z. The sphere is a unit ball, so the distance is in radii.
    //
    // `T * R`, not `R * T`: the camera pulls back along the axis the rotation has already put the
    // target on, and `translate_in_place` post-multiplies, which would translate in the turned
    // frame instead. Composed the wrong way round the target lands behind the camera and every
    // projection returns nothing, which is what the centring test said first.
    let mut back = crate::camera::identity();
    crate::camera::translate_in_place(&mut back, 0.0, 0.0, -distance);
    let eye = crate::camera::multiply(&back, &turned);
    // `sphere_point`'s `y` points *down* -- GL JS's convention, kept because everything downstream
    // of it there assumes the sign -- and clip space has `y` up. One of the two has to give, and it
    // gives here rather than in the projection so that `sphere_point` stays the thing GL JS
    // documents. The flip reverses triangle winding, which is the consumer's to know about when it
    // culls faces: a globe patch wound like a Mercator one comes out back-facing.
    // On the *output* side: `scale` post-multiplies, and applied there it would flip the point
    // before the rotation rather than the picture after it, which moves the centre off screen.
    let flip = crate::camera::scale(&crate::camera::identity(), 1.0, -1.0, 1.0);
    let eye = crate::camera::multiply(&flip, &eye);
    // Near and far bracket the *visible cap*, not the ball.
    //
    // The cap runs from the nearest surface point, `distance - 1`, to the horizon, where the line
    // of sight grazes the sphere at `sqrt(distance² - 1)`. Nothing beyond the horizon is ever
    // drawn -- the sphere is in the way, which is what `faces_camera` says -- so bracketing the
    // whole ball spends the depth range on a hemisphere that cannot appear.
    //
    // That waste is harmless at low zoom and fatal above z10. The camera closes on the surface as
    // the zoom rises: at z14 it sits 0.000674 radii out while `(distance + 1) * 1.5` still asks
    // for a far plane at three. Worse, a near floored at `0.01` overtakes the surface entirely --
    // the old form clamped there and the clip-space z of the point under the camera went from
    // +0.002 at z10 to -0.85 at z11 and -13.9 at z14, which is behind the near plane and clipped.
    // The whole planet went black from z11 up, and every test here passed because they all run at
    // a zoom where the clamp does not bind.
    //
    // The margins keep a surface fragment off both planes. The floor is against a camera on the
    // surface rather than against a zoom, since `camera_distance` can answer exactly one.
    let (near, far) = depth_range(view);
    #[allow(clippy::cast_possible_truncation)]
    let fov = f64::from(crate::camera::DEFAULT_FOV as f32);
    // A viewport with no width divides by zero inside `perspective` and hands back a matrix of
    // NaNs, which propagates into every vertex rather than failing anywhere findable. A square
    // frustum is the wrong picture for a degenerate viewport and it is a picture.
    let aspect = if view.width > 0.0 && view.height > 0.0 && (view.width / view.height).is_finite()
    {
        view.width / view.height
    } else {
        1.0
    };
    let projection = crate::camera::perspective(fov, aspect, near, far);
    crate::camera::multiply(&projection, &eye)
}

/// The near and far planes [`clip_matrix`] brackets the visible cap with.
///
/// Exposed because the depth bias has to be sized against it. mbgl separates coincident layers by
/// a fixed nudge in clip space, which works while the frustum is a fixed depth -- and a globe's is
/// not. The camera closes on the surface as the zoom rises, so the span here falls from 1.7 at z4
/// to 0.055 at z14, by which point a bias of 0.031 is more than half of it and the layer it
/// belongs to is pushed through the near plane. At z16 it is 112% of the span. A producer scales
/// the nudge by this rather than sending it absolute.
#[must_use]
pub fn depth_range(view: &crate::cover::ViewTransform) -> (f64, f64) {
    let distance = camera_distance(view.zoom, view.height);
    // The cap runs from the nearest surface point to the horizon; nothing past the horizon is
    // ever drawn, so bracketing the whole ball spends the range on a hemisphere that cannot
    // appear -- and at high zoom spends nearly all of it.
    let horizon = (distance * distance - 1.0).max(0.0).sqrt();
    let near = ((distance - 1.0) * 0.5).max(f64::EPSILON);
    (near, (horizon * 1.5).max(near * 2.0))
}

/// A point through a matrix, divided through by `w`, or `None` when it is behind the camera.
///
/// The perspective divide, exposed because every check of [`clip_matrix`] is "where did this land"
/// and doing it by hand in each is how sign errors survive.
#[must_use]
pub fn project_point(matrix: &crate::camera::Mat4, point: [f64; 3]) -> Option<[f64; 3]> {
    let mut out = [0.0f64; 4];
    for row in 0..4 {
        out[row] = matrix[row] * point[0]
            + matrix[4 + row] * point[1]
            + matrix[8 + row] * point[2]
            + matrix[12 + row];
    }
    if out[3] <= 0.0 {
        return None;
    }
    Some([out[0] / out[3], out[1] / out[3], out[2] / out[3]])
}
