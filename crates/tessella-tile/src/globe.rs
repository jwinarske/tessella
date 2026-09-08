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
    let span = 1.0 / f64::from(1u32 << z);
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
    let arc = 2.0 * core::f64::consts::PI / f64::from(1u32 << z);
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
    let arc = 2.0 * core::f64::consts::PI / f64::from(1u32 << z);
    radius * (1.0 - (arc / (2.0 * f64::from(segments))).cos())
}
