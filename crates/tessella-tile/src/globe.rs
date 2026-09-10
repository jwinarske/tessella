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

/// How far from the sphere's center a camera sits, in sphere radii, for a viewport `height` tall.
///
/// One radius is the surface, so this is always greater than one: at zoom zero the world is a small
/// ball a long way off, and the camera closes on the surface as the zoom rises. The height is not
/// optional -- `camera_to_center_distance` is proportional to it, and passing a stand-in put the
/// camera 1.018 radii out at z0, where the visible cap is ten degrees wide and the cull removed the
/// entire world. The test that counts what the cull removes is what found that.
///
/// # Why the latitude is here
///
/// `world_size(zoom)` is the *equator*. Mercator stretches everything else to keep its angles, by
/// `1 / cos(latitude)` locally, so one zoom is one scale only on the equator -- and a globe whose
/// radius came straight from `world_size / 2π` draws `cos(latitude)` of the scale the same zoom
/// gives a plane. Measured against the flat path: 0.6758, and `cos(47.4839°)` is 0.6758.
///
/// That is not a rounding difference, it is a third of the map, and it *moves with the latitude* --
/// so panning north rescales a map nobody zoomed and the tile level goes with it. Dividing the
/// radius by `cos(latitude)` is what makes one zoom mean one scale under both projections, and what
/// lets a map switch between them without the picture jumping 1.48x.
///
/// Clamped at the Mercator limit, where `cos` is 0.086 rather than zero: past it there is no
/// Mercator scale to match, and the pole itself would divide by nothing.
#[must_use]
pub fn camera_distance(zoom: f64, latitude: f64, height: f64) -> f64 {
    // The world spans `world_size(zoom)` pixels and the sphere's circumference is the same world,
    // so the radius in pixels is `world_size / 2π`. A camera `camera_to_center_distance` pixels
    // from the centre of the screen therefore sits that many radii out.
    let stretch = latitude
        .clamp(
            -crate::projection::LATITUDE_MAX,
            crate::projection::LATITUDE_MAX,
        )
        .to_radians()
        .cos();
    let radius = crate::camera::world_size(zoom) / (2.0 * core::f64::consts::PI) / stretch;
    if radius <= 0.0 || !radius.is_finite() {
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
    let distance = camera_distance(zoom, latitude, height);
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
    let distance = camera_distance(view.zoom, view.latitude, view.height);
    // The plane's own two angles, unnegated. Named here rather than written into the composition
    // so that a sign is one line to find -- both were wrong once, and each presents differently:
    // a wrong bearing turns the map the other way, a wrong pitch mirrors the foreshortening while
    // still tilting in the right direction, which reads as a rigid shift of the whole picture.
    let pitch = crate::camera::pitch_radians(view);
    let bearing = crate::camera::bearing_radians(view);
    // Turn (longitude, latitude) onto the +z axis, which is where the camera is.
    // Longitude first, then latitude -- and that means writing them the other way round, because
    // these post-multiply: `rotate_y(rotate_x(I, lat), lon)` is `Rx · Ry`, which applies `Ry` to
    // the point first. Composed the intuitive way the two turns happen in the wrong order and only
    // a camera on the equator or the prime meridian lands right.
    let turned = crate::camera::rotate_y(
        &crate::camera::rotate_x(&crate::camera::identity(), -view.latitude.to_radians()),
        -view.longitude.to_radians(),
    );
    // Then the camera, in the order GL JS's `VerticalPerspectiveTransform` composes it:
    //
    //     T(0, 0, -standoff) . Rx(pitch) . Rz(bearing) . T(0, 0, -1) . turned
    //
    // # Why the pull-back is split in two
    //
    // The obvious form backs the camera off by the whole `distance` and rotates about the
    // sphere's centre. That is a planet on a turntable, and it is not what a map does: pitching
    // would swing the point under the camera off the screen, and at street zoom -- where the
    // visible cap is a few hundred metres across and the camera is 0.0007 radii above it -- it
    // would swing it into the next country.
    //
    // A map pitches about the point under the camera. So the surface point comes to the origin
    // first, by the unit translate; the rotations happen there; and only then does the camera
    // stand off by what is left, which is `distance - 1` and is exactly
    // `camera_to_center_distance` in radii. That is also what makes a globe and a plane agree at
    // the zoom they are meant to be interchangeable at, since the plane pitches about its centre
    // too.
    //
    // Both angles keep the plane's sign, which is not obvious in advance: `flip` below negates y
    // after all of this, and a rotation composed here is seen through that negation. What settles
    // it is `a_pitched_globe_agrees_with_a_pitched_plane` rather than an argument -- there is no
    // globe oracle, `mbgl-render` having no globe, and the plane at street zoom is the nearest
    // thing to one.
    let mut to_surface = crate::camera::identity();
    crate::camera::translate_in_place(&mut to_surface, 0.0, 0.0, -1.0);
    let on_surface = crate::camera::multiply(&to_surface, &turned);
    // `rotate_*` post-multiplies, so this reads left to right as the matrix product and right to
    // left as what happens to a point: bearing first, then pitch.
    let aimed = crate::camera::rotate_z(
        &crate::camera::rotate_x(&crate::camera::identity(), pitch),
        bearing,
    );
    let mut back = crate::camera::identity();
    crate::camera::translate_in_place(&mut back, 0.0, 0.0, -(distance - 1.0));
    let eye = crate::camera::multiply(&back, &crate::camera::multiply(&aimed, &on_surface));
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
    let distance = camera_distance(view.zoom, view.latitude, view.height);
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

/// The bend expanded about a tile's center, so a shader never forms a number near one.
///
/// # Why this exists
///
/// The direct bend computes a unit-sphere position and lets [`clip_matrix`] amplify it. That
/// matrix grows with the zoom -- the sphere's radius is 1,663,008 screen pixels at z14 -- so one
/// `f32` ulp of the sphere position is a fifth of a pixel there, and the four transcendentals a
/// vertex each cost a few ulp of their own. Measured against the oracle, that is 1.98% of the frame
/// at Monterey z14 in bands a pixel or two wide along every edge, where z11 measures 0.13%.
///
/// Nothing about the bend needs world-scale numbers. It needs *where this tile is*, which is large,
/// and *where this vertex is inside it*, which is small; adding them before the trig is what
/// destroys the small one. So the large part is evaluated here, in `f64`, and what crosses the wire
/// is already in clip space:
///
/// ```text
/// clip(du, dv) = anchor + d_u du + d_v dv + (d_uu du^2 + d_vv dv^2) / 2 + d_uv du dv
/// ```
///
/// with `du`, `dv` measured in tile units from the tile's center. Every coefficient is small
/// except the anchor, and the anchor is a *difference of large numbers* that `f64` takes and `f32`
/// would lose -- which is exactly the cancellation this moves off the GPU.
///
/// # Where it holds
///
/// A quadratic is only as good as the arc the tile subtends. Checked against the exact chain over
/// a tile, worst screen error: 0.325 px at z6, 0.005 at z9, 0.0003 at z11, and 0.0001 from z13 up.
/// The direct bend is the better of the two below about z10 and the worse above it, and both are
/// far under a pixel between z9 and z11 -- so there is a wide overlap to switch in, and no zoom
/// where neither works. See plan.md §18 item 6.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AnchoredBend {
    /// Clip position of the tile's center.
    pub anchor: [f64; 4],
    /// First derivatives with respect to tile-local x and y.
    pub d_u: [f64; 4],
    /// See [`Self::d_u`].
    pub d_v: [f64; 4],
    /// Second derivatives. `d_uv` is the mixed term, which is not zero: longitude and latitude
    /// share `cos(latitude)` in the sphere point.
    pub d_uu: [f64; 4],
    /// See [`Self::d_uu`].
    pub d_vv: [f64; 4],
    /// See [`Self::d_uu`].
    pub d_uv: [f64; 4],
}

impl AnchoredBend {
    /// The expansion at a tile-local offset from the center, in tile units.
    #[must_use]
    pub fn at(&self, du: f64, dv: f64) -> [f64; 4] {
        core::array::from_fn(|i| {
            self.anchor[i]
                + self.d_u[i] * du
                + self.d_v[i] * dv
                + 0.5 * (self.d_uu[i] * du * du + self.d_vv[i] * dv * dv)
                + self.d_uv[i] * du * dv
        })
    }
}

/// Builds [`AnchoredBend`] for a tile under a view.
///
/// Analytic rather than by finite difference: the Mercator inverse is the Gudermannian, whose
/// derivative is `cos(latitude)` outright, so there is no step size to choose and no cancellation
/// to manage.
#[must_use]
pub fn anchored_bend(
    view: &crate::cover::ViewTransform,
    z: u8,
    x: u32,
    y: u32,
    wrap: i32,
) -> AnchoredBend {
    use core::f64::consts::PI;

    let clip = clip_matrix(view);
    let placement = crate::camera::mercator_matrix_for_tile(z, x, y, wrap);
    let extent = crate::camera::EXTENT;
    let (u0, v0) = (extent / 2.0, extent / 2.0);

    // Tile units to normalized Mercator, and the center of the tile in it.
    let (a, b) = (placement[0], placement[5]);
    let mx = a * u0 + placement[12];
    let my = b * v0 + placement[13];

    // Longitude is linear in `mx`; latitude is the Gudermannian of `my`.
    let longitude = 2.0 * PI * mx - PI;
    let latitude = crate::camera::latitude_of(my.clamp(0.0, 1.0)).to_radians();
    let (sin_lon, cos_lon) = longitude.sin_cos();
    let (sin_lat, cos_lat) = latitude.sin_cos();

    // d(longitude)/du and the two latitude derivatives, carried into tile units.
    let lon_u = 2.0 * PI * a;
    let lat_v = -2.0 * PI * cos_lat * b;
    let lat_vv = -4.0 * PI * PI * sin_lat * cos_lat * b * b;

    // The sphere point and its partials in (longitude, latitude), y negated as `sphere_point` has
    // it -- GL JS's convention, which `clip_matrix` carries the compensating flip for.
    let point = [cos_lat * sin_lon, -sin_lat, cos_lat * cos_lon];
    let d_lon = [cos_lat * cos_lon, 0.0, -cos_lat * sin_lon];
    let d_lat = [-sin_lat * sin_lon, -cos_lat, -sin_lat * cos_lon];
    let d_lon_lon = [-cos_lat * sin_lon, 0.0, -cos_lat * cos_lon];
    let d_lat_lat = [-cos_lat * sin_lon, sin_lat, -cos_lat * cos_lon];
    let d_lon_lat = [-sin_lat * cos_lon, 0.0, sin_lat * sin_lon];

    // Chain rule into tile units.
    let s_u: [f64; 3] = core::array::from_fn(|i| d_lon[i] * lon_u);
    let s_v: [f64; 3] = core::array::from_fn(|i| d_lat[i] * lat_v);
    let s_uu: [f64; 3] = core::array::from_fn(|i| d_lon_lon[i] * lon_u * lon_u);
    let s_vv: [f64; 3] = core::array::from_fn(|i| d_lat_lat[i] * lat_v * lat_v + d_lat[i] * lat_vv);
    let s_uv: [f64; 3] = core::array::from_fn(|i| d_lon_lat[i] * lon_u * lat_v);

    // Into clip space. The position carries a `w` of one and every derivative a `w` of zero,
    // because differentiating a constant is what that is.
    let apply = |v: [f64; 3], w: f64| -> [f64; 4] {
        let p = [v[0], v[1], v[2], w];
        core::array::from_fn(|r| (0..4).map(|c| clip[c * 4 + r] * p[c]).sum())
    };
    AnchoredBend {
        anchor: apply(point, 1.0),
        d_u: apply(s_u, 0.0),
        d_v: apply(s_v, 0.0),
        d_uu: apply(s_uu, 0.0),
        d_vv: apply(s_vv, 0.0),
        d_uv: apply(s_uv, 0.0),
    }
}

/// The anchored bend's linear part, as a matrix a caller can use where a plane's would go.
///
/// [`anchored_bend`] is a quadratic and most of the symbol path takes a `Mat4` -- the label plane,
/// the perspective ratio, the screen projection placement competes in. Dropping the second-order
/// term gives an affine map that is exactly right at the tile's center and degrades outward, which
/// is what those callers can use unchanged.
///
/// Accurate enough for *placement*, which is a question about where a label's box lands against its
/// neighbours', and not for drawing: a glyph drawn through this would sit where the quadratic term
/// says it should not. The material evaluates the full expansion.
///
/// The columns are the derivatives and the translation is the anchor with the tile's center taken
/// back out, so `M * (u, v, 0, 1)` is `anchor + d_u (u - cu) + d_v (v - cv)`.
#[must_use]
pub fn anchored_matrix(
    view: &crate::cover::ViewTransform,
    z: u8,
    x: u32,
    y: u32,
    wrap: i32,
) -> crate::camera::Mat4 {
    let bend = anchored_bend(view, z, x, y, wrap);
    let half = crate::camera::EXTENT / 2.0;
    let mut out = [0.0; 16];
    for row in 0..4 {
        out[row] = bend.d_u[row];
        out[4 + row] = bend.d_v[row];
        out[8 + row] = 0.0;
        out[12 + row] = bend.anchor[row] - bend.d_u[row] * half - bend.d_v[row] * half;
    }

    let scale = clip_w_scale(view);
    for value in &mut out {
        *value *= scale;
    }
    out
}

/// What to multiply a globe's clip coordinates by so `w` means what a plane's `w` means.
///
/// A projective coordinate is scale-invariant in `x / w`, so this moves nothing on screen. It
/// matters because `w` is not only a divisor: the symbol path reads it as the distance from the
/// camera to the anchor and divides `camera_to_center_distance` by it to decide how much
/// perspective shrank a label -- both to size the type and to size the box it competes with.
///
/// [`clip_matrix`] measures that distance in sphere radii, so `w` arrives at 0.0011 where a
/// plane's is 1152. The ratio is then a million, `perspective_ratio` pins to its clamp of four,
/// and two things follow: every collision box is four times its size, so every label in the frame
/// collides with every other -- 12 glyph quads drawn against a plane's 1384 at the same camera --
/// and the type that does survive is drawn four times too large.
///
/// Taken at the point under the camera, so it is one number for the frame rather than one per
/// tile: a per-tile normalisation would leave neighbouring tiles disagreeing about how far away
/// they are, which is the thing `w` exists to say.
#[must_use]
pub fn clip_w_scale(view: &crate::cover::ViewTransform) -> f64 {
    let reference = crate::camera::camera_to_center_distance(view.height);
    let under = sphere_point(view.longitude, view.latitude);
    let clip = clip_matrix(view);
    let point = [under[0], under[1], under[2], 1.0];
    let centre_w: f64 = (0..4).map(|c| clip[c * 4 + 3] * point[c]).sum();
    if centre_w.abs() <= f64::EPSILON {
        return 1.0;
    }
    reference / centre_w
}
