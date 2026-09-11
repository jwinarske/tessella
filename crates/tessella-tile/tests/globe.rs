//! The globe projection, checked against identities rather than an oracle.
//!
//! MapLibre Native has no globe, so there is nothing to render against. What can be pinned is what
//! the geometry has to be true of regardless of implementation: the poles, the round trip, the
//! antimeridian, and the horizon's tangent.

use tessella_tile::camera;
use tessella_tile::cover;
use tessella_tile::globe;

/// Everything the projection produces is on the unit sphere.
#[test]
fn every_point_is_on_the_sphere() {
    for &longitude in &[-180.0, -90.0, -1.0, 0.0, 1.0, 90.0, 179.9] {
        for &latitude in &[-85.0, -45.0, 0.0, 45.0, 85.0] {
            let p = globe::sphere_point(longitude, latitude);
            let length = globe::dot(p, p).sqrt();
            assert!(
                (length - 1.0).abs() < 1e-12,
                "({longitude}, {latitude}) has length {length}"
            );
        }
    }
}

/// The origin faces `+z`, the poles are on `∓y`, and east is `+x`.
///
/// GL JS's convention, and the reason `y` is negated: it is the screen's axis, not the Earth's.
#[test]
fn the_axes_are_where_gl_js_puts_them() {
    let origin = globe::sphere_point(0.0, 0.0);
    assert!((origin[0]).abs() < 1e-12 && (origin[1]).abs() < 1e-12);
    assert!((origin[2] - 1.0).abs() < 1e-12, "(0,0) is +z: {origin:?}");

    let north = globe::sphere_point(0.0, 90.0);
    assert!(
        (north[1] + 1.0).abs() < 1e-12,
        "the north pole is -y: {north:?}"
    );
    let south = globe::sphere_point(0.0, -90.0);
    assert!(
        (south[1] - 1.0).abs() < 1e-12,
        "the south pole is +y: {south:?}"
    );

    let east = globe::sphere_point(90.0, 0.0);
    assert!((east[0] - 1.0).abs() < 1e-12, "90°E is +x: {east:?}");
}

/// A pole is one point however you spell its longitude.
#[test]
fn the_pole_is_one_place() {
    let a = globe::sphere_point(0.0, 90.0);
    for &longitude in &[-180.0, -37.0, 55.0, 180.0] {
        let b = globe::sphere_point(longitude, 90.0);
        for axis in 0..3 {
            assert!(
                (a[axis] - b[axis]).abs() < 1e-12,
                "the north pole moved at {longitude}: {a:?} against {b:?}"
            );
        }
    }
}

/// The antimeridian's two spellings are the same point.
#[test]
fn the_antimeridian_meets_itself() {
    let west = globe::sphere_point(-180.0, 20.0);
    let east = globe::sphere_point(180.0, 20.0);
    for axis in 0..3 {
        assert!(
            (west[axis] - east[axis]).abs() < 1e-12,
            "-180 and 180 differ: {west:?} against {east:?}"
        );
    }
}

/// Going through Mercator and back returns the latitude it started from.
///
/// The bend takes tile geometry, which is Mercator, so this is the composition that actually runs.
#[test]
fn mercator_round_trips_through_the_sphere() {
    for &latitude in &[-80.0, -45.0, -0.5, 0.0, 0.5, 45.0, 80.0] {
        let y = camera::mercator_fraction(latitude);
        let direct = globe::sphere_point(30.0, latitude);
        let via_mercator = globe::sphere_point_from_mercator((30.0 + 180.0) / 360.0, y);
        for axis in 0..3 {
            assert!(
                (direct[axis] - via_mercator[axis]).abs() < 1e-9,
                "latitude {latitude} bent differently through Mercator: \
                 {direct:?} against {via_mercator:?}"
            );
        }
    }
}

/// Mercator has no poles, so a tile edge past its limit clamps rather than diverging.
#[test]
fn past_the_mercator_limit_clamps_to_the_pole() {
    let over = globe::sphere_point_from_mercator(0.5, -0.25);
    let north = globe::sphere_point(0.0, camera::latitude_of(0.0));
    for axis in 0..3 {
        assert!(
            (over[axis] - north[axis]).abs() < 1e-12,
            "a vertex above the world did not clamp: {over:?} against {north:?}"
        );
    }
}

/// A point exactly on the horizon is exactly on the horizon.
///
/// For a unit sphere seen from `d` radii out, the tangent grazes at `cos θ = 1/d`. So a camera two
/// radii out sees exactly the cap within sixty degrees of the point beneath it.
#[test]
fn the_horizon_is_where_the_tangent_grazes() {
    let toward = [0.0, 0.0, 1.0];
    let distance = 2.0;

    let just_inside = globe::sphere_point(59.9, 0.0);
    let just_outside = globe::sphere_point(60.1, 0.0);
    assert!(
        globe::faces_camera(just_inside, toward, distance),
        "59.9° from the sub-camera point is visible at two radii"
    );
    assert!(
        !globe::faces_camera(just_outside, toward, distance),
        "60.1° is not"
    );

    // And the further out the camera, the more of the sphere it sees.
    let far = globe::sphere_point(80.0, 0.0);
    assert!(!globe::faces_camera(far, toward, 2.0));
    assert!(
        globe::faces_camera(far, toward, 100.0),
        "a distant camera sees 80°"
    );
}

/// The camera is always outside the sphere, and closer as the zoom rises.
#[test]
fn the_camera_backs_off_as_the_zoom_falls() {
    let mut previous = f64::INFINITY;
    for zoom in 0..12 {
        let distance = globe::camera_distance(f64::from(zoom), 0.0, 700.0);
        assert!(
            distance > 1.0,
            "the camera is outside the surface at z{zoom}"
        );
        assert!(
            distance < previous,
            "z{zoom} is not nearer than the zoom before it: {distance} against {previous}"
        );
        previous = distance;
    }
}

/// A safe horizon cull removes nothing at tile granularity, and this pins that.
///
/// §13.4 recorded "a third to a half of the cover between z1 and z2.5", which the measurement below
/// reproduces to the tile -- but only for a test that asks whether a tile's *centre* is behind the
/// horizon. A z1 tile spans ninety degrees of longitude, so its centre goes behind while a third of
/// it is still on screen, and culling on that leaves a hole in the planet. Asked safely -- is any
/// part of the tile visible -- the answer is nothing, at every zoom.
///
/// So the cull belongs after the subdivision rather than before it, on patches small enough for the
/// question to have a useful answer. This test exists to keep the safe version honest: if it ever
/// starts culling whole tiles, either the sampling got coarser or the camera model moved.
#[test]
fn the_horizon_cuts_only_the_lowest_zooms() {
    use tessella_tile::cover::{self, ViewTransform, WorldCopies};

    for zoom in 0..8 {
        let view = ViewTransform {
            longitude: -122.3321,
            latitude: 47.6062,
            zoom: f64::from(zoom),
            width: 900.0,
            height: 700.0,
            bearing: 0.0,
            pitch: 0.0,
        };
        let view = camera::settled(&view);
        let tiles = cover::cover_with(&view, WorldCopies::One).expect("covers");
        let behind = tiles
            .iter()
            .filter(|tile| {
                !globe::tile_faces_camera(
                    tile.z,
                    tile.x,
                    tile.y,
                    view.longitude,
                    view.latitude,
                    view.zoom,
                    view.height,
                )
            })
            .count();
        assert_eq!(
            behind,
            0,
            "z{zoom} culled {behind} of {} tiles, and a conservative test should cull none: \
             every tile a Mercator cover produces has a corner in front of the horizon",
            tiles.len()
        );
    }
}

/// Prints what the cull removes, for the record rather than as an assertion.
#[test]
#[ignore = "a measurement, not a check"]
fn horizon_counts() {
    use tessella_tile::cover::{self, ViewTransform, WorldCopies};
    for tenth in 0..=40 {
        let zoom = f64::from(tenth) / 10.0;
        let view = camera::settled(&ViewTransform {
            longitude: -122.3321,
            latitude: 47.6062,
            zoom,
            width: 900.0,
            height: 700.0,
            bearing: 0.0,
            pitch: 0.0,
        });
        let tiles = cover::cover_with(&view, WorldCopies::One).expect("covers");
        let behind = tiles
            .iter()
            .filter(|t| {
                !globe::tile_faces_camera(
                    t.z,
                    t.x,
                    t.y,
                    view.longitude,
                    view.latitude,
                    view.zoom,
                    view.height,
                )
            })
            .count();
        // The same question asked three ways, because the answer depends entirely on which.
        let distance = globe::camera_distance(view.zoom, view.latitude, view.height);
        let toward = globe::sphere_point(view.longitude, view.latitude);
        let centre_behind = tiles
            .iter()
            .filter(|t| {
                let span = 1.0 / f64::from(1u32 << t.z);
                let p = globe::sphere_point_from_mercator(
                    (f64::from(t.x) + 0.5) * span,
                    (f64::from(t.y) + 0.5) * span,
                );
                !globe::faces_camera(p, toward, distance)
            })
            .count();
        if tenth % 5 == 0 {
            println!(
                "z{zoom:>4}  cover {:>3}  no-corner-visible {behind:>3}  centre-behind {centre_behind:>3}",
                tiles.len()
            );
        }
    }
}

/// The subdivision keeps the chord within the tolerance it was asked for.
///
/// The property the bound exists for, checked rather than trusted, across the zooms and tile levels
/// a globe actually draws.
#[test]
fn the_subdivision_holds_its_tolerance() {
    for &tolerance in &[0.25, 0.5, 1.0, 2.0] {
        for z in 0..8u8 {
            for zoom in 0..8 {
                let zoom = f64::from(zoom);
                let n = globe::edge_segments(z, zoom, tolerance);
                assert!(n >= 1, "z{z} at zoom {zoom} asked for no segments");
                if n < globe::MAX_EDGE_SEGMENTS {
                    let error = globe::chord_error(z, zoom, n);
                    assert!(
                        error <= tolerance,
                        "z{z} at zoom {zoom} split into {n} leaves {error} pixels, over {tolerance}"
                    );
                }
            }
        }
    }
}

/// And it is not wasteful about it: one segment fewer would break the tolerance.
///
/// Without this the bound is satisfied by subdividing everything to the ceiling, which is the
/// expensive way to be correct.
#[test]
fn the_subdivision_is_not_more_than_it_needs() {
    let tolerance = 0.5;
    for z in 0..8u8 {
        for zoom in 0..8 {
            let zoom = f64::from(zoom);
            let n = globe::edge_segments(z, zoom, tolerance);
            if n > 1 && n < globe::MAX_EDGE_SEGMENTS {
                let coarser = globe::chord_error(z, zoom, n - 1);
                assert!(
                    coarser > tolerance,
                    "z{z} at zoom {zoom} used {n} segments where {} would have held \
                     ({coarser} pixels)",
                    n - 1
                );
            }
        }
    }
}

/// A finer tile needs fewer segments than a coarser one at the same camera.
///
/// The whole reason the count is per tile rather than per frame: a z8 tile is a small patch of the
/// sphere and barely curves, a z0 tile is the whole of it.
#[test]
fn a_finer_tile_needs_less_subdivision() {
    let zoom = 3.0;
    let mut previous = u32::MAX;
    for z in 0..10u8 {
        let n = globe::edge_segments(z, zoom, 0.5);
        assert!(
            n <= previous,
            "z{z} wanted {n} segments where z{} wanted {previous}",
            z - 1
        );
        previous = n;
    }
    assert_eq!(previous, 1, "a small enough tile is a flat quad");
}

/// The ceiling holds however extreme the camera.
#[test]
fn the_subdivision_has_a_ceiling() {
    for zoom in [0.0, 8.0, 16.0, 22.0] {
        for &tolerance in &[0.001, 0.01] {
            let n = globe::edge_segments(0, zoom, tolerance);
            assert!(
                n <= globe::MAX_EDGE_SEGMENTS,
                "zoom {zoom} at {tolerance} px asked for {n} segments"
            );
        }
    }
}

/// What the subdivision costs, for the record.
#[test]
#[ignore = "a measurement, not a check"]
fn subdivision_counts() {
    println!("segments per tile edge, 0.5 px tolerance");
    println!("      z0    z1    z2    z3    z4    z5    z6    z8   z10");
    for zoom in [0.0, 1.0, 2.0, 3.0, 4.0, 6.0, 8.0, 12.0, 16.0] {
        print!("zoom {zoom:>4}");
        for z in [0u8, 1, 2, 3, 4, 5, 6, 8, 10] {
            print!("{:>6}", globe::edge_segments(z, zoom, 0.5));
        }
        println!();
    }
    println!("\nvertices for one tile (segments+1 squared), at its own zoom:");
    for z in 0..8u8 {
        let n = globe::edge_segments(z, f64::from(z), 0.5);
        println!("  z{z}: {n} segments -> {} vertices", (n + 1) * (n + 1));
    }
}

fn globe_view(longitude: f64, latitude: f64, zoom: f64) -> tessella_tile::cover::ViewTransform {
    tessella_tile::cover::ViewTransform {
        longitude,
        latitude,
        zoom,
        width: 900.0,
        height: 700.0,
        bearing: 0.0,
        pitch: 0.0,
    }
}

/// The point under the camera lands in the middle of the screen.
///
/// The one thing a view matrix has to get right, and the check that catches a sign error in either
/// rotation: get the latitude's sign wrong and the centre moves off in y, the longitude's and it
/// moves off in x.
#[test]
fn the_camera_looks_at_the_point_it_is_over() {
    for &(longitude, latitude) in &[
        (0.0, 0.0),
        (-122.3321, 47.6062),
        (139.7, 35.7),
        (0.0, -33.9),
        (179.0, 0.0),
    ] {
        for zoom in [0.0, 2.0, 5.0] {
            let view = globe_view(longitude, latitude, zoom);
            let matrix = globe::clip_matrix(&view);
            let under = globe::sphere_point(longitude, latitude);
            let ndc = globe::project_point(&matrix, under)
                .expect("the point under the camera is in front of it");
            assert!(
                ndc[0].abs() < 1e-9 && ndc[1].abs() < 1e-9,
                "({longitude}, {latitude}) at zoom {zoom} landed at {ndc:?}, not the centre"
            );
        }
    }
}

/// North is up.
///
/// A point a little further north than the camera lands above the centre, which pins the sign of
/// the latitude rotation against `sphere_point`'s downward `y`.
#[test]
fn north_is_up() {
    let view = globe_view(0.0, 10.0, 3.0);
    let matrix = globe::clip_matrix(&view);
    let north = globe::project_point(&matrix, globe::sphere_point(0.0, 20.0)).expect("in front");
    let south = globe::project_point(&matrix, globe::sphere_point(0.0, 0.0)).expect("in front");
    assert!(
        north[1] > 0.0,
        "ten degrees north of the camera landed at y={}, which is not up",
        north[1]
    );
    assert!(south[1] < 0.0, "the equator landed at y={}", south[1]);
}

/// East is right.
#[test]
fn east_is_right() {
    let view = globe_view(0.0, 0.0, 3.0);
    let matrix = globe::clip_matrix(&view);
    let east = globe::project_point(&matrix, globe::sphere_point(10.0, 0.0)).expect("in front");
    assert!(east[0] > 0.0, "ten degrees east landed at x={}", east[0]);
}

/// The far side of the planet projects *in front* of the camera, and only the horizon knows.
///
/// Written first as "the antipode is behind the camera" and that is false: it sits on the view axis
/// inside the frustum, so the matrix puts it at the centre of the screen, further away in depth.
/// A projection cannot express occlusion. That is the whole reason `faces_camera` exists and why a
/// globe needs either it or a depth test -- and the assertion is the pair of them agreeing, not
/// the matrix doing something it cannot.
#[test]
fn the_antipode_projects_in_front_but_faces_away() {
    let view = globe_view(-122.3321, 47.6062, 1.0);
    let matrix = globe::clip_matrix(&view);
    let toward = globe::sphere_point(view.longitude, view.latitude);
    let distance = globe::camera_distance(view.zoom, view.latitude, view.height);

    let near = globe::sphere_point(view.longitude, view.latitude);
    let far = globe::sphere_point(view.longitude + 180.0, -view.latitude);

    let near_ndc = globe::project_point(&matrix, near).expect("the near side is in front");
    let far_ndc = globe::project_point(&matrix, far).expect("so is the far side, being occluded");
    assert!(
        far_ndc[2] > near_ndc[2],
        "the far side is not further away in depth: {} against {}",
        far_ndc[2],
        near_ndc[2]
    );
    assert!(globe::faces_camera(near, toward, distance));
    assert!(
        !globe::faces_camera(far, toward, distance),
        "the horizon test kept a point on the far side of the planet"
    );
}

/// Everything the horizon test keeps is on screen, and the sphere fits.
///
/// The two halves have to agree: a tile `faces_camera` accepts must actually project somewhere a
/// frame can draw, or the cull and the matrix are describing different cameras.
#[test]
fn what_faces_the_camera_projects_in_front_of_it() {
    for zoom in [0.0, 1.0, 3.0] {
        let view = globe_view(-122.3321, 47.6062, zoom);
        let matrix = globe::clip_matrix(&view);
        let toward = globe::sphere_point(view.longitude, view.latitude);
        let distance = globe::camera_distance(view.zoom, view.latitude, view.height);
        for lon in (-180..180).step_by(15) {
            for lat in (-80..81).step_by(20) {
                let p = globe::sphere_point(f64::from(lon), f64::from(lat));
                if globe::faces_camera(p, toward, distance) {
                    assert!(
                        globe::project_point(&matrix, p).is_some(),
                        "({lon}, {lat}) faces the camera at zoom {zoom} but projects behind it"
                    );
                }
            }
        }
    }
}

/// A zoom past the shift's width must not panic, and must not silently mean "one tile".
///
/// `1u32 << z` panics in debug for `z >= 32` and shifts by `z % 32` in release, where `z = 32`
/// answers one tile across and the caller reads a tile covering the whole world. Every entry point
/// clamps to `MAX_ZOOM`.
#[test]
fn an_impossible_zoom_is_clamped_not_wrapped() {
    for z in [0u8, 22, 30, 31, 32, 64, 255] {
        let n = globe::edge_segments(z, 4.0, 0.5);
        let e = globe::chord_error(z, 4.0, 4);
        let f = globe::tile_faces_camera(z, 0, 0, 0.0, 0.0, 4.0, 700.0);
        let _ = f;
        assert!(
            (1..=globe::MAX_EDGE_SEGMENTS).contains(&n),
            "z{z} asked for {n} segments"
        );
        assert!(e.is_finite(), "z{z} has a chord error of {e}");
    }
    // And the clamp is a clamp: past the ceiling every level answers the same as the ceiling.
    assert_eq!(
        globe::edge_segments(30, 4.0, 0.5),
        globe::edge_segments(255, 4.0, 0.5),
        "a zoom past MAX_ZOOM should read as MAX_ZOOM"
    );
}

/// A degenerate viewport must not produce a matrix of NaNs.
///
/// A zero width divides by zero inside `perspective`, and a NaN matrix propagates into every vertex
/// rather than failing anywhere a caller could find it.
#[test]
fn a_degenerate_viewport_still_gives_a_matrix() {
    for (w, h) in [(900.0, 700.0), (0.0, 700.0), (900.0, 0.0), (0.0, 0.0)] {
        let view = tessella_tile::cover::ViewTransform {
            longitude: 0.0,
            latitude: 0.0,
            zoom: 3.0,
            width: w,
            height: h,
            bearing: 0.0,
            pitch: 0.0,
        };
        let m = globe::clip_matrix(&view);
        assert!(
            m.iter().all(|v| v.is_finite()),
            "a {w}x{h} viewport produced a non-finite matrix"
        );
    }
}

// --- The bend, run end to end on the CPU ---------------------------------------------------
//
// plan.md §13.4's bend is a material, and a material is the first thing on that page that can only
// be judged by eye. What can be settled before one exists is the arithmetic it will transcribe:
// `tile-local -> normalized Mercator -> sphere -> clip` is three steps, all of them here, and a
// shader that disagrees with these is wrong rather than merely different.

/// One point through the whole bend, the way a vertex shader will run it.
fn bend(
    view: &cover::ViewTransform,
    z: u8,
    x: u32,
    y: u32,
    wrap: i32,
    local: [f64; 2],
) -> Option<[f64; 3]> {
    let to_mercator = camera::mercator_matrix_for_tile(z, x, y, wrap);
    let mercator = camera::transform_point(&to_mercator, local);
    let point = globe::sphere_point_from_mercator(mercator[0], mercator[1]);
    globe::project_point(&globe::clip_matrix(view), point)
}

/// The tile-local coordinates of a longitude and latitude inside a given tile.
fn local_of(z: u8, x: u32, y: u32, longitude: f64, latitude: f64) -> [f64; 2] {
    let across = f64::from(1u32 << z);
    let world_x = (longitude + 180.0) / 360.0 * across;
    let world_y = camera::mercator_fraction(latitude) * across;
    [
        (world_x - f64::from(x)) * camera::EXTENT,
        (world_y - f64::from(y)) * camera::EXTENT,
    ]
}

#[test]
fn the_mercator_matrix_puts_a_tiles_corners_where_the_tile_is() {
    // z2, column 1, row 2: the square `x in 0.25..0.5`, `y in 0.5..0.75` of the unit world.
    let matrix = camera::mercator_matrix_for_tile(2, 1, 2, 0);
    let corner = |local: [f64; 2]| camera::transform_point(&matrix, local);
    let near =
        |a: [f64; 2], b: [f64; 2]| (a[0] - b[0]).abs() < 1e-12 && (a[1] - b[1]).abs() < 1e-12;
    assert!(near(corner([0.0, 0.0]), [0.25, 0.50]));
    assert!(near(corner([camera::EXTENT, 0.0]), [0.50, 0.50]));
    assert!(near(corner([0.0, camera::EXTENT]), [0.25, 0.75]));
    assert!(near(corner([camera::EXTENT, camera::EXTENT]), [0.50, 0.75]));
}

/// The difference from [`camera::matrix_for_tile`] that the globe exists for.
#[test]
fn the_mercator_matrix_does_not_move_with_the_zoom() {
    let at = |zoom: f64| camera::matrix_for_tile(5, 9, 12, 0, zoom);
    assert_ne!(
        at(5.0),
        at(9.0),
        "the plane's placement is scaled by the zoom"
    );
    // A sphere is one size however far away the camera is; the zoom lives in `clip_matrix`.
    let mercator = camera::mercator_matrix_for_tile(5, 9, 12, 0);
    assert_eq!(mercator, camera::mercator_matrix_for_tile(5, 9, 12, 0));
}

#[test]
fn a_wrapped_tile_lands_one_world_over() {
    let home = camera::transform_point(&camera::mercator_matrix_for_tile(3, 2, 4, 0), [0.0, 0.0]);
    let east = camera::transform_point(&camera::mercator_matrix_for_tile(3, 2, 4, 1), [0.0, 0.0]);
    let west = camera::transform_point(&camera::mercator_matrix_for_tile(3, 2, 4, -1), [0.0, 0.0]);
    assert!((east[0] - home[0] - 1.0).abs() < 1e-12);
    assert!((west[0] - home[0] + 1.0).abs() < 1e-12);
    assert!((east[1] - home[1]).abs() < 1e-12);
}

/// The whole bend, closed: the point the camera is over lands in the middle of the screen.
///
/// This is the check the material is judged against before anything is drawn. It runs every step a
/// vertex shader will run -- the per-tile matrix, the sphere, the globe camera -- against a camera
/// that is not on the equator or the prime meridian, which is the pair of cases a hand check picks
/// and the pair that hid two of the three sign errors in `clip_matrix`.
#[test]
fn the_point_under_the_camera_bends_to_the_middle_of_the_screen() {
    for (longitude, latitude, zoom) in [
        (-122.3321, 47.6062, 4.0), // Seattle
        (139.7671, 35.6812, 6.0),  // Tokyo
        (7.7345, 47.4839, 2.0),    // Liestal
        (0.0, 0.0, 1.0), // the null island case, which must not be the only one that works
    ] {
        let view = cover::ViewTransform {
            longitude,
            latitude,
            zoom,
            width: 900.0,
            height: 700.0,
            bearing: 0.0,
            pitch: 0.0,
        };
        let z = 4u8;
        let across = f64::from(1u32 << z);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let x = ((longitude + 180.0) / 360.0 * across).floor() as u32;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let y = (camera::mercator_fraction(latitude) * across).floor() as u32;

        let local = local_of(z, x, y, longitude, latitude);
        let clip = bend(&view, z, x, y, 0, local).expect("the center of the screen is in front");
        assert!(
            clip[0].abs() < 1e-9 && clip[1].abs() < 1e-9,
            "({longitude}, {latitude}) bent to {clip:?} rather than the middle of the screen",
        );
    }
}

/// North is up after the bend, not just in the matrix.
#[test]
fn the_bend_keeps_north_up() {
    let view = cover::ViewTransform {
        longitude: 7.7345,
        latitude: 47.4839,
        zoom: 3.0,
        width: 900.0,
        height: 700.0,
        bearing: 0.0,
        pitch: 0.0,
    };
    let z = 4u8;
    let across = f64::from(1u32 << z);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let x = ((view.longitude + 180.0) / 360.0 * across).floor() as u32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let y = (camera::mercator_fraction(view.latitude) * across).floor() as u32;

    let center = bend(
        &view,
        z,
        x,
        y,
        0,
        local_of(z, x, y, view.longitude, view.latitude),
    )
    .unwrap();
    let north = bend(
        &view,
        z,
        x,
        y,
        0,
        local_of(z, x, y, view.longitude, view.latitude + 1.0),
    )
    .unwrap();
    assert!(
        north[1] > center[1],
        "a degree north of the center landed at {north:?}, below {center:?}",
    );
}

/// A tile on the far side of the planet is behind the horizon, and the two say so together.
#[test]
fn the_bend_and_the_horizon_agree_about_the_far_side() {
    let view = cover::ViewTransform {
        longitude: 0.0,
        latitude: 0.0,
        zoom: 1.0,
        width: 900.0,
        height: 700.0,
        bearing: 0.0,
        pitch: 0.0,
    };
    let distance = globe::camera_distance(view.zoom, view.latitude, view.height);
    // The antipode of the camera: normalized Mercator x of 0.0 is longitude -180.
    let point = globe::sphere_point_from_mercator(0.0, 0.5);
    assert!(
        !globe::faces_camera(point, [0.0, 0.0, 1.0], distance),
        "the antipode faces the camera",
    );
    // It still projects -- a projection cannot express occlusion, which is why the cull exists.
    assert!(globe::project_point(&globe::clip_matrix(&view), point).is_some());
}

/// Where the bend's 32-bit arithmetic runs out, measured rather than asserted.
///
/// The consumer's vertex stage receives the placement as `f32` and multiplies in `f32`, so a
/// tile-local step of one unit has to survive `scale * local + translate` against a translation of
/// up to one whole world. One unit is `1 / (2^z * EXTENT)`; the ulp near the middle of `0..1` is
/// about 6e-8. **z11 is the last zoom where a unit still moves the result, and z12 collapses it to
/// zero.**
///
/// That is not a defect to fix here, and a wider float is not the fix: GLSL ES has no double, so
/// past this the bend has to be re-anchored per tile rather than expressed in world coordinates.
/// MapLibre GL JS switches its globe to Mercator around z12, which is the same boundary arrived at
/// from the other side. The material comments quote this test.
#[test]
fn the_bends_f32_placement_resolves_a_tile_unit_through_z11() {
    let resolves = |z: u8| {
        let across = f64::from(1u32 << z);
        // The worst case is a tile in the middle of the world, where the translation is largest.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let x = (across / 2.0) as u32;
        let matrix = camera::mercator_matrix_for_tile(z, x, 0, 0);
        #[allow(clippy::cast_possible_truncation)]
        let (scale, translate) = (matrix[0] as f32, matrix[12] as f32);
        let at = |local: f32| scale * local + translate;
        at(1.0) - at(0.0) > 0.0
    };
    for z in 0..=11 {
        assert!(
            resolves(z),
            "z{z} lost a tile unit before it was expected to"
        );
    }
    assert!(
        !resolves(12),
        "z12 resolved a tile unit, so the boundary moved"
    );
}

/// One zoom is one scale, under either projection.
///
/// `world_size(zoom)` is the equator. Mercator stretches every other latitude by `1 / cos` to keep
/// its angles, so a globe whose radius came straight from `world_size / 2pi` draws `cos(latitude)`
/// of the scale the same zoom gives a plane -- 0.68 at Basel, 0.66 at Bellingham. Measured against
/// the flat path before it was fixed: 0.67582, and `cos(47.4839 deg)` is 0.67580.
///
/// Two things made that worse than a constant error. It is a function of the *latitude*, so panning
/// north rescales a map nobody zoomed and the tile level goes with it; and it means a map cannot
/// switch between the two projections without the picture jumping by half again.
#[test]
fn a_globe_and_a_plane_draw_one_zoom_at_one_scale() {
    for latitude in [0.0_f64, 31.24, 47.4839, 48.7519, 70.0] {
        for zoom in [4.0_f64, 8.0, 12.0] {
            let view = cover::ViewTransform {
                longitude: -122.4787,
                latitude,
                zoom,
                width: 1024.0,
                height: 768.0,
                bearing: 0.0,
                pitch: 0.0,
            };
            let settled = camera::settled(&view);
            let clip = globe::clip_matrix(&settled);

            // A degree of longitude either side of the center, on the sphere and on the plane.
            let step = 0.01;
            let on_screen = |longitude: f64| {
                let point = globe::sphere_point(longitude, settled.latitude);
                let ndc = globe::project_point(&clip, point).expect("in front of the camera");
                (ndc[0] + 1.0) / 2.0 * view.width
            };
            let bent = on_screen(settled.longitude + step) - on_screen(settled.longitude - step);
            let flat = 2.0 * step / 360.0 * camera::world_size(zoom);

            let ratio = bent / flat;
            assert!(
                (ratio - 1.0).abs() < 2e-3,
                "at {latitude} deg, z{zoom}: a globe draws {ratio:.5} of the plane's scale"
            );
        }
    }
}

/// The anchored expansion agrees with the exact bend, and by a bound that tightens with the zoom.
///
/// A second implementation of the same function, checked against the first without rendering
/// anything -- which is the whole reason the arithmetic was closed before any material was written.
/// The coefficients are analytic, and an analytic derivative is exactly the kind of thing that is
/// wrong in one term and right in the rest, so this walks a grid over the tile rather than sampling
/// the center it was expanded about.
#[test]
fn the_anchored_bend_reproduces_the_exact_one() {
    // Worst screen error a quadratic can leave, per zoom. A tile subtends less sphere as the zoom
    // rises, so the third-order term this drops falls with it. Below z9 the expansion is the wrong
    // tool -- a z6 tile is a third of a pixel out -- and that is why it is chosen per tile.
    for (zoom, bound) in [(9.0_f64, 0.02), (11.0, 0.001), (13.0, 0.001), (16.0, 0.001)] {
        let (width, height) = (1024.0_f64, 768.0_f64);
        let view = camera::settled(&cover::ViewTransform {
            longitude: -121.8947,
            latitude: 36.6002,
            zoom,
            width,
            height,
            bearing: 0.0,
            pitch: 0.0,
        });
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let z = zoom.floor() as u8;
        let scale = f64::from(1u32 << z);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let x = ((view.longitude + 180.0) / 360.0 * scale).floor() as u32;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let y = (camera::mercator_fraction(view.latitude) * scale).floor() as u32;

        let clip = globe::clip_matrix(&view);
        let placement = camera::mercator_matrix_for_tile(z, x, y, 0);
        let bend = globe::anchored_bend(&view, z, x, y, 0);

        let extent = camera::EXTENT;
        let screen = |c: [f64; 4]| {
            (
                (c[0] / c[3] + 1.0) / 2.0 * width,
                (1.0 - c[1] / c[3]) / 2.0 * height,
            )
        };
        let mut worst: f64 = 0.0;
        for i in 0..=8 {
            for j in 0..=8 {
                let (u, v) = (extent * f64::from(i) / 8.0, extent * f64::from(j) / 8.0);
                let point = globe::sphere_point_from_mercator(
                    placement[0] * u + placement[12],
                    placement[5] * v + placement[13],
                );
                let exact: [f64; 4] = core::array::from_fn(|r| {
                    let p = [point[0], point[1], point[2], 1.0];
                    (0..4).map(|c| clip[c * 4 + r] * p[c]).sum()
                });
                let (ex, ey) = screen(exact);
                let (gx, gy) = screen(bend.at(u - extent / 2.0, v - extent / 2.0));
                worst = worst.max(((gx - ex).powi(2) + (gy - ey).powi(2)).sqrt());
            }
        }
        assert!(
            worst < bound,
            "z{zoom}: the expansion is {worst} screen pixels from the exact bend, over {bound}"
        );
    }
}

/// The anchor really is the tile's center, which is what makes every other term small.
#[test]
fn the_anchor_is_the_middle_of_its_tile() {
    let view = camera::settled(&cover::ViewTransform {
        longitude: -121.8947,
        latitude: 36.6002,
        zoom: 12.0,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
    });
    let (z, x, y) = (12_u8, 655_u32, 1582_u32);
    let bend = globe::anchored_bend(&view, z, x, y, 0);
    let placement = camera::mercator_matrix_for_tile(z, x, y, 0);
    let extent = camera::EXTENT;
    let point = globe::sphere_point_from_mercator(
        placement[0] * extent / 2.0 + placement[12],
        placement[5] * extent / 2.0 + placement[13],
    );
    let clip = globe::clip_matrix(&view);
    let want: [f64; 4] = core::array::from_fn(|r| {
        let p = [point[0], point[1], point[2], 1.0];
        (0..4).map(|c| clip[c * 4 + r] * p[c]).sum()
    });
    let centered = bend.at(0.0, 0.0);
    for (index, (&got, &expected)) in bend.anchor.iter().zip(want.iter()).enumerate() {
        assert!(
            (got - expected).abs() < 1e-12,
            "slot {index}: anchor {got} is not the tile's center {expected}"
        );
        // And at zero offset the expansion is the anchor, by construction.
        assert!((centered[index] - got).abs() < 1e-15);
    }
}

/// The pitch and bearing a globe camera takes, settled against the plane's.
///
/// # There is no globe oracle, and this is the nearest thing to one
///
/// `mbgl-render` has no globe, so nothing outside this tree can say where a pitched globe puts a
/// point. What can say it is the plane: above `kAnchoredFromZoom` the two projections are meant
/// to be interchangeable -- that is the whole premise of the anchored bend and of `clip_w_scale`
/// -- so at street zoom a point a few hundred metres from the centre has to land in the same
/// place under both, whatever the camera is doing. A sign error in either angle moves it by
/// hundreds of pixels, and a rotation about the wrong pivot moves it off the screen.
///
/// Written as a sweep rather than one camera because a single pitch with a single bearing admits
/// three of the four sign combinations: at bearing zero the bearing's sign does not show, and a
/// point due north of the centre is unmoved by it at any pitch.
mod pitched_globe {
    use tessella_tile::{camera, cover, globe};

    /// Screen pixels for a clip point, which is what the two projections have in common.
    fn screen(clip: [f64; 3], view: &cover::ViewTransform) -> [f64; 2] {
        [
            (clip[0] * 0.5 + 0.5) * view.width,
            (0.5 - clip[1] * 0.5) * view.height,
        ]
    }

    /// A longitude and latitude through the plane, in screen pixels.
    fn on_plane(view: &cover::ViewTransform, longitude: f64, latitude: f64) -> Option<[f64; 2]> {
        let matrix = camera::proj_matrix(view).expect("a viewport");
        let world = camera::world_size(view.zoom);
        let point = [
            (longitude + 180.0) / 360.0 * world,
            camera::mercator_fraction(latitude) * world,
            0.0,
        ];
        globe::project_point(&matrix, point).map(|clip| screen(clip, view))
    }

    /// The same point through the sphere, in screen pixels.
    fn on_globe(view: &cover::ViewTransform, longitude: f64, latitude: f64) -> Option<[f64; 2]> {
        let point = globe::sphere_point(longitude, latitude);
        globe::project_point(&globe::clip_matrix(view), point).map(|clip| screen(clip, view))
    }

    #[test]
    fn a_pitched_globe_agrees_with_a_pitched_plane() {
        // Street zoom, where the two projections are meant to be interchangeable. The offsets are
        // a few hundred metres, which at z15 is most of the screen and is where a sign error is
        // largest rather than smallest.
        let (longitude, latitude) = (13.405, 52.52);
        let offsets = [
            (0.0, 0.0),
            (0.004, 0.0),
            (-0.004, 0.0),
            (0.0, 0.002),
            (0.0, -0.002),
            (0.003, 0.002),
        ];
        let mut worst = 0.0f64;
        for pitch in [0.0, 15.0, 30.0, 45.0, 60.0] {
            for bearing in [0.0, 45.0, 90.0, 180.0, 270.0] {
                let view = cover::ViewTransform {
                    longitude,
                    latitude,
                    zoom: 15.0,
                    width: 1024.0,
                    height: 768.0,
                    bearing,
                    pitch,
                };
                for (dlon, dlat) in offsets {
                    let plane = on_plane(&view, longitude + dlon, latitude + dlat)
                        .expect("in front of the plane's camera");
                    let globe = on_globe(&view, longitude + dlon, latitude + dlat)
                        .expect("in front of the globe's camera");
                    let apart =
                        ((plane[0] - globe[0]).powi(2) + (plane[1] - globe[1]).powi(2)).sqrt();
                    // A twentieth of a pixel. A wrong sign in either angle is hundreds, and a
                    // rotation about the sphere's centre rather than the surface point is
                    // thousands, so the bound is not what catches those -- what it catches is a
                    // term that is *nearly* right. What is left at this bound is the difference
                    // between a sphere and a Mercator plane over the offsets, which is the one
                    // disagreement that is supposed to be here.
                    assert!(
                        apart < 0.05,
                        "pitch {pitch} bearing {bearing} offset {dlon},{dlat}: \
                         plane {plane:?} globe {globe:?}, {apart:.4} px apart"
                    );
                    worst = worst.max(apart);
                }
            }
        }
        assert!(worst < 0.05, "worst {worst:.4} px");
    }

    /// The centre stays the centre, which is what pivoting on the surface point buys.
    ///
    /// Rotating about the sphere's centre instead passes every check that only looks at the
    /// unpitched camera and fails this one at every pitch: the point under the camera swings away
    /// by the angle times the radius, which at street zoom is most of a continent.
    #[test]
    fn the_centre_holds_under_any_pitch_or_bearing() {
        for zoom in [1.0, 4.0, 9.0, 14.0] {
            for pitch in [0.0, 30.0, 60.0] {
                for bearing in [0.0, 90.0, 210.0] {
                    let view = cover::ViewTransform {
                        longitude: 7.7345,
                        latitude: 47.4839,
                        zoom,
                        width: 900.0,
                        height: 700.0,
                        bearing,
                        pitch,
                    };
                    let point = globe::sphere_point(view.longitude, view.latitude);
                    let clip = globe::project_point(&globe::clip_matrix(&view), point)
                        .expect("the centre is in front of the camera");
                    assert!(
                        clip[0].abs() < 1e-9 && clip[1].abs() < 1e-9,
                        "zoom {zoom} pitch {pitch} bearing {bearing}: centre at {clip:?}"
                    );
                }
            }
        }
    }

    /// An unpitched, unrotated globe is exactly what it was before the camera gained either.
    ///
    /// The composition splits the pull-back in two so the rotations can pivot on the surface, and
    /// at zero angles the halves have to come back together *exactly* rather than nearly: every
    /// parity number the globe has was measured on this camera, and a matrix that differed in the
    /// last bit would move a z15 vertex by a fifth of a pixel.
    ///
    /// Compared against the old composition rebuilt here rather than against a recorded triple,
    /// because a recorded triple only says the code agrees with itself on the day it was written.
    #[test]
    fn the_flat_on_camera_is_untouched() {
        for (longitude, latitude, zoom) in [
            (-122.3321, 47.6062, 6.0),
            (139.7671, 35.6812, 11.0),
            (0.0, 0.0, 1.0),
            (7.7345, 47.4839, 15.0),
        ] {
            let view = cover::ViewTransform {
                longitude,
                latitude,
                zoom,
                width: 900.0,
                height: 700.0,
                bearing: 0.0,
                pitch: 0.0,
            };
            // The old composition: one pull-back from the sphere's centre, no rotations between.
            let distance = globe::camera_distance(view.zoom, view.latitude, view.height);
            let turned = camera::rotate_y(
                &camera::rotate_x(&camera::identity(), -view.latitude.to_radians()),
                -view.longitude.to_radians(),
            );
            let mut back = camera::identity();
            camera::translate_in_place(&mut back, 0.0, 0.0, -distance);
            let eye = camera::scale(&camera::identity(), 1.0, -1.0, 1.0);
            let eye = camera::multiply(&eye, &camera::multiply(&back, &turned));
            let (near, far) = globe::depth_range(&view);
            #[allow(clippy::cast_possible_truncation)]
            let fov = f64::from(camera::DEFAULT_FOV as f32);
            let projection = camera::perspective(fov, view.width / view.height, near, far);
            let mut expected = camera::multiply(&projection, &eye);
            // `clip_matrix` carries `clip_w_scale`, which is a uniform scale of the whole matrix
            // and moves nothing on screen -- see its own doc. The composition above is the bare
            // projection, so the reference takes the same scale to be comparable element by
            // element rather than projectively.
            let scale = globe::clip_w_scale(&view);
            for value in &mut expected {
                *value *= scale;
            }

            let actual = globe::clip_matrix(&view);
            assert_eq!(
                actual, expected,
                "zoom {zoom} at {longitude},{latitude}: the split pull-back did not recombine"
            );
        }
    }
}
