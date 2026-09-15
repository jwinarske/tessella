//! Cheap ruler: distances and bearings near one latitude, without a geodesic solver.
//!
//! A transcription of `cheap-ruler-cpp`'s constructor and `destination`, which is the whole of
//! what mbgl's location indicator uses it for -- 72 points around an accuracy circle, all within a
//! few hundred meters of each other.
//!
//! # Why an approximation is the exact answer here
//!
//! Because it is the one the oracle uses. A geodesic solver would be *more* correct and would put
//! the circle's points somewhere `mbgl-render` does not, which is the one thing a transcription
//! cannot do. The approximation is also a good one at the scale it is used: it flattens the
//! ellipsoid to a plane whose two scale factors are taken at the ruler's own latitude, so the
//! error grows with distance from it and is far below a pixel over an accuracy radius.
//!
//! # The two multipliers
//!
//! `kx` and `ky` convert a degree of longitude and of latitude into meters, from the normal and
//! meridional radii of curvature at the latitude the ruler was made at. Both come out of the
//! WGS84 ellipsoid, and the constants below are cheap-ruler's own: an equatorial radius in
//! *kilometers* and a flattening, with the unit conversion folded into `mul`.

/// Equatorial radius in kilometers, which is the unit cheap-ruler works in.
const EQUATORIAL_RADIUS_KM: f64 = 6378.137;

/// WGS84 flattening.
const FLATTENING: f64 = 1.0 / 298.257_223_563;

/// First eccentricity squared, from the flattening.
const E2: f64 = FLATTENING * (2.0 - FLATTENING);

/// Degrees to radians, spelled as cheap-ruler spells it.
const RAD: f64 = core::f64::consts::PI / 180.0;

/// Kilometers to meters, which is the unit this ruler is built in.
const METERS_PER_KM: f64 = 1000.0;

/// A flat approximation of the ellipsoid near one latitude.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ruler {
    /// Meters per degree of longitude here.
    kx: f64,
    /// Meters per degree of latitude here.
    ky: f64,
}

impl Ruler {
    /// A ruler for the given latitude, measuring in meters.
    ///
    /// The curvature formulas are cheap-ruler's, which cites
    /// <https://en.wikipedia.org/wiki/Earth_radius#Meridional>: `kx` from the normal radius of
    /// curvature and `ky` from the meridional one.
    #[must_use]
    pub fn at(latitude: f64) -> Self {
        let mul = RAD * EQUATORIAL_RADIUS_KM * METERS_PER_KM;
        let coslat = (latitude * RAD).cos();
        let w2 = 1.0 / (1.0 - E2 * (1.0 - coslat * coslat));
        let w = w2.sqrt();
        Self {
            kx: mul * w * coslat,
            ky: mul * w * w2 * (1.0 - E2),
        }
    }

    /// Meters per degree of longitude at this ruler's latitude.
    #[must_use]
    pub const fn kx(self) -> f64 {
        self.kx
    }

    /// Meters per degree of latitude at this ruler's latitude.
    #[must_use]
    pub const fn ky(self) -> f64 {
        self.ky
    }

    /// The point `distance` meters from `origin` along `bearing` degrees clockwise from north.
    ///
    /// `origin` and the result are `[longitude, latitude]`.
    #[must_use]
    pub fn destination(self, origin: [f64; 2], distance: f64, bearing: f64) -> [f64; 2] {
        let a = bearing * RAD;
        self.offset(origin, a.sin() * distance, a.cos() * distance)
    }

    /// The point `dx` meters east and `dy` meters north of `origin`.
    #[must_use]
    pub fn offset(self, origin: [f64; 2], dx: f64, dy: f64) -> [f64; 2] {
        [origin[0] + dx / self.kx, origin[1] + dy / self.ky]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The multipliers at the equator, where the normal radius is the equatorial one.
    ///
    /// A degree of longitude at the equator is about 111.3 km, and a degree of latitude about
    /// 110.6 -- latitude is *shorter* there, which is the ellipsoid's flattening and is the sign
    /// that `kx` and `ky` have not been swapped. Swapping them is invisible at the equator in a
    /// picture and is a tenth of a percent in the numbers.
    #[test]
    fn the_equators_multipliers_are_the_ellipsoids() {
        let ruler = Ruler::at(0.0);
        assert!((ruler.kx() - 111_319.49).abs() < 1.0, "{}", ruler.kx());
        assert!((ruler.ky() - 110_574.27).abs() < 1.0, "{}", ruler.ky());
        assert!(
            ruler.ky() < ruler.kx(),
            "latitude is the shorter degree here"
        );
    }

    /// At Berlin's latitude a degree of longitude is roughly cos(52.52) of the equator's, and a
    /// degree of latitude is slightly *longer* than the equator's rather than shorter.
    ///
    /// "Roughly", and the gap is the point: the ratio is about 0.6098 where the cosine is 0.6078,
    /// three parts in a thousand apart. That difference is the normal radius of curvature growing
    /// with latitude -- the `w` term -- and it is what makes this an ellipsoid ruler rather than a
    /// spherical one. A tolerance tight enough to reject it would be asserting the wrong model.
    #[test]
    fn longitude_shortens_with_latitude_and_latitude_lengthens() {
        let ruler = Ruler::at(52.52);
        let equator = Ruler::at(0.0);
        let ratio = ruler.kx() / equator.kx();
        assert!((ratio - (52.52 * RAD).cos()).abs() < 0.005, "{ratio}");
        assert!(
            ratio > (52.52 * RAD).cos(),
            "the ellipsoid makes it larger, not smaller"
        );
        assert!(ruler.ky() > equator.ky());
        assert!(
            ruler.ky() > ruler.kx(),
            "latitude is the longer degree here"
        );
    }

    /// North, east, south and west from a point, at a distance a puck's accuracy radius reaches.
    #[test]
    fn the_four_cardinals_go_where_they_should() {
        let ruler = Ruler::at(52.52);
        let origin = [13.405, 52.52];
        let north = ruler.destination(origin, 240.0, 0.0);
        let east = ruler.destination(origin, 240.0, 90.0);
        let south = ruler.destination(origin, 240.0, 180.0);
        let west = ruler.destination(origin, 240.0, 270.0);

        assert!(north[1] > origin[1] && (north[0] - origin[0]).abs() < 1e-9);
        assert!(south[1] < origin[1] && (south[0] - origin[0]).abs() < 1e-9);
        assert!(east[0] > origin[0] && (east[1] - origin[1]).abs() < 1e-9);
        assert!(west[0] < origin[0] && (west[1] - origin[1]).abs() < 1e-9);

        // And symmetric: 240 m north and 240 m south are the same distance away.
        assert!(((north[1] - origin[1]) + (south[1] - origin[1])).abs() < 1e-12);
        assert!(((east[0] - origin[0]) + (west[0] - origin[0])).abs() < 1e-12);
    }

    /// A bearing of 45 degrees is not 45 degrees of *longitude and latitude*: a degree of
    /// longitude is shorter here, so the same easting is more degrees than the same northing.
    #[test]
    fn a_diagonal_bearing_is_not_a_diagonal_in_degrees() {
        let ruler = Ruler::at(52.52);
        let origin = [13.405, 52.52];
        let at = ruler.destination(origin, 1000.0, 45.0);
        let east = at[0] - origin[0];
        let north = at[1] - origin[1];
        assert!(east > north, "a degree of longitude is shorter at 52.52");
        // The ratio is the two multipliers', which is what makes this a ruler rather than a
        // rotation.
        assert!((east / north - ruler.ky() / ruler.kx()).abs() < 1e-9);
    }

    /// Zero distance stays put, whatever the bearing.
    #[test]
    fn no_distance_is_no_movement() {
        let ruler = Ruler::at(-33.9);
        let origin = [18.42, -33.9];
        for bearing in [0.0, 37.0, 180.0, 359.5] {
            assert_eq!(ruler.destination(origin, 0.0, bearing), origin);
        }
    }
}
