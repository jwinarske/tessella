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

/// How far from the sphere's centre a camera sits to see it at a given zoom, in sphere radii.
///
/// One radius is the surface, so this is always greater than one. The globe is the whole world in
/// view at zoom zero and approaches the surface as the zoom rises, which is the same relationship
/// `world_size` describes for the plane -- written against it so the two agree at the zoom where a
/// map switches between them.
#[must_use]
pub fn camera_distance(zoom: f64) -> f64 {
    // The world spans `world_size(zoom)` pixels and the sphere's circumference is the same world,
    // so the radius in pixels is `world_size / 2π`. A camera at `camera_to_center_distance` pixels
    // therefore sits that many radii out.
    let radius = crate::camera::world_size(zoom) / (2.0 * core::f64::consts::PI);
    if radius <= 0.0 {
        return f64::INFINITY;
    }
    1.0 + crate::camera::camera_to_center_distance(1.0) / radius
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
