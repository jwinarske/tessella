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
//! # The oracle turns the ring twice as far as it means to
//!
//! `bearing` here is the map's, in degrees. mbgl's is too -- `prepare` writes
//! `rad2deg(-state.getBearing())` into the render parameters -- and then `updateRadius` calls
//! `rad2deg` on it a second time before subtracting it, so a map bearing of 38 turns the oracle's
//! ring by `wrap(38 * 57.2958, 0, 360)` = 17.2 degrees. This does it once.
//!
//! The picture barely differs, which is why it went unnoticed: a 72-gon turned about its center is
//! nearly itself, and only the vertex phase and the quarter-percent ellipticity below move at all
//! -- 17 gross pixels at z14 against a scene otherwise identical. It is recorded here because a
//! bearing is the one camera the puck scene is held out of, and this is why.
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

/// A location indicator's geometry, ready to encode.
///
/// One vertex buffer and two index buffers over it, which is what mbgl builds: the dump shows two
/// drawables of `LocationIndicatorShader` sharing 73 vertices, one with 216 indices and one with
/// 72. The first is the accuracy circle's interior as triangles and the second is its border as a
/// line strip.
///
/// # Why the fan is expanded into indices
///
/// It already is in mbgl: the drawable path builds `{0, i, i + 1}` triples rather than asking for
/// a `GL_TRIANGLE_FAN`, and this carries them as they are. This protocol has no topology field --
/// a family implies it, the way the fill-outline family implies lines -- so triangles that say
/// what they are in their own indices need nothing added.
///
/// The border does not have that luxury. mbgl gives it `gfx::LineStrip(1.0f)` and 72 indices
/// walking the ring, and a strip is not a list of pairs: read as `Lines` it would draw 36
/// disconnected chords. The family has to carry the topology, which is what the
/// `LocationIndicatorShader` family means on this wire.
#[derive(Debug, Clone, PartialEq)]
pub struct LocationIndicatorBucket {
    /// The circle, center first, in world pixels from the puck.
    pub vertices: Vec<[f32; 2]>,
    /// Triangles over [`Self::vertices`], three indices each, fanning from the center.
    pub fill_indices: Vec<u16>,
    /// The border, as a line strip over the circumference.
    pub border_indices: Vec<u16>,
}

impl LocationIndicatorBucket {
    /// Builds the geometry for a puck at a location.
    ///
    /// `location` is longitude then latitude, already turned over from the style's order.
    #[must_use]
    pub fn new(
        location: [f64; 2],
        radius_meters: f64,
        bearing_degrees: f64,
        world_size: f64,
    ) -> Self {
        let vertices = accuracy_circle(location, radius_meters, bearing_degrees, world_size);

        // A fan from the center: triangle `i` is `0, i, i + 1`, for `i` up to the second-to-last
        // vertex. Seventy-one of those cover the disc.
        //
        // And then mbgl adds one more, `{0, vertexCount - 1, 1}`, closing the fan from the last
        // vertex back to the first. It is degenerate -- those two vertices are the same point,
        // which is what the 360/71 step arranges -- so it rasterizes nothing. It is here because
        // it is in the oracle's index buffer, and 216 rather than 213 is what the dump says.
        let mut fill_indices = Vec::with_capacity(CIRCUMFERENCE_VERTICES * 3);
        for index in 1..CIRCUMFERENCE_VERTICES {
            #[allow(clippy::cast_possible_truncation)]
            fill_indices.extend_from_slice(&[0, index as u16, (index + 1) as u16]);
        }
        #[allow(clippy::cast_possible_truncation)]
        fill_indices.extend_from_slice(&[0, CIRCUMFERENCE_VERTICES as u16, 1]);

        // The border skips the center and walks the ring. It closes because the last point is the
        // first, which is why the step is 360/71 -- see the module note.
        #[allow(clippy::cast_possible_truncation)]
        let border_indices = (1..=CIRCUMFERENCE_VERTICES).map(|i| i as u16).collect();

        Self {
            vertices,
            fill_indices,
            border_indices,
        }
    }

    /// Whether this drew nothing, which is a puck with no accuracy radius.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.vertices.len() < CIRCLE_VERTICES
    }
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

    /// The two index buffers are the oracle's two: 216 and 72.
    ///
    /// Those numbers are read off the dump rather than derived here -- `ilen=216` and `ilen=72`
    /// on two drawables of `sh0028` sharing `vlen=73` -- which is what makes them a check on this
    /// rather than a restatement of it.
    #[test]
    fn the_index_counts_are_the_oracles() {
        let bucket = LocationIndicatorBucket::new(BERLIN, 240.0, 0.0, WORLD);
        assert_eq!(bucket.vertices.len(), 73);
        assert_eq!(bucket.fill_indices.len(), 216);
        assert_eq!(bucket.border_indices.len(), 72);
    }

    /// Every triangle of the fan starts at the center and walks one step round.
    #[test]
    fn the_fill_fans_from_the_center() {
        let bucket = LocationIndicatorBucket::new(BERLIN, 240.0, 0.0, WORLD);
        let triangles = bucket.fill_indices.as_chunks::<3>().0;
        for (triangle, chunk) in triangles[..71].iter().enumerate() {
            let step = u16::try_from(triangle).expect("71 triangles");
            assert_eq!(*chunk, [0, step + 1, step + 2], "triangle {triangle}");
        }
        // And the closing one, which mbgl emits and which is degenerate: vertex 72 is vertex 1.
        assert_eq!(triangles[71], [0, 72, 1]);
        let first = bucket.vertices[1];
        let last = bucket.vertices[72];
        assert!((first[0] - last[0]).abs() < 1e-3 && (first[1] - last[1]).abs() < 1e-3);
    }

    /// The border walks the ring and skips the center. Including index zero would draw a spoke
    /// from the middle to the rim, which is a line the oracle does not have.
    #[test]
    fn the_border_skips_the_center() {
        let bucket = LocationIndicatorBucket::new(BERLIN, 240.0, 0.0, WORLD);
        assert!(!bucket.border_indices.contains(&0));
        assert_eq!(bucket.border_indices[0], 1);
        assert_eq!(bucket.border_indices[71], 72);
    }

    /// The strip closes, which is the whole reason the step is 360/71: the vertex the border ends
    /// on is the vertex it started on.
    #[test]
    fn the_border_closes_on_itself() {
        let bucket = LocationIndicatorBucket::new(BERLIN, 240.0, 0.0, WORLD);
        let first = bucket.vertices[usize::from(bucket.border_indices[0])];
        let last = bucket.vertices[usize::from(bucket.border_indices[71])];
        assert!((first[0] - last[0]).abs() < 1e-3);
        assert!((first[1] - last[1]).abs() < 1e-3);
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
