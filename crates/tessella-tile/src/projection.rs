//! Spherical Mercator, spelled to match the oracle bit for bit.
//!
//! # Why the arithmetic is written the way it is
//!
//! This is not the textbook formula transcribed loosely. Every operation is in the order
//! `mbgl/util/projection.hpp` performs it, because floating-point addition and multiplication
//! do not associate and the difference shows up in the last bit.
//!
//! That matters more than it sounds. §6.3 gates `CameraUpdate` emission on an **exact** f64
//! compare of the camera fields, so a one-ULP disagreement is not a rounding curiosity — it is
//! a camera that reports itself changed on every frame of a parked map, which is precisely the
//! zero-bytes-when-parked guarantee DR-8 makes and §9.3 asserts in CI. And `centerZoom0` is the
//! field `frame_diff.hpp` documents a historic flicker regression against.
//!
//! The concrete instance: mbgl converts radians to degrees as `rad * 180.0 / π`, multiplying
//! before dividing. The obvious alternative, multiplying by a precomputed `180/π`, gives a
//! different last bit — measured against the golden dump, not assumed. There is a test.
//!
//! # Two world sizes, and the trap between them
//!
//! `project` takes a world size rather than a zoom, and mbgl calls it two ways that look alike
//! and are not:
//!
//! - With a **scale**, where the world is `scale * 512` pixels. This is the map's own space.
//! - With an **integer zoom**, where mbgl passes `1 << zoom` *directly* as the world size, not
//!   `(1 << zoom) * 512`. The result is in tile units — the tile containing a point is its
//!   integer part.
//!
//! Reading the second as the first is a factor-of-512 error that looks like a projection bug.
//! [`tile_units`] and [`world_pixels`] name the two so the caller cannot pick wrong.

/// Tile side in pixels. The world at zoom zero is one tile.
pub const TILE_SIZE: f64 = 512.0;

/// Tile-local coordinate extent. A tile spans `0..=EXTENT` on each axis.
pub const EXTENT: i32 = 8192;

/// Latitude beyond which Mercator is not defined for a square world.
///
/// The exact value matters: it is the latitude whose projected y is exactly the world height,
/// and mbgl clamps to this rather than to 85 or 85.05.
///
/// `constants.hpp` writes it as `85.051128779806604`. That trailing `4` does not change the
/// nearest f64 — clippy is right that it is redundant — but the literal is kept as the source
/// spells it so the two are greppably the same, and a test pins that the truncation really is
/// value-preserving rather than merely assumed to be.
#[allow(clippy::excessive_precision)]
pub const LATITUDE_MAX: f64 = 85.051_128_779_806_604;

/// Longitude bound.
pub const LONGITUDE_MAX: f64 = 180.0;

/// Degrees in a full turn.
pub const DEGREES_MAX: f64 = 360.0;

/// Converts radians to degrees exactly as mbgl does.
///
/// The order is load-bearing. `rad * 180.0 / π` and `rad * (180.0 / π)` differ in the last bit,
/// and only the first reproduces the oracle.
#[must_use]
fn rad2deg(rad: f64) -> f64 {
    rad * 180.0 / core::f64::consts::PI
}

/// Projects a longitude and latitude into a world of `world_size` units on a side.
///
/// Latitude is clamped to [`LATITUDE_MAX`]; longitude is not, so a point past the antimeridian
/// projects outside `0..world_size` and the caller decides which world copy it belongs to.
#[must_use]
pub fn project(longitude: f64, latitude: f64, world_size: f64) -> [f64; 2] {
    let latitude = latitude.clamp(-LATITUDE_MAX, LATITUDE_MAX);
    let scale = world_size / DEGREES_MAX;
    [
        (LONGITUDE_MAX + longitude) * scale,
        (LONGITUDE_MAX
            - rad2deg(
                (core::f64::consts::PI / 4.0 + latitude * core::f64::consts::PI / DEGREES_MAX)
                    .tan()
                    .ln(),
            ))
            * scale,
    ]
}

/// Projects into world pixels at a given scale, where the world is `scale * 512` pixels.
#[must_use]
pub fn world_pixels(longitude: f64, latitude: f64, scale: f64) -> [f64; 2] {
    project(longitude, latitude, scale * TILE_SIZE)
}

/// Projects into tile units at an integer zoom, where the world is `2^zoom` tiles.
///
/// The integer part of each coordinate is the tile that contains the point, and the fraction is
/// the position within it.
#[must_use]
pub fn tile_units(longitude: f64, latitude: f64, zoom: u8) -> [f64; 2] {
    project(longitude, latitude, f64::from(1u32 << zoom))
}

/// The map center at zoom zero, which is what the camera block carries (§2.2).
///
/// Scale-free on purpose, in `0..512` whatever the map's zoom. Sending it pre-multiplied by a
/// frame's zoom scale couples it to that frame: a consumer placing tiles from a slightly
/// different zoom then disagrees about scale by over a million units at zoom 17, so the camera
/// looks where the tiles are not and frames come back empty — visible only while zooming, as
/// flicker.
#[must_use]
pub fn center_zoom0(longitude: f64, latitude: f64) -> [f64; 2] {
    project(longitude, latitude, TILE_SIZE)
}

/// Inverts [`project`].
#[must_use]
pub fn unproject(point: [f64; 2], world_size: f64) -> (f64, f64) {
    let scaled = [
        point[0] * DEGREES_MAX / world_size,
        point[1] * DEGREES_MAX / world_size,
    ];
    let latitude = (((LONGITUDE_MAX - scaled[1]) * core::f64::consts::PI / LONGITUDE_MAX).exp())
        .atan()
        * DEGREES_MAX
        / core::f64::consts::PI
        - 90.0;
    (scaled[0] - LONGITUDE_MAX, latitude)
}

/// A point's position within the tile that contains it, in `0..EXTENT` units.
///
/// Returned as f64 rather than i16 because clipping and simplification happen before rounding:
/// a coordinate outside `0..EXTENT` is a point in a neighbouring tile that this tile's buffer
/// may still need, and rounding it here would lose the sign information that says so.
#[must_use]
pub fn tile_local(longitude: f64, latitude: f64, zoom: u8, tile_x: u32, tile_y: u32) -> [f64; 2] {
    let units = tile_units(longitude, latitude, zoom);
    let extent = f64::from(EXTENT);
    [
        (units[0] - f64::from(tile_x)) * extent,
        (units[1] - f64::from(tile_y)) * extent,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bit-exact agreement with the oracle.
    ///
    /// These are the `centerZoom0` bits from `tests/golden/hermetic_style.dump`, at the camera
    /// the probe jumps to. Comparing bit patterns rather than an epsilon is the point: §6.3
    /// gates camera emission on an exact f64 compare, so "close enough" here is a camera that
    /// reports itself changed every frame on a parked map.
    #[test]
    fn center_zoom0_matches_the_oracle_bit_for_bit() {
        let center = center_zoom0(-0.11, 51.505);
        assert_eq!(center[0].to_bits(), 0x406f_fafe_6838_6f0c);
        assert_eq!(center[1].to_bits(), 0x4065_4846_0db7_9819);
    }

    /// The one-ULP trap, pinned so nobody "simplifies" it away.
    ///
    /// Multiplying by a precomputed `180/π` is the obvious optimization and it produces a
    /// different last bit. This asserts the two really do differ, so the test above is
    /// demonstrably testing something.
    #[test]
    fn the_radian_conversion_order_is_load_bearing() {
        let latitude: f64 = 51.505;
        let inner = (core::f64::consts::PI / 4.0 + latitude * core::f64::consts::PI / DEGREES_MAX)
            .tan()
            .ln();

        let ours = rad2deg(inner);
        let tempting = inner * (180.0 / core::f64::consts::PI);
        assert_ne!(
            ours.to_bits(),
            tempting.to_bits(),
            "if these ever agree, the ordering no longer matters and this test can go"
        );
        assert_eq!(
            ours.to_bits(),
            (inner * 180.0 / core::f64::consts::PI).to_bits()
        );
    }

    /// The extra digit clippy objects to is provably redundant, which is why keeping mbgl's
    /// spelling costs nothing.
    #[test]
    fn the_latitude_bound_matches_its_shorter_spelling() {
        assert_eq!(LATITUDE_MAX.to_bits(), 85.051_128_779_806_6_f64.to_bits());
        // But not arbitrarily shorter: this one is a different number.
        assert_ne!(LATITUDE_MAX.to_bits(), 85.051_128_779_807_f64.to_bits());
    }

    /// The world is one tile at zoom zero, and the origin is the northwest corner.
    #[test]
    fn the_world_has_the_expected_corners() {
        let nw = center_zoom0(-180.0, LATITUDE_MAX);
        assert!(nw[0].abs() < 1e-9, "{}", nw[0]);
        assert!(nw[1].abs() < 1e-9, "{}", nw[1]);

        let se = center_zoom0(180.0, -LATITUDE_MAX);
        assert!((se[0] - 512.0).abs() < 1e-9, "{}", se[0]);
        assert!((se[1] - 512.0).abs() < 1e-9, "{}", se[1]);

        let origin = center_zoom0(0.0, 0.0);
        assert!((origin[0] - 256.0).abs() < 1e-9);
        assert!((origin[1] - 256.0).abs() < 1e-9);
    }

    /// Tile units and world pixels differ by exactly the tile size. Reading one as the other is
    /// a factor-of-512 error that presents as a projection bug.
    #[test]
    fn tile_units_are_not_world_pixels() {
        let units = tile_units(-0.11, 51.505, 13);
        let pixels = world_pixels(-0.11, 51.505, f64::from(1u32 << 13));
        for axis in 0..2 {
            assert!(
                (pixels[axis] / units[axis] - TILE_SIZE).abs() < 1e-9,
                "axis {axis}"
            );
        }
    }

    /// The projection and the oracle's tile addressing agree on something checkable.
    ///
    /// The golden dump covers x in 4092..=4094 and y in 2723..=2724 at zoom 13 — a 3x2 cover
    /// for a 1024x768 viewport. The camera must land in the middle column of that, which both
    /// pins the projection and confirms the cover is centred where it should be. Picking the
    /// first tile that appears in the dump instead would have asserted 4092, and passed only
    /// by being wrong in a way nothing else checked.
    #[test]
    fn the_camera_lands_in_the_middle_of_the_oracles_cover() {
        let units = tile_units(-0.11, 51.505, 13);
        let (x, y) = (units[0].floor() as u32, units[1].floor() as u32);
        assert_eq!((x, y), (4093, 2724));

        // Interior on x, where the cover is three wide. On y the cover is only two deep and
        // the camera sits in the lower row, near the boundary — which is why the row above is
        // covered at all.
        assert!(x > 4092 && x < 4094, "middle column of 4092..=4094");
        assert!((2723..=2724).contains(&y));
    }

    #[test]
    fn latitude_is_clamped_rather_than_diverging() {
        let pole = center_zoom0(0.0, 90.0);
        let clamped = center_zoom0(0.0, LATITUDE_MAX);
        assert_eq!(pole, clamped);
        assert!(pole[1].is_finite(), "the pole must not project to infinity");

        let south = center_zoom0(0.0, -90.0);
        assert_eq!(south, center_zoom0(0.0, -LATITUDE_MAX));
    }

    /// Longitude is deliberately not clamped: a point past the antimeridian belongs to another
    /// world copy, and which one is the caller's business.
    #[test]
    fn longitude_is_not_clamped() {
        let wrapped = center_zoom0(190.0, 0.0);
        assert!(wrapped[0] > 512.0, "{}", wrapped[0]);
    }

    #[test]
    fn projection_round_trips() {
        for (longitude, latitude) in [(0.0, 0.0), (-0.11, 51.505), (139.7, 35.68), (-74.0, -33.9)] {
            let projected = center_zoom0(longitude, latitude);
            let (back_lon, back_lat) = unproject(projected, TILE_SIZE);
            assert!(
                (back_lon - longitude).abs() < 1e-9,
                "{longitude} -> {back_lon}"
            );
            assert!(
                (back_lat - latitude).abs() < 1e-9,
                "{latitude} -> {back_lat}"
            );
        }
    }

    /// Tile-local coordinates are relative to the tile's northwest corner and span the extent.
    #[test]
    fn tile_local_spans_the_extent() {
        // The northwest corner of tile 4092/2723 at zoom 13.
        let (lon, lat) = unproject([4092.0, 2723.0], f64::from(1u32 << 13));
        let local = tile_local(lon, lat, 13, 4092, 2723);
        assert!(local[0].abs() < 1e-6, "{}", local[0]);
        assert!(local[1].abs() < 1e-6, "{}", local[1]);

        // The southeast corner is one tile further on, which is EXTENT units.
        let (lon, lat) = unproject([4093.0, 2724.0], f64::from(1u32 << 13));
        let local = tile_local(lon, lat, 13, 4092, 2723);
        assert!((local[0] - f64::from(EXTENT)).abs() < 1e-6, "{}", local[0]);
        assert!((local[1] - f64::from(EXTENT)).abs() < 1e-6, "{}", local[1]);
    }

    /// A point outside the tile keeps its sign rather than being clamped, because a negative
    /// coordinate is what says "this belongs to the neighbour, and this tile's buffer wants it".
    #[test]
    fn tile_local_does_not_clamp_to_the_tile() {
        let (lon, lat) = unproject([4091.5, 2722.5], f64::from(1u32 << 13));
        let local = tile_local(lon, lat, 13, 4092, 2723);
        assert!(local[0] < 0.0, "{}", local[0]);
        assert!(local[1] < 0.0, "{}", local[1]);
    }
}
