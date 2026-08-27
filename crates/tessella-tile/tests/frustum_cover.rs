//! The frustum cover, which is what a pitched view needs instead of a bounding rectangle.
//!
//! # What can be checked without an oracle
//!
//! The golden dump is captured unpitched, so a pitched cover cannot be diffed against mbgl's.
//! What can be checked is the geometry itself: a frustum is a convex volume, a tile is a box,
//! and whether one crosses the other is a question with an answer that does not depend on mbgl.
//! So the tests below sample the ground the view actually projects and assert the cover contains
//! every tile a sample landed in — an independent computation of the same set, from the
//! projection rather than from the traversal.

use tessella_tile::camera;
use tessella_tile::cover::{self, ViewTransform};
use tessella_tile::frustum::{Aabb, Frustum, Intersection};

fn view(pitch: f64, bearing: f64) -> ViewTransform {
    camera::settled(&ViewTransform {
        longitude: 13.4,
        latitude: 52.5,
        zoom: 14.0,
        width: 900.0,
        height: 700.0,
        bearing,
        pitch,
    })
}

/// Every tile the view's own projection lands in is in the cover.
///
/// The independent check. Rather than trusting the traversal, this walks the *screen* — a grid
/// of pixels — pushes each back onto the ground through the inverse projection, and asks which
/// tile it fell in. A cover that missed any of them would be a hole in the middle of the map.
#[test]
fn the_cover_contains_every_tile_the_screen_lands_in() {
    for pitch in [0.0, 30.0, 50.0] {
        let view = view(pitch, 0.0);
        let tiles = cover::cover(&view).expect("a cover");
        let z = view.tile_zoom();
        let scale = f64::from(1u32 << z);

        let projection = camera::proj_matrix(&view).expect("a matrix");
        let inverse = camera::invert(&projection).expect("an inverse");

        // Unproject a screen point onto z = 0 by intersecting the ray through it with the plane.
        let ground = |sx: f64, sy: f64| -> Option<[f64; 2]> {
            let ndc = |z: f64| {
                let clip = [sx * 2.0 - 1.0, 1.0 - sy * 2.0, z, 1.0];
                let mut out = [0.0f64; 4];
                for row in 0..4 {
                    out[row] = inverse[row] * clip[0]
                        + inverse[4 + row] * clip[1]
                        + inverse[8 + row] * clip[2]
                        + inverse[12 + row] * clip[3];
                }
                (out[3] != 0.0).then(|| [out[0] / out[3], out[1] / out[3], out[2] / out[3]])
            };
            let near = ndc(-1.0)?;
            let far = ndc(1.0)?;
            let dz = far[2] - near[2];
            if dz.abs() < 1e-12 {
                return None;
            }
            let t = -near[2] / dz;
            // Only forward along the ray; behind the camera is not visible ground.
            if !(0.0..=1.0).contains(&t) {
                return None;
            }
            let world = camera::world_size(view.zoom);
            Some([
                (near[0] + (far[0] - near[0]) * t) / world * scale,
                (near[1] + (far[1] - near[1]) * t) / world * scale,
            ])
        };

        let mut sampled = 0;
        for row in 0..=20 {
            for column in 0..=20 {
                let Some(point) = ground(f64::from(column) / 20.0, f64::from(row) / 20.0) else {
                    continue;
                };
                #[allow(clippy::cast_possible_truncation)]
                let (tx, ty) = (point[0].floor() as i64, point[1].floor() as i64);
                if ty < 0 || ty >= scale as i64 {
                    continue;
                }
                let wrap = (point[0] / scale).floor();
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let wrapped = (tx - (wrap as i64) * (scale as i64)) as u32;
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let found = tiles
                    .iter()
                    .any(|t| t.x == wrapped && t.y == ty as u32 && t.wrap == wrap as i32);
                assert!(
                    found,
                    "pitch {pitch}: screen ({column}, {row}) lands in tile \
                     {wrapped}/{ty} wrap {wrap}, which the cover does not have"
                );
                sampled += 1;
            }
        }
        assert!(
            sampled > 100,
            "pitch {pitch}: only {sampled} samples hit ground"
        );
    }
}

/// Nearest to the centre first, because that is the order tiles are wanted in.
#[test]
fn the_cover_is_ordered_from_the_centre_out() {
    let view = view(55.0, 0.0);
    let tiles = cover::cover(&view).expect("a cover");
    let z = view.tile_zoom();
    let centre = tessella_tile::projection::tile_units(view.longitude, view.latitude, z);
    let scale = f64::from(1u32 << z);

    let distance = |t: &cover::TileCoord| {
        let dx = f64::from(t.wrap) * scale + f64::from(t.x) + 0.5 - centre[0];
        let dy = f64::from(t.y) + 0.5 - centre[1];
        dx * dx + dy * dy
    };
    let mut previous = f64::NEG_INFINITY;
    for tile in &tiles {
        let d = distance(tile);
        assert!(
            d >= previous - 1e-9,
            "{tile:?} at {d} came after {previous}"
        );
        previous = d;
    }
}

/// A box wholly inside answers `Contains`, which is what lets a subtree skip the test entirely.
#[test]
fn a_contained_box_short_circuits_its_subtree() {
    let view = view(0.0, 0.0);
    let projection = camera::proj_matrix(&view).expect("a matrix");
    let z = view.tile_zoom();
    let frustum =
        Frustum::from_projection(&projection, camera::world_size(view.zoom), f64::from(z))
            .expect("a frustum");

    let centre = tessella_tile::projection::tile_units(view.longitude, view.latitude, z);
    // A sliver at the very centre of the screen is inside on every plane.
    let tiny = Aabb {
        min: [centre[0] - 0.01, centre[1] - 0.01, 0.0],
        max: [centre[0] + 0.01, centre[1] + 0.01, 0.0],
    };
    assert_eq!(frustum.intersects(&tiny), Intersection::Contains);

    // And something on the far side of the world is not.
    let elsewhere = Aabb {
        min: [centre[0] + 5000.0, centre[1], 0.0],
        max: [centre[0] + 5001.0, centre[1] + 1.0, 0.0],
    };
    assert_eq!(frustum.intersects(&elsewhere), Intersection::Separate);
}

/// A quadrant's index and its tile coordinate agree, which the traversal relies on.
#[test]
fn quadrant_indices_match_tile_coordinates() {
    let unit = Aabb {
        min: [0.0, 0.0, 0.0],
        max: [2.0, 2.0, 0.0],
    };
    // `x` from the low bit, `y` from the high — the order `childrenOf` numbers children in.
    assert_eq!(unit.quadrant(0).min, [0.0, 0.0, 0.0]);
    assert_eq!(unit.quadrant(1).min, [1.0, 0.0, 0.0]);
    assert_eq!(unit.quadrant(2).min, [0.0, 1.0, 0.0]);
    assert_eq!(unit.quadrant(3).min, [1.0, 1.0, 0.0]);
    for index in 0..4 {
        let child = unit.quadrant(index);
        assert_eq!(child.max[0] - child.min[0], 1.0);
        assert_eq!(child.max[1] - child.min[1], 1.0);
    }
}
