// SPDX-License-Identifier: BSD-2-Clause
//! The projection a raster is drawn through, snapped to the pixel grid.
//!
//! mbgl keeps a second projection matrix for raster and hillshade tiles, shifted by under a pixel
//! so that a tile's texels land on screen pixels rather than between them. What these check is
//! the property that shift exists for, on cameras whose center is and is not already on the grid.

use tessella_tile::camera::{self, Mat4};
use tessella_tile::cover::ViewTransform;

/// A flat, north-up view of `width` by `height` at zoom 13.
fn view(longitude: f64, latitude: f64, width: f64, height: f64) -> ViewTransform {
    ViewTransform {
        longitude,
        latitude,
        zoom: 13.0,
        width,
        height,
        bearing: 0.0,
        pitch: 0.0,
        ground_below: 0.0,
    }
}

/// Where a world-pixel point lands on screen, in pixels from the left and top.
fn screen(matrix: &Mat4, view: &ViewTransform, point: (f64, f64)) -> (f64, f64) {
    let clip = [
        matrix[0] * point.0 + matrix[4] * point.1 + matrix[12],
        matrix[1] * point.0 + matrix[5] * point.1 + matrix[13],
        matrix[3] * point.0 + matrix[7] * point.1 + matrix[15],
    ];
    let (ndc_x, ndc_y) = (clip[0] / clip[2], clip[1] / clip[2]);
    (
        (ndc_x + 1.0) / 2.0 * view.width,
        (1.0 - ndc_y) / 2.0 * view.height,
    )
}

/// The world pixel under the center of `view`.
fn center_pixel(view: &ViewTransform) -> (f64, f64) {
    let world = camera::world_size(view.zoom);
    let [x, y] = camera::center_offset(view.longitude, view.latitude, view.zoom);
    (0.5 * world - x, 0.5 * world - y)
}

/// A whole world pixel a few pixels from the center, which is on screen whatever the camera.
fn nearby_whole_pixel(view: &ViewTransform) -> (f64, f64) {
    let (x, y) = center_pixel(view);
    (x.floor() - 3.0, y.floor() - 2.0)
}

fn distance_from_whole(value: f64) -> f64 {
    (value - value.round()).abs()
}

/// Off the grid, a whole world pixel lands on a whole screen pixel only through the aligned
/// matrix.
///
/// draw-a-circle's camera, whose center sits at a fraction of (0.891, 0.554) of a pixel. Through
/// the plain matrix an OSM tile's texels fall that far between screen pixels, which is every thin
/// line in the basemap blended into its neighbors.
#[test]
fn an_off_grid_center_is_snapped_onto_the_grid() {
    let view = view(2.3454, 48.8452, 1024.0, 768.0);
    let (cx, cy) = center_pixel(&view);
    assert!(
        distance_from_whole(cx) > 0.05 && distance_from_whole(cy) > 0.05,
        "the camera is meant to be off the grid: {cx} {cy}"
    );

    let point = nearby_whole_pixel(&view);
    let plain = camera::proj_matrix(&view).expect("a viewport");
    let aligned = camera::aligned_proj_matrix(&view).expect("a viewport");

    let (px, py) = screen(&plain, &view, point);
    assert!(
        distance_from_whole(px) > 0.05 || distance_from_whole(py) > 0.05,
        "the plain matrix already lands on the grid: {px} {py}"
    );
    let (ax, ay) = screen(&aligned, &view, point);
    assert!(
        distance_from_whole(ax) < 1e-6 && distance_from_whole(ay) < 1e-6,
        "the aligned matrix lands at {ax} {ay}"
    );
}

/// On the grid already, there is nothing to shift.
#[test]
fn an_on_grid_center_is_left_alone() {
    let off = view(2.3454, 48.8452, 1024.0, 768.0);
    let (cx, cy) = center_pixel(&off);
    let world = camera::world_size(off.zoom);
    // Back from a whole world pixel to a longitude and latitude.
    let longitude = cx.round() / world * 360.0 - 180.0;
    let latitude = (core::f64::consts::PI * (1.0 - 2.0 * cy.round() / world))
        .sinh()
        .atan()
        .to_degrees();
    let on = view(longitude, latitude, 1024.0, 768.0);

    let plain = camera::proj_matrix(&on).expect("a viewport");
    let aligned = camera::aligned_proj_matrix(&on).expect("a viewport");
    let point = nearby_whole_pixel(&on);
    let (px, py) = screen(&plain, &on, point);
    let (ax, ay) = screen(&aligned, &on, point);
    assert!(
        (px - ax).abs() < 1e-4 && (py - ay).abs() < 1e-4,
        "moved from {px} {py} to {ax} {ay}"
    );
}

/// The shift is never more than half a pixel, odd viewport or even.
#[test]
fn the_shift_is_at_most_half_a_pixel() {
    for (width, height) in [(1024.0, 768.0), (1023.0, 767.0)] {
        for (longitude, latitude) in [(2.3454, 48.8452), (-77.04, 38.907), (13.405, 52.52)] {
            let view = view(longitude, latitude, width, height);
            let point = nearby_whole_pixel(&view);
            let plain = camera::proj_matrix(&view).expect("a viewport");
            let aligned = camera::aligned_proj_matrix(&view).expect("a viewport");
            let (px, py) = screen(&plain, &view, point);
            let (ax, ay) = screen(&aligned, &view, point);
            assert!(
                (px - ax).abs() <= 0.5 + 1e-6 && (py - ay).abs() <= 0.5 + 1e-6,
                "{width}x{height} at {longitude},{latitude}: moved {} {}",
                ax - px,
                ay - py
            );
        }
    }
}
