//! How much of a flat cover a globe would hide (§13, and the frontend plan's §16).
//!
//! # The question
//!
//! The globe is drawn by bending Mercator geometry per vertex in the material — the producer
//! emits the ordinary flat placement and knows nothing about it. That is true of *placement*.
//! Tile **selection** is a separate question: `cover` culls a frustum against a plane, and on a
//! sphere the visible set is a spherical cap. A flat cull can therefore ask for tiles the globe
//! has curved out of sight, and every one of those is a fetch, a decode, a bucket build and a
//! subdivision spent on geometry behind the planet.
//!
//! This measures it rather than arguing about it, because the answer decides whether the
//! producer is in the globe story at all. If the over-cover is a tile or two the globe is purely
//! a consumer concern; if it is half the cover at the zooms a globe sweep spends its time in,
//! then four views multiply that by four and it belongs in the producer's cover.
//!
//! # The model
//!
//! The frontend's `mlfToGlobe`, read as geometry rather than as a shader. The sphere's
//! circumference is the flat world's width, so its radius is `world_size / 2π` in the same
//! world pixels everything else here is in. A flat point maps back through Mercator — x is
//! longitude, y inverts through `atan(sinh(·))` — and lands on the sphere at that longitude and
//! latitude.
//!
//! A tile is behind the horizon when the angle between its surface normal and the map centre's
//! exceeds `acos(R / (R + d))`, where `d` is the camera's distance to the centre. That is the
//! ordinary horizon of a sphere seen from a finite distance, and it degenerates correctly: as
//! the camera closes in, `d` shrinks, the horizon tightens, and at the zooms where a tile spans
//! a fraction of a degree there is nothing to cull.

use std::f64::consts::PI;

use tessella_orchestrate::sweep;
use tessella_tile::camera;
use tessella_tile::cover::{self, ViewTransform};

/// A unit normal on the sphere for a normalized Mercator point.
fn normal(x: f64, y: f64) -> [f64; 3] {
    let longitude = (x - 0.5) * 2.0 * PI;
    let latitude = (PI * (1.0 - 2.0 * y)).sinh().atan();
    [
        latitude.cos() * longitude.cos(),
        latitude.cos() * longitude.sin(),
        latitude.sin(),
    ]
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// The centre of a tile, in normalized Mercator.
fn tile_centre(z: u8, x: u32, y: u32) -> (f64, f64) {
    let span = f64::from(1u32 << z);
    ((f64::from(x) + 0.5) / span, (f64::from(y) + 0.5) / span)
}

/// How many of a view's cover tiles are copies of the world rather than parts of it.
///
/// A Mercator plane repeats horizontally, so a cover near a low zoom legitimately holds the same
/// tile at several `wrap` values and the map draws each. A sphere has no copies: every wrap of a
/// tile bends to the *same* patch, so drawing two is drawing one twice — z-fighting on the
/// surface, and paying subdivision twice at the zooms where subdivision is dearest.
fn world_copies(view: &ViewTransform) -> (usize, usize) {
    let Ok(tiles) = cover::cover(view) else {
        return (0, 0);
    };
    let copies = tiles.iter().filter(|tile| tile.wrap != 0).count();
    (copies, tiles.len())
}

/// How many of a view's cover tiles the globe would have hidden.
///
/// By the tile's own position, ignoring its `wrap` — a copy of a hidden tile is hidden too, and
/// counting them apart would mix this question with the one above.
fn behind_the_horizon(view: &ViewTransform) -> (usize, usize) {
    let Ok(tiles) = cover::cover(view) else {
        return (0, 0);
    };

    // The sphere, in the world pixels the camera is measured in.
    let radius = camera::world_size(view.zoom) / (2.0 * PI);
    let distance = camera::camera_to_center_distance(view.height);
    let horizon = radius / (radius + distance);

    // The map centre's own normal, which is where the camera is looking.
    let centre_y = {
        let offset = camera::center_offset(view.longitude, view.latitude, view.zoom);
        0.5 - offset[1] / camera::world_size(view.zoom)
    };
    let centre_x = 0.5 + view.longitude / 360.0;
    let centre = normal(centre_x, centre_y);

    let hidden = tiles
        .iter()
        .filter(|tile| {
            let (x, y) = tile_centre(tile.z, tile.x, tile.y);
            dot(normal(x, y), centre) < horizon
        })
        .count();
    (hidden, tiles.len())
}

/// A cluster-sized viewport at a given zoom.
fn at_zoom(base: &ViewTransform, zoom: f64) -> ViewTransform {
    camera::settled(&ViewTransform {
        zoom,
        width: 1920.0,
        height: 1080.0,
        ..*base
    })
}

/// What a globe would cull, across the zooms it is visible at.
///
/// Reported rather than asserted at a number: the number is what this exists to find out, and
/// pinning it would make a change in the cover look like a change in the globe.
#[test]
fn measure_what_a_globe_hides() {
    let views = sweep::four_views();
    println!("\n zoom | cover | behind horizon | world copies | wasted");
    println!("------+-------+----------------+--------------+-------");

    let mut worst = 0.0_f64;
    for step in 0..=16 {
        let zoom = f64::from(step) / 2.0;
        // The four views sit within a fiftieth of a degree of each other, so at these zooms
        // they want the same tiles and the shared store builds them once — which is why one row
        // per zoom says what four would.
        let view = at_zoom(&views[0], zoom);
        let (hidden, total) = behind_the_horizon(&view);
        let (copies, _) = world_copies(&view);
        if total == 0 {
            continue;
        }
        // A tile is wasted if it is hidden or a copy; the two overlap, so the union is what a
        // globe would actually decline to draw.
        let wasted = hidden.max(copies);
        #[allow(clippy::cast_precision_loss)]
        let share = wasted as f64 / total as f64;
        worst = worst.max(share);
        println!(
            " {zoom:4.1} | {total:5} | {hidden:14} | {copies:12} | {:4.0}%",
            share * 100.0
        );
    }
    println!("\nworst share a globe would not draw: {:.0}%\n", worst * 100.0);

    // The one thing worth asserting: the model degenerates. By the zooms the existing sweep
    // runs over, a tile spans a fraction of a degree and there is nothing for a horizon to cut.
    for base in &views {
        let (hidden, total) = behind_the_horizon(&at_zoom(base, sweep::SWEEP_LOW));
        assert_eq!(
            hidden, 0,
            "at z{}, {hidden} of {total} tiles were called hidden — the globe is flat there and \
             a model that culls anything is wrong about the horizon, not about the cover",
            sweep::SWEEP_LOW
        );
    }
}
