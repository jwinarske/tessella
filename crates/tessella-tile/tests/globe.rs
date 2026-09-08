//! The globe projection, checked against identities rather than an oracle.
//!
//! MapLibre Native has no globe, so there is nothing to render against. What can be pinned is what
//! the geometry has to be true of regardless of implementation: the poles, the round trip, the
//! antimeridian, and the horizon's tangent.

use tessella_tile::camera;
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
        let distance = globe::camera_distance(f64::from(zoom), 700.0);
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
        let distance = globe::camera_distance(view.zoom, view.height);
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
    let distance = globe::camera_distance(view.zoom, view.height);

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
        let distance = globe::camera_distance(view.zoom, view.height);
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
