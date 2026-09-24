// SPDX-License-Identifier: BSD-2-Clause
//! The Douglas-Peucker annotation, against the threshold the oracle was measured at.

use tessella_source::simplify::{
    DEFAULT_MAX_ZOOM, DEFAULT_TOLERANCE, keeps, line, ring, tolerance_at,
};

/// One tile unit of longitude at `zoom`.
fn unit(zoom: u8) -> f64 {
    (360.0 / f64::from(1u32 << zoom)) / 8192.0
}

/// A three-point line whose middle point is `deviation` tile units off the straight run.
fn kinked(zoom: u8, deviation: f64) -> Vec<[f64; 2]> {
    let (lon, lat) = (-0.110, 51.505);
    vec![
        [lon, lat - 0.004],
        [lon + deviation * unit(zoom), lat],
        [lon, lat + 0.004],
    ]
}

/// The threshold is six tile units, and the same six at any zoom.
///
/// Measured against `mbgl-capture-probe` before it was implemented: a three-point line at two
/// zooms kept its middle point from seven units and dropped it at six, identically.
///
/// ```text
/// z13   d=4:v4  d=5:v4  d=6:v4  d=7:v6  d=8:v6
/// z15   d=4:v4  d=5:v4  d=6:v4  d=7:v6  d=8:v6
/// ```
#[test]
fn the_threshold_is_six_tile_units_at_every_zoom() {
    for zoom in [13u8, 15, 8] {
        let sq_tolerance = tolerance_at(zoom, DEFAULT_TOLERANCE).powi(2);
        for deviation in [4.0, 5.0, 6.0] {
            let simplified = line(
                &kinked(zoom, deviation),
                DEFAULT_MAX_ZOOM,
                DEFAULT_TOLERANCE,
            );
            assert!(
                !keeps(simplified.importance[1], sq_tolerance),
                "z{zoom} deviation {deviation} should be dropped: {} against {sq_tolerance}",
                simplified.importance[1]
            );
        }
        for deviation in [7.0, 8.0, 20.0] {
            let simplified = line(
                &kinked(zoom, deviation),
                DEFAULT_MAX_ZOOM,
                DEFAULT_TOLERANCE,
            );
            assert!(
                keeps(simplified.importance[1], sq_tolerance),
                "z{zoom} deviation {deviation} should survive: {} against {sq_tolerance}",
                simplified.importance[1]
            );
        }
    }
}

/// Endpoints always survive, which is what their importance of one is for.
#[test]
fn endpoints_are_never_dropped() {
    let simplified = line(&kinked(13, 0.0), DEFAULT_MAX_ZOOM, DEFAULT_TOLERANCE);
    let sq_tolerance = tolerance_at(13, DEFAULT_TOLERANCE).powi(2);
    assert!(keeps(simplified.importance[0], sq_tolerance));
    assert!(keeps(*simplified.importance.last().unwrap(), sq_tolerance));
    // And the dead-straight middle point is not.
    assert!(!keeps(simplified.importance[1], sq_tolerance));
}

/// A line carries its projected length and a ring its absolute projected area.
///
/// mbgl drops a whole line shorter than the tolerance and a whole ring smaller than its square,
/// so these are the two whole-feature filters and not decoration.
#[test]
fn a_line_carries_its_length_and_a_ring_its_area() {
    let straight = line(
        &[[-0.110, 51.505], [-0.100, 51.505]],
        DEFAULT_MAX_ZOOM,
        DEFAULT_TOLERANCE,
    );
    // 0.01 degrees of longitude is 0.01/360 of the projected world.
    assert!(
        (straight.extent - 0.01 / 360.0).abs() < 1e-9,
        "a line's extent is its projected length: {}",
        straight.extent
    );

    let square = ring(
        &[
            [-0.110, 51.505],
            [-0.100, 51.505],
            [-0.100, 51.515],
            [-0.110, 51.515],
            [-0.110, 51.505],
        ],
        DEFAULT_MAX_ZOOM,
        DEFAULT_TOLERANCE,
    );
    assert!(
        square.extent > 0.0,
        "a ring's extent is its absolute area: {}",
        square.extent
    );
    // Unsigned: the same ring the other way round has the same area. Compared relatively, because
    // a shoelace over projected coordinates subtracts terms of order 0.17 to reach 1.2e-9 and
    // loses about eight digits doing it -- mbgl's loses the same ones.
    let reversed = vec![
        [-0.110, 51.505],
        [-0.110, 51.515],
        [-0.100, 51.515],
        [-0.100, 51.505],
        [-0.110, 51.505],
    ];
    let other = ring(&reversed, DEFAULT_MAX_ZOOM, DEFAULT_TOLERANCE);
    assert!(
        (square.extent - other.extent).abs() / square.extent < 1e-6,
        "area is unsigned: {} against {}",
        square.extent,
        other.extent
    );
}

/// A degenerate ring or line annotates without panicking.
#[test]
fn degenerate_input_survives() {
    for points in [vec![], vec![[0.0, 0.0]], vec![[0.0, 0.0], [0.0, 0.0]]] {
        let simplified = line(&points, DEFAULT_MAX_ZOOM, DEFAULT_TOLERANCE);
        assert_eq!(simplified.importance.len(), points.len());
        let simplified = ring(&points, DEFAULT_MAX_ZOOM, DEFAULT_TOLERANCE);
        assert_eq!(simplified.importance.len(), points.len());
    }
}
