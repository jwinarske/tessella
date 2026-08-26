//! The plane a label is laid out in, and the trip back to clip space.
//!
//! Two branches, chosen by `text-pitch-alignment`, and they are different *kinds* of matrix. A
//! viewport-aligned label is laid out in screen pixels, so its plane is a projection and carries
//! the tile matrix. A map-aligned one lies flat on the ground and is laid out in tile units, so
//! its plane is a scale and carries no camera at all — the tile matrix already places it.
//!
//! Confusing them draws a pitched map's labels flat when they should stand up, or standing when
//! they should lie flat. Both look like a shader problem.

use tessella_tile::camera::{
    gl_coord_matrix, gl_coord_matrix_on_map, identity, label_plane_matrix,
    label_plane_matrix_on_map, multiply, pixels_to_tile_units, rotate_z,
};

/// A z13 tile viewed at z13, so a pixel is sixteen tile units.
const Z: u8 = 13;
const ZOOM: f64 = 13.0;

/// Rounding slack. These are `f64` matrices built from `f32` ratios, so exact equality is only
/// available where a term is structurally zero.
const SLACK: f64 = 1e-9;

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < SLACK
}

/// A pixel is the extent over the tile's screen size.
#[test]
fn a_pixel_is_sixteen_tile_units_at_a_tiles_own_zoom() {
    assert!(
        close(pixels_to_tile_units(Z, ZOOM), 16.0),
        "{}",
        pixels_to_tile_units(Z, ZOOM)
    );
    // One zoom in halves it: the tile covers twice the screen, so a pixel is fewer tile units.
    assert!(close(pixels_to_tile_units(Z, ZOOM + 1.0), 8.0));
    assert!(close(pixels_to_tile_units(Z, ZOOM - 1.0), 32.0));
}

/// The map-aligned plane is a scale, with no camera in it.
///
/// The whole distinction: a label lying flat is measured in tile units and the tile matrix
/// already places it, so the plane converts units and nothing more. A version that folded the
/// projection in would place it twice.
#[test]
fn the_map_aligned_plane_is_a_scale() {
    let plane = label_plane_matrix_on_map(Z, ZOOM, 0.0, true);

    // One over sixteen on both axes, z untouched, no translation.
    assert!(close(plane[0], 1.0 / 16.0), "{plane:?}");
    assert!(close(plane[5], 1.0 / 16.0), "{plane:?}");
    assert!(close(plane[10], 1.0));
    assert_eq!(
        [plane[12], plane[13], plane[14], plane[15]],
        [0.0, 0.0, 0.0, 1.0]
    );
}

/// It undoes the bearing when the label does not rotate with the map.
///
/// `text-pitch-alignment` and `text-rotation-alignment` are separate properties because a label
/// can lie flat *and* stay upright — a road name on a tilted map is exactly that. When it does
/// rotate with the map there is nothing to undo, so the two agree at a bearing of zero and part
/// company at any other.
#[test]
fn the_bearing_is_undone_only_when_the_label_does_not_turn_with_the_map() {
    let quarter = core::f64::consts::FRAC_PI_2;

    let turning = label_plane_matrix_on_map(Z, ZOOM, quarter, true);
    let upright = label_plane_matrix_on_map(Z, ZOOM, quarter, false);
    assert_ne!(turning, upright, "the bearing did not reach the plane");

    // At no bearing the rotation is the identity, so the two are the same matrix.
    let a = label_plane_matrix_on_map(Z, ZOOM, 0.0, true);
    let b = label_plane_matrix_on_map(Z, ZOOM, 0.0, false);
    for (left, right) in a.iter().zip(&b) {
        assert!(close(*left, *right), "{a:?} against {b:?}");
    }
}

/// The map-aligned pair are inverses of each other, through the tile.
///
/// The property that says the two halves belong together: a point taken into the label plane and
/// back must land where it started. Getting one of the two rotations' signs wrong satisfies every
/// structural check above and fails this.
#[test]
fn the_map_aligned_pair_round_trips() {
    let quarter = core::f64::consts::FRAC_PI_2;
    // A stand-in for a tile matrix. Any invertible one does: what is being checked is that the
    // plane and the coordinate matrix undo each other, not what the tile matrix is.
    let tile = [
        0.5, 0.0, 0.0, 0.0, //
        0.0, 0.25, 0.0, 0.0, //
        0.0, 0.0, 1.0, 0.0, //
        3.0, -7.0, 0.0, 1.0,
    ];

    for rotate_with_map in [true, false] {
        let plane = label_plane_matrix_on_map(Z, ZOOM, quarter, rotate_with_map);
        let coord = gl_coord_matrix_on_map(&tile, Z, ZOOM, quarter, rotate_with_map);

        // `coord * plane` should be the tile matrix: the plane converts to label units and the
        // coordinate matrix converts back and applies the tile.
        let round_trip = multiply(&coord, &plane);
        for (index, (left, right)) in round_trip.iter().zip(&tile).enumerate() {
            assert!(
                close(*left, *right),
                "rotate_with_map={rotate_with_map}: element {index} is {left}, not {right}"
            );
        }
    }
}

/// The viewport-aligned pair are unchanged, and unrelated to the tile.
///
/// Pinned because the map-aligned branch was added beside them: the viewport path is what every
/// golden checks, and a change that reached it would be a regression the capture would catch —
/// but only for the styles a capture exists for.
#[test]
fn the_viewport_aligned_pair_are_the_viewports_alone() {
    let tile = identity();
    let plane = label_plane_matrix(&tile, 1024.0, 768.0);
    assert!(close(plane[0], 512.0), "{plane:?}");
    assert!(close(plane[5], -384.0), "{plane:?}");

    let coord = gl_coord_matrix(1024.0, 768.0);
    assert!(close(coord[0], 2.0 / 1024.0));
    assert!(close(coord[5], -2.0 / 768.0));
    assert_eq!([coord[12], coord[13], coord[15]], [-1.0, 1.0, 1.0]);
}

/// A rotation about z leaves the z axis and the translation alone.
#[test]
fn rotating_about_z_touches_only_two_columns() {
    let start = [
        1.0, 2.0, 3.0, 4.0, //
        5.0, 6.0, 7.0, 8.0, //
        9.0, 10.0, 11.0, 12.0, //
        13.0, 14.0, 15.0, 16.0,
    ];
    let turned = rotate_z(&start, core::f64::consts::FRAC_PI_3);
    assert_eq!(&turned[8..], &start[8..], "z and translation moved");
    assert_ne!(&turned[..8], &start[..8], "nothing turned");

    // A full turn is the identity, to rounding.
    let full = rotate_z(&start, core::f64::consts::TAU);
    for (left, right) in full.iter().zip(&start) {
        assert!((left - right).abs() < 1e-12, "{full:?}");
    }
}
