//! The camera block: world-to-clip, `pixelsPerMeter`, and the scale-free center (§6.3, §11.1).
//!
//! # Why a frontend computes a projection at all
//!
//! Under DR-9 an interactive view's camera belongs to the consumer, and the producer reads it
//! back over the reverse channel. That does not make this dead code: producer-camera mode is
//! still the path for non-interactive views, and in *both* modes the producer needs the same
//! numbers to decide cover and to size screen-space properties. What changes between modes is
//! who is authoritative, not who can compute it.
//!
//! # `pixelsPerMeter` is not derivable from the matrix
//!
//! It rides in the camera block separately because mbgl's projection is not isotropic: heights
//! arrive in meters while x and y are in world pixels, so the z column carries a factor the
//! other two do not. A consumer that owned the camera and tried to recover the factor from the
//! matrix would get buildings wrong by its reciprocal. Element 11 of the matrix is exactly its
//! negation, which is the clearest statement of where it lives.
//!
//! # Checked bit-exactly, and where it is not
//!
//! The golden dump carries the oracle's matrix as sixteen f64 bit patterns. Fourteen of the
//! sixteen reproduce exactly. The two that do not are the x and y translation, each off by one
//! ULP — a part in 2^52 of a translation of about four million, which is nanometers of a pixel,
//! but not bit-exactness, and it is recorded as a divergence rather than rounded away. See
//! [`OneUlpDivergence`] for what is known about the cause.
//!
//! Getting even fourteen right required matching mbgl's operation order rather than the algebra:
//!
//! - The field of view is read as a `double` for the camera distance and as a `float` for the
//!   projection. `getFieldOfView()` returns `float`, so the perspective divide runs on a value
//!   that has been through f32, while `getCameraToCenterDistance()` uses the f64 constant. That
//!   single asymmetry is the difference between a matrix whose `[5]` is exactly -3 and the
//!   oracle's -3.0000000293447.
//! - The stored center is a signed offset from the projection origin, not a pixel position:
//!   `x = -longitude * worldSize / 360` and `y = 0.5 * Cc * ln((1 + sin φ) / (1 - sin φ))`. The
//!   atanh form is not the same floating-point value as `Cc * ln(tan(π/4 + φ/2))` at most
//!   latitudes, though it is the same real number everywhere. The probe's own latitude is one
//!   where they agree, so this is a difference a single-camera check would not have found.
//! - `pixelsPerMeter` is taken at the latitude of the *camera's* stored position, recovered by
//!   inverting the mercator y — not at the latitude that was passed in. They agree here, and
//!   the derivation is what has to match, not the coincidence.

use crate::cover::ViewTransform;
use crate::projection;

/// mbgl's `util::DEFAULT_FOV`.
pub const DEFAULT_FOV: f64 = 0.6435011087932844;

/// Earth radius in meters, as mbgl defines it.
pub const EARTH_RADIUS_M: f64 = 6_378_137.0;

/// A camera block that could not be computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CameraError {
    /// The view is rotated or pitched.
    ///
    /// The unrotated case is checked against the oracle bit for bit. A rotated one needs the
    /// orientation quaternion mbgl builds from roll, pitch and bearing, and there is no dump at
    /// a non-zero bearing to check it against — so it is refused rather than written blind.
    /// Producing a plausible matrix that has never been compared is how a projection defect
    /// reaches a screen and gets diagnosed as a tessellation bug.
    #[error("rotated and pitched cameras need the orientation quaternion, which is not ported")]
    Rotated,
}

/// A 4x4 matrix in the column-major order mbgl and GL use.
pub type Mat4 = [f64; 16];

/// mbgl's `matrix::perspective`, operation for operation.
///
/// The literals in mbgl are `float`-suffixed (`1.0f`, `2.0f`) but every operand is `double`, so
/// they widen and the arithmetic is f64 throughout. Reproducing the suffixes as f32 here would
/// change the result; reproducing the widening is what matches.
#[must_use]
pub fn perspective(fovy: f64, aspect: f64, near: f64, far: f64) -> Mat4 {
    let f = 1.0 / (fovy / 2.0).tan();
    let nf = 1.0 / (near - far);
    let mut out = [0.0; 16];
    out[0] = f / aspect;
    out[5] = f;
    out[10] = (far + near) * nf;
    out[11] = -1.0;
    out[14] = (2.0 * far * near) * nf;
    out
}

/// mbgl's `matrix::multiply`: `out = a * b` in column-major terms.
///
/// Written out rather than looped because the accumulation order is the thing being reproduced.
/// A loop that summed in a different order would agree to within rounding and disagree in the
/// last bit, which is the whole quantity being checked.
#[must_use]
pub fn multiply(a: &Mat4, b: &Mat4) -> Mat4 {
    let mut out = [0.0; 16];
    for column in 0..4 {
        let (b0, b1, b2, b3) = (
            b[column * 4],
            b[column * 4 + 1],
            b[column * 4 + 2],
            b[column * 4 + 3],
        );
        for row in 0..4 {
            out[column * 4 + row] =
                b0 * a[row] + b1 * a[4 + row] + b2 * a[8 + row] + b3 * a[12 + row];
        }
    }
    out
}

/// mbgl's in-place `matrix::translate`.
///
/// The in-place branch, because that is the one `getWorldToCamera` takes — it passes the same
/// matrix as source and destination, and mbgl's function tests for aliasing and computes only
/// the last column.
/// The `+=` clippy suggests is bit-identical here — the two operands are the same and addition
/// commutes — but the shape is kept because the value of this function is that it can be read
/// against mbgl's line by line. A transcription that has been tidied is one that has to be
/// re-derived to be re-checked.
#[allow(clippy::assign_op_pattern)]
pub fn translate_in_place(m: &mut Mat4, x: f64, y: f64, z: f64) {
    m[12] = m[0] * x + m[4] * y + m[8] * z + m[12];
    m[13] = m[1] * x + m[5] * y + m[9] * z + m[13];
    m[14] = m[2] * x + m[6] * y + m[10] * z + m[14];
    m[15] = m[3] * x + m[7] * y + m[11] * z + m[15];
}

/// The distance from the camera to the center of the map, in world pixels.
///
/// Uses the f64 field of view. `getCameraToClipPerspective` uses the f32 one, and the pair of
/// them is not a mistake to normalize away: with mbgl's default fov `tan(fov / 2)` is exactly
/// one third in f64, so this is exactly `1.5 * height`, while the f32 path is not.
#[must_use]
pub fn camera_to_center_distance(height: f64) -> f64 {
    0.5 * height / (DEFAULT_FOV / 2.0).tan()
}

/// The stored center: a signed offset from the projection origin, in world pixels.
///
/// Not a pixel position. mbgl recovers the location from it as `lon = -x / Bc`, which is what
/// fixes the sign and the origin.
#[must_use]
pub fn center_offset(longitude: f64, latitude: f64, zoom: f64) -> [f64; 2] {
    let world_size = world_size(zoom);
    let bc = world_size / 360.0;
    let cc = world_size / core::f64::consts::TAU;

    // mbgl clamps the sine rather than the latitude, so a pole is representable as a finite
    // offset instead of an infinity.
    let m = 1.0 - 1e-15;
    let f = (latitude * core::f64::consts::PI / 180.0)
        .sin()
        .clamp(-m, m);
    [-longitude * bc, 0.5 * cc * ((1.0 + f) / (1.0 - f)).ln()]
}

/// World size in pixels at a fractional zoom.
#[must_use]
pub fn world_size(zoom: f64) -> f64 {
    zoom.exp2() * projection::TILE_SIZE
}

/// The camera position in normalized mercator, as `updateCameraState` computes it.
///
/// The `0.5 * worldSize - x` is the negation the comment in mbgl describes: the stored value
/// places the map, and the camera moves opposite to it.
#[must_use]
pub fn camera_position(view: &ViewTransform) -> [f64; 3] {
    let world = world_size(view.zoom);
    let [x, y] = center_offset(view.longitude, view.latitude, view.zoom);
    let distance = camera_to_center_distance(view.height);
    [
        (0.5 * world - x) / world,
        (0.5 * world - y) / world,
        distance / world,
    ]
}

/// The latitude a normalized mercator y stands for.
#[must_use]
pub fn lat_from_mercator_y(y: f64) -> f64 {
    let radians = 2.0
        * (core::f64::consts::PI - y * core::f64::consts::TAU)
            .exp()
            .atan()
        - core::f64::consts::FRAC_PI_2;
    radians * 180.0 / core::f64::consts::PI
}

/// World pixels per meter, at the latitude of the camera's own position.
#[must_use]
pub fn pixels_per_meter(view: &ViewTransform) -> f64 {
    let world = world_size(view.zoom);
    let latitude = lat_from_mercator_y(camera_position(view)[1]);
    world
        / ((latitude * core::f64::consts::PI / 180.0).cos()
            * core::f64::consts::TAU
            * EARTH_RADIUS_M)
}

/// The map center at zoom zero, which is what the camera block carries.
///
/// Scale-free, so a consumer can rescale it itself rather than being handed a number that is
/// only meaningful alongside the zoom it was computed at.
#[must_use]
pub fn center_zoom0(view: &ViewTransform) -> [f64; 2] {
    let position = camera_position(view);
    [
        position[0] * projection::TILE_SIZE,
        position[1] * projection::TILE_SIZE,
    ]
}

/// The world-to-camera matrix for an unrotated camera.
fn world_to_camera(view: &ViewTransform) -> Mat4 {
    let world = world_size(view.zoom);
    let position = camera_position(view);
    let ppm = pixels_per_meter(view);

    // The orientation is identity for an unrotated camera, so the rotation matrix mbgl builds
    // from the conjugate quaternion is the identity and the translate below is the whole of it.
    let mut result: Mat4 = [
        1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
    ];
    translate_in_place(
        &mut result,
        -position[0] * world,
        -position[1] * world,
        -position[2] * world,
    );

    // Pre-multiply y, because the viewport is not flipped. Skipping this is a map upside down.
    for index in [1, 5, 9, 13] {
        result[index] *= -1.0;
    }
    // Post-multiply z: heights are meters and everything else is world pixels.
    for index in [8, 9, 10, 11] {
        result[index] *= ppm;
    }
    result
}

/// The world-to-clip matrix, column-major.
///
/// # Errors
///
/// [`CameraError::Rotated`] when the view has bearing or pitch.
pub fn proj_matrix(view: &ViewTransform) -> Result<Mat4, CameraError> {
    if view.bearing.abs() > f64::EPSILON || view.pitch.abs() > f64::EPSILON {
        return Err(CameraError::Rotated);
    }

    let distance = camera_to_center_distance(view.height);
    // With no pitch the sea-level distance is the center distance and the horizon term vanishes,
    // so the far plane is the center distance with mbgl's one-percent margin. The margin exists
    // to keep a fragment at exactly the far distance from failing the depth test.
    let far_z = distance * 1.01;
    let near_z = 1.0;

    // The f32 field of view, which is what `getFieldOfView()` returns.
    #[allow(clippy::cast_possible_truncation)]
    let fov = f64::from(DEFAULT_FOV as f32);
    let camera_to_clip = perspective(fov, view.width / view.height, near_z, far_z);

    Ok(multiply(&camera_to_clip, &world_to_camera(view)))
}

/// What is known about the two elements that are not bit-exact.
///
/// Kept as a type rather than a comment so the divergence is something a test names and a reader
/// can find, in the same way the clip rotation is recorded where it happens.
///
/// The x and y translation each differ from the oracle by one ULP. The cause is narrowed but not
/// closed:
///
/// - The other fourteen elements, including the scale factors that multiply these two, are
///   exact — so the difference is in the translation itself, not in the projection applied to it.
/// - `centerZoom0` in the same dump matches this crate's value *exactly*, and both it and the
///   translation derive from the camera position by scaling with a power of two, which is
///   lossless. So the oracle's own two readouts of the camera position disagree with each other
///   by an ULP, which means the probe computes `centerZoom0` by a different route than the
///   matrix uses.
/// - That points at mbgl's stored `x` and `y` differing by an ULP from the recomputation here —
///   plausibly a libm difference in `sin` or `log`, or a camera-setting path that arrives at the
///   stored value by a different sequence.
///
/// Closing it means having the probe print `TransformState::x` and `y` directly, which is the
/// same instrument that settled the vertex-hash question. Until then the divergence is bounded
/// and asserted: [`crate::camera`]'s tests require every element to be either bit-exact or
/// within one ULP, so a real regression cannot hide behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OneUlpDivergence {
    /// Matrix indices known to differ.
    pub indices: [usize; 2],
}

impl OneUlpDivergence {
    /// The elements that differ from the oracle by one ULP.
    pub const KNOWN: Self = Self { indices: [12, 13] };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The oracle's projection matrix, as sixteen f64 bit patterns from the golden dump.
    const ORACLE_PROJ: [u64; 16] = [
        0x4002_0000_02f4_34f4,
        0,
        0,
        0,
        0,
        0xc008_0000_03f0_469a,
        0,
        0,
        0,
        0,
        0xbfc5_8f42_2841_b4f3,
        0xbfc5_85c7_86b9_7b6e,
        0xc151_fd2f_1d93_7d1c,
        0x414f_ec69_19d0_8b4d,
        0x4091_ffeb_b490_7e6c,
        0x4092_0000_0000_0000,
    ];
    const ORACLE_PIXELS_PER_METER: u64 = 0x3fc5_85c7_86b9_7b6e;
    const ORACLE_CENTER_ZOOM0: [u64; 2] = [0x406f_fafe_6838_6f0c, 0x4065_4846_0db7_9819];

    fn probe() -> ViewTransform {
        ViewTransform {
            longitude: -0.11,
            latitude: 51.505,
            zoom: 13.0,
            width: 1024.0,
            height: 768.0,
            bearing: 0.0,
            pitch: 0.0,
        }
    }

    /// Distance between two f64 in ULPs, for stating a divergence as a bound rather than a
    /// tolerance. A relative epsilon would pass a matrix that was wrong in a way that happened
    /// to be small; counting representable values between them does not.
    fn ulps_apart(a: f64, b: f64) -> u64 {
        if a == b {
            return 0;
        }
        let ordered = |x: f64| {
            let bits = x.to_bits();
            if bits & (1 << 63) != 0 {
                !bits
            } else {
                bits | (1 << 63)
            }
        };
        ordered(a).abs_diff(ordered(b))
    }

    /// Fourteen of sixteen elements reproduce the oracle bit for bit, and the other two are
    /// within one ULP. Both halves are asserted: loosening the whole matrix to a tolerance would
    /// stop the fourteen from being evidence of anything.
    #[test]
    fn the_projection_matches_the_oracle() {
        let matrix = proj_matrix(&probe()).expect("an unrotated camera");
        let mut exact = 0;
        for (index, &got) in matrix.iter().enumerate() {
            let want = f64::from_bits(ORACLE_PROJ[index]);
            match ulps_apart(got, want) {
                0 => exact += 1,
                distance => {
                    assert!(
                        OneUlpDivergence::KNOWN.indices.contains(&index),
                        "element {index} diverges and is not a known divergence: {got:?} vs {want:?}"
                    );
                    assert_eq!(distance, 1, "element {index} is {distance} ULPs out");
                }
            }
        }
        assert_eq!(exact, 14, "fourteen elements bit-exact");
    }

    /// The known divergence is exactly where it is documented to be, so it cannot quietly
    /// migrate to another element and still pass.
    #[test]
    fn the_divergence_is_only_the_translation() {
        let matrix = proj_matrix(&probe()).expect("an unrotated camera");
        for index in OneUlpDivergence::KNOWN.indices {
            let want = f64::from_bits(ORACLE_PROJ[index]);
            assert_eq!(ulps_apart(matrix[index], want), 1, "element {index}");
        }
        // And the scale factors that multiply them are exact, which is what places the
        // difference in the translation rather than in the projection.
        for index in [0, 5] {
            assert_eq!(
                matrix[index].to_bits(),
                ORACLE_PROJ[index],
                "element {index}"
            );
        }
    }

    /// `pixelsPerMeter` is bit-exact, and is exactly the negation of matrix element 11.
    #[test]
    fn pixels_per_meter_matches_and_is_element_eleven_negated() {
        let view = probe();
        let ppm = pixels_per_meter(&view);
        assert_eq!(ppm.to_bits(), ORACLE_PIXELS_PER_METER);

        let matrix = proj_matrix(&view).expect("an unrotated camera");
        assert_eq!(matrix[11], -ppm, "the z column carries it negated");
    }

    /// The scale-free center is bit-exact on both axes.
    #[test]
    fn the_scale_free_center_matches_the_oracle() {
        let center = center_zoom0(&probe());
        assert_eq!(center[0].to_bits(), ORACLE_CENTER_ZOOM0[0]);
        assert_eq!(center[1].to_bits(), ORACLE_CENTER_ZOOM0[1]);
    }

    /// The camera distance is exactly 1.5 times the height, because the f64 field of view makes
    /// `tan(fov / 2)` exactly one third. The f32 one does not, and using it here would move the
    /// far plane and the matrix with it.
    #[test]
    fn the_camera_distance_uses_the_f64_field_of_view() {
        assert_eq!(camera_to_center_distance(768.0), 1152.0);
        assert_eq!(camera_to_center_distance(1080.0), 1620.0);

        #[allow(clippy::cast_possible_truncation)]
        let as_f32 = f64::from(DEFAULT_FOV as f32);
        assert_ne!(
            0.5 * 768.0 / (as_f32 / 2.0).tan(),
            1152.0,
            "the f32 field of view is a different number, which is why the split matters"
        );
    }

    /// The projection reads the f32 field of view, which is what makes element 5 not exactly -3.
    #[test]
    fn the_projection_uses_the_f32_field_of_view() {
        let matrix = proj_matrix(&probe()).expect("an unrotated camera");
        assert_ne!(matrix[5], -3.0, "the oracle's is -3.0000000293447");
        assert_eq!(matrix[5].to_bits(), ORACLE_PROJ[5]);

        // Had the f64 field of view been used throughout, element 5 would be exactly -3.
        let exact = perspective(DEFAULT_FOV, 1024.0 / 768.0, 1.0, 1163.52);
        assert_eq!(exact[5], 3.0);
    }

    /// The center offset is a signed offset from the origin, not a pixel position — mbgl
    /// recovers longitude as `-x / Bc`, and a pixel position would put the prime meridian in the
    /// wrong place by half a world.
    #[test]
    fn the_center_offset_is_signed_from_the_origin() {
        let [x, _] = center_offset(-0.11, 51.505, 13.0);
        assert!(x > 0.0, "a negative longitude gives a positive offset: {x}");
        let [x0, y0] = center_offset(0.0, 0.0, 13.0);
        assert_eq!([x0, y0], [0.0, 0.0], "the origin is the origin");
    }

    /// The atanh form is not interchangeable with the tangent form at this precision, which is
    /// why the port copies mbgl's expression rather than the identity it stands for.
    ///
    /// They happen to agree at the probe's own latitude, so checking only there would conclude
    /// the opposite. Over a sweep of the usable range they disagree in the last bit at most
    /// latitudes, which is what makes copying the expression necessary rather than pedantic.
    #[test]
    fn the_atanh_form_is_not_the_tangent_form() {
        let cc = world_size(13.0) / core::f64::consts::TAU;
        let tangent_form = |latitude: f64| {
            let radians = latitude * core::f64::consts::PI / 180.0;
            cc * (core::f64::consts::FRAC_PI_4 + radians / 2.0).tan().ln()
        };

        // The probe's latitude is one of the agreeing ones.
        assert_eq!(
            center_offset(0.0, 51.505, 13.0)[1].to_bits(),
            tangent_form(51.505).to_bits(),
            "which is why a single-latitude check would miss this"
        );

        let mut differing = 0;
        let mut compared = 0;
        let mut hundredths = -8500;
        while hundredths < 8500 {
            let latitude = f64::from(hundredths) / 100.0;
            let atanh = center_offset(0.0, latitude, 13.0)[1];
            let tangent = tangent_form(latitude);
            assert!(
                (atanh - tangent).abs() < 1e-6,
                "the same real number at {latitude}: {atanh} vs {tangent}"
            );
            compared += 1;
            if atanh.to_bits() != tangent.to_bits() {
                differing += 1;
            }
            hundredths += 7;
        }
        assert!(
            differing * 2 > compared,
            "most latitudes disagree in the last bit: {differing} of {compared}"
        );
    }

    /// A rotated or pitched camera is refused rather than guessed at.
    #[test]
    fn rotation_and_pitch_are_refused() {
        for view in [
            ViewTransform {
                bearing: 45.0,
                ..probe()
            },
            ViewTransform {
                pitch: 30.0,
                ..probe()
            },
        ] {
            assert_eq!(proj_matrix(&view), Err(CameraError::Rotated));
        }
    }

    /// Multiplication reproduces mbgl's accumulation, checked against identity and a known
    /// product rather than only against the oracle's one matrix.
    #[test]
    fn multiply_composes_in_the_expected_order() {
        let identity: Mat4 = [
            1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
        ];
        let mut translated = identity;
        translate_in_place(&mut translated, 3.0, 5.0, 7.0);
        assert_eq!(multiply(&identity, &translated), translated);
        assert_eq!(multiply(&translated, &identity), translated);
        assert_eq!(&translated[12..15], &[3.0, 5.0, 7.0]);
    }
}
