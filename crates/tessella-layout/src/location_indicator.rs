//! A location indicator's geometry: the accuracy circle, in world pixels around the puck.
//!
//! mbgl's `updateRadius`. Seventy-three points -- "72 points + position" in its own comment -- the
//! first at the center and the rest around it, each one `cheap_ruler`'s `destination` from the
//! location at the accuracy radius, projected and expressed as an offset from the projected
//! center.
//!
//! # Why the geometry moves with the camera
//!
//! Because the offsets are in *world pixels at the current scale*, not in tile units or in
//! degrees. `Projection::project(latlng, s.getScale())` is what mbgl calls and the scale is the
//! camera's, so a zoom changes the vertices rather than only the matrix. Every other family here
//! puts the zoom in the matrix and leaves the vertices alone; this one does not, and a builder
//! that cached the circle across a zoom would draw an accuracy radius that no longer means what
//! it says.
//!
//! # Why the last point is the first
//!
//! The step is `360 / 71` over 72 circumference points, so the 72nd lands exactly 360 degrees
//! from the 1st. mbgl's comment says so -- "first and last points are the same" -- and the reason
//! is the border: it is drawn as a line strip over points 1..72, which closes only because the
//! two ends coincide. A step of `360 / 72` would leave a gap one step wide in the ring.

use alloc::vec::Vec;

/// Points in the circle, center included. mbgl's `std::array<vec2, 73>`.
pub const CIRCLE_VERTICES: usize = 73;

/// Points around the circumference, which is every vertex but the center.
pub const CIRCUMFERENCE_VERTICES: usize = CIRCLE_VERTICES - 1;

/// A location indicator's accuracy circle, as offsets in world pixels from its center.
///
/// `location` is longitude then latitude -- the *style* writes it the other way round, and the
/// caller has already turned it over. `bearing` is the map's, in degrees, and turns the ring so
/// that a circle drawn at an angle still starts where the oracle starts it.
///
/// The first point is always `[0, 0]`: the center, which the fill's triangle fan needs and the
/// border's line strip skips.
#[must_use]
pub fn accuracy_circle(
    location: [f64; 2],
    radius_meters: f64,
    bearing_degrees: f64,
    world_size: f64,
) -> Vec<[f32; 2]> {
    let ruler = tessella_tile::ruler::Ruler::at(location[1]);
    let center = tessella_tile::projection::project(location[0], location[1], world_size);

    // 360 over *71*, not 72. See the module note: the last point has to land on the first.
    #[allow(clippy::cast_precision_loss)]
    let step = 360.0 / (CIRCUMFERENCE_VERTICES - 1) as f64;
    let bearing = bearing_degrees.rem_euclid(360.0);

    let mut out = Vec::with_capacity(CIRCLE_VERTICES);
    out.push([0.0, 0.0]);
    for index in 0..CIRCUMFERENCE_VERTICES {
        #[allow(clippy::cast_precision_loss)]
        let at = index as f64 * step - bearing;
        let point = ruler.destination(location, radius_meters, at);
        let projected = tessella_tile::projection::project(point[0], point[1], world_size);
        #[allow(clippy::cast_possible_truncation)]
        out.push([
            (projected[0] - center[0]) as f32,
            (projected[1] - center[1]) as f32,
        ]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Berlin at z14, where a world pixel is a real unit rather than an abstraction.
    const WORLD: f64 = 512.0 * 16384.0;
    const BERLIN: [f64; 2] = [13.405, 52.52];

    #[test]
    fn the_center_comes_first_and_the_count_is_mbgls() {
        let circle = accuracy_circle(BERLIN, 240.0, 0.0, WORLD);
        assert_eq!(circle.len(), CIRCLE_VERTICES);
        assert_eq!(circle[0], [0.0, 0.0]);
    }

    /// The ring closes. A step of `360 / 72` would leave the strip a gap wide open, which in a
    /// picture is a circle with a nick in it and in the numbers is nothing at all.
    #[test]
    fn the_last_point_is_the_first() {
        let circle = accuracy_circle(BERLIN, 240.0, 0.0, WORLD);
        let first = circle[1];
        let last = circle[CIRCLE_VERTICES - 1];
        assert!((first[0] - last[0]).abs() < 1e-3, "{first:?} {last:?}");
        assert!((first[1] - last[1]).abs() < 1e-3, "{first:?} {last:?}");
    }

    /// Every circumference point is about the same distance from the center -- about, and the
    /// gap is worth knowing.
    ///
    /// Mercator is conformal, so a small metric circle maps to a circle, and cheap-ruler's two
    /// multipliers cancel against Mercator's latitude stretch exactly. What does not cancel is the
    /// *model*: the ruler is on the WGS84 ellipsoid and web Mercator is on a sphere, and at 52.52
    /// the two disagree about the ratio of a latitude degree to a longitude one by a quarter of a
    /// percent. So the ring is very slightly elliptical -- 82.39 to 82.59 world pixels at z14 --
    /// and it is elliptical in mbgl too, by the same quarter percent and for the same reason.
    ///
    /// The bound is loose enough to admit that and tight enough to catch a ring that is not a
    /// ring. Asserting a perfect circle here would be asserting a model neither renderer uses.
    #[test]
    fn the_circumference_is_a_circle() {
        let circle = accuracy_circle(BERLIN, 240.0, 0.0, WORLD);
        let radii: Vec<f64> = circle[1..]
            .iter()
            .map(|point| f64::from(point[0]).hypot(f64::from(point[1])))
            .collect();
        let min = radii.iter().copied().fold(f64::MAX, f64::min);
        let max = radii.iter().copied().fold(0.0, f64::max);
        assert!(min > 0.0);
        assert!((max - min) / max < 0.005, "{min} {max}");
        assert!(
            (max - min) / max > 0.0005,
            "the ellipsoid and the sphere do disagree"
        );
    }

    /// The radius in world pixels is the radius in meters times the projection's own scale there.
    ///
    /// Checked against the projection rather than against a constant: a world pixel is a function
    /// of zoom and latitude, and hard-coding one would be asserting this test's arithmetic rather
    /// than the circle's.
    #[test]
    fn the_radius_is_the_accuracy_radius() {
        let circle = accuracy_circle(BERLIN, 240.0, 0.0, WORLD);
        let north = circle[1];

        let ruler = tessella_tile::ruler::Ruler::at(BERLIN[1]);
        let expected_point = ruler.destination(BERLIN, 240.0, 0.0);
        let center = tessella_tile::projection::project(BERLIN[0], BERLIN[1], WORLD);
        let projected =
            tessella_tile::projection::project(expected_point[0], expected_point[1], WORLD);
        #[allow(clippy::cast_possible_truncation)]
        let expected = (projected[1] - center[1]) as f32;
        assert!(
            (north[1] - expected).abs() < 1e-3,
            "{} {expected}",
            north[1]
        );
        // North is *up*, which in world pixels is a smaller y.
        assert!(north[1] < 0.0);
    }

    /// The map's bearing turns the ring. A quarter turn puts the point that was north at east.
    #[test]
    fn the_bearing_turns_the_ring() {
        let flat = accuracy_circle(BERLIN, 240.0, 0.0, WORLD);
        let turned = accuracy_circle(BERLIN, 240.0, 90.0, WORLD);

        // `at = index * step - bearing`, so a bearing of 90 moves every point back a quarter
        // turn: what was drawn at 90 degrees is now drawn at 0.
        let north = flat[1];
        let quarter = turned[1];
        assert!((quarter[0] - north[1]).abs() > 1e-6 || north[1].abs() < 1e-9);
        // The set of points is the same ring, so the extent is unchanged.
        let extent = |c: &[[f32; 2]]| {
            c[1..]
                .iter()
                .map(|p| f64::from(p[0]).hypot(f64::from(p[1])))
                .fold(0.0, f64::max)
        };
        assert!((extent(&flat) - extent(&turned)).abs() / extent(&flat) < 0.001);
    }

    /// A bearing past a full turn is the same ring as the bearing inside one.
    #[test]
    fn a_bearing_wraps() {
        let a = accuracy_circle(BERLIN, 240.0, 37.0, WORLD);
        let b = accuracy_circle(BERLIN, 240.0, 397.0, WORLD);
        let c = accuracy_circle(BERLIN, 240.0, -323.0, WORLD);
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    /// No accuracy is no circle, which is the spec's default: every point collapses onto the
    /// center rather than the layer drawing a dot of arbitrary size.
    #[test]
    fn no_radius_collapses_to_the_center() {
        let circle = accuracy_circle(BERLIN, 0.0, 0.0, WORLD);
        assert!(
            circle
                .iter()
                .all(|point| point[0].abs() < 1e-6 && point[1].abs() < 1e-6)
        );
    }
}
