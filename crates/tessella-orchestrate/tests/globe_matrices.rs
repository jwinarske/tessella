// SPDX-License-Identifier: Apache-2.0

//! What a drawable is placed by under each projection — plan.md §13.4's producer half.
//!
//! A Mercator drawable carries `proj_matrix * placement` and reaches clip space. A globe's carries
//! the placement alone and reaches *normalized Mercator*: the two steps after it are the bend onto
//! the sphere, which is trig rather than a matrix, and `CameraUpdate::globe_matrix`, which is per
//! frame rather than per drawable.
//!
//! There is no oracle for any of this. What is checkable is that the plane is byte-identical to
//! what it was, that the globe's matrix is the one `mercator_matrix_for_tile` computes, and that
//! the depth offset survives the move to a slot a matrix multiply used to fold it into.

use tessella_capture_abi::ProjectionMode;
use tessella_orchestrate::ubo::{DrawableEntry, depth_offset};
use tessella_tile::camera;
use tessella_tile::cover::ViewTransform;
use tessella_tile::globe;

fn view() -> ViewTransform {
    ViewTransform {
        longitude: -122.3321,
        latitude: 47.6062,
        zoom: 4.0,
        width: 900.0,
        height: 700.0,
        bearing: 0.0,
        pitch: 0.0,
    }
}

const TILE: (u8, u32, u32, i32) = (4, 2, 5, 0);

fn entry(projection: ProjectionMode, layer: i32, sub: i32) -> [f32; 16] {
    let (z, x, y, wrap) = TILE;
    DrawableEntry::for_tile(&view(), projection, z, x, y, wrap, layer, sub)
        .expect("the entry builds")
        .matrix
}

/// The plane is exactly what it was: this change must be invisible to a Mercator map.
#[test]
fn the_plane_still_carries_projection_times_placement() {
    let (z, x, y, wrap) = TILE;
    let view = view();
    let mut clip = camera::proj_matrix(&view).expect("a projection");
    clip[14] -= f64::from(depth_offset(3, 0));
    let want = camera::multiply(&clip, &camera::matrix_for_tile(z, x, y, wrap, view.zoom));

    #[allow(clippy::cast_possible_truncation)]
    let want: [f32; 16] = core::array::from_fn(|index| want[index] as f32);
    assert_eq!(entry(ProjectionMode::Mercator, 3, 0), want);
}

/// A globe's is the placement alone, over a unit world, with the depth nudge scaled to the frustum.
#[test]
fn a_globe_carries_the_mercator_placement() {
    let (z, x, y, wrap) = TILE;
    let mut want = camera::mercator_matrix_for_tile(z, x, y, wrap);
    let (near, far) = globe::depth_range(&view());
    want[14] = -f64::from(depth_offset(3, 0)) * (far - near);

    #[allow(clippy::cast_possible_truncation)]
    let want: [f32; 16] = core::array::from_fn(|index| want[index] as f32);
    assert_eq!(entry(ProjectionMode::Globe, 3, 0), want);
}

/// The tile's corners land where the tile is, read straight off the emitted matrix.
///
/// The end-to-end bend is pinned in `tessella-tile`'s own tests; this is the narrower claim that
/// what reaches the *wire* is the same matrix those tests ran through.
#[test]
fn the_emitted_matrix_puts_the_tile_where_it_belongs() {
    let matrix = entry(ProjectionMode::Globe, 0, 0);
    let at = |local: [f32; 2]| {
        [
            matrix[0] * local[0] + matrix[4] * local[1] + matrix[12],
            matrix[1] * local[0] + matrix[5] * local[1] + matrix[13],
        ]
    };
    #[allow(clippy::cast_possible_truncation)]
    let extent = camera::EXTENT as f32;
    // z4, column 2, row 5: x in 2/16..3/16, y in 5/16..6/16 of the unit world.
    let near = |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).abs() < 1e-6 && (a[1] - b[1]).abs() < 1e-6;
    assert!(
        near(at([0.0, 0.0]), [2.0 / 16.0, 5.0 / 16.0]),
        "{:?}",
        at([0.0, 0.0])
    );
    assert!(near(at([extent, extent]), [3.0 / 16.0, 6.0 / 16.0]));
}

/// The placement's *scale and translation* are the tile's alone; only the depth term is the
/// camera's.
///
/// This was a pure function of the tile until the depth nudge had to be scaled to the frustum. A
/// globe's frustum shrinks as the camera closes on the surface -- 1.7 deep at z4 and 0.055 at z14
/// -- so an absolute nudge is 56% of the range at z14 and pushes its layer through the near plane.
/// Scaling it is what stopped the planet going black up there, and the cost is this: `[14]` moves
/// with the zoom where the rest does not.
///
/// What that does *not* cost is §5.1. The bucket is still camera-free; this matrix lives in a
/// per-drawable uniform block that is written every frame either way, as the plane's is.
#[test]
fn only_the_depth_term_of_a_globes_placement_moves_with_the_zoom() {
    let (z, x, y, wrap) = TILE;
    let at = |zoom: f64, projection| {
        let mut view = view();
        view.zoom = zoom;
        DrawableEntry::for_tile(&view, projection, z, x, y, wrap, 0, 0)
            .unwrap()
            .matrix
    };
    let (low, high) = (
        at(4.0, ProjectionMode::Globe),
        at(9.0, ProjectionMode::Globe),
    );
    for index in 0..16 {
        if index == 14 {
            assert_ne!(
                low[index], high[index],
                "the depth term did not follow the frustum"
            );
        } else {
            assert_eq!(
                low[index], high[index],
                "slot {index} moved with the camera"
            );
        }
    }
    assert_ne!(
        at(4.0, ProjectionMode::Mercator),
        at(9.0, ProjectionMode::Mercator),
        "the plane's placement is scaled by the zoom",
    );
}

/// The depth offset had a matrix multiply to fold it into and now does not.
///
/// Under Mercator it biases the projection's `[14]` before the multiply; under a globe there is no
/// multiply, so it rides in the placement's own `[14]` -- free, because tile geometry is 2D and
/// nothing writes that slot on the way in. Layers still separate, and by the same amount.
#[test]
fn the_depth_offset_survives_the_move() {
    for projection in [ProjectionMode::Mercator, ProjectionMode::Globe] {
        let low = entry(projection, 0, 0);
        let high = entry(projection, 40, 0);
        assert_ne!(low, high, "{projection:?} gave two layers the same depth");
    }
    // On a globe the slot is readable directly, which is the property the consumer depends on --
    // scaled to the frustum, because a globe's is 1.7 deep at z4 and 0.055 at z14 and an absolute
    // nudge is over half the range up there.
    let (z, x, y, wrap) = TILE;
    let (near, far) = globe::depth_range(&view());
    for (layer, sub) in [(0, 0), (7, 1), (40, 0)] {
        let matrix =
            DrawableEntry::for_tile(&view(), ProjectionMode::Globe, z, x, y, wrap, layer, sub)
                .unwrap()
                .matrix;
        // Multiplied in f64 and cast once, as the producer does. Scaling in f32 instead differs
        // in the last bit, which is a real difference on a wire that is compared byte for byte.
        #[allow(clippy::cast_possible_truncation)]
        let want = (-f64::from(depth_offset(layer, sub)) * (far - near)) as f32;
        assert_eq!(matrix[14], want);
        // Everything else is the placement, untouched.
        let want = camera::mercator_matrix_for_tile(z, x, y, wrap);
        #[allow(clippy::cast_possible_truncation)]
        for index in [0usize, 5, 12, 13] {
            assert_eq!(matrix[index], want[index] as f32, "slot {index} moved");
        }
    }
}

/// A wrapped tile still lands one world over, which is what keeps a fold-rather-than-filter cover
/// drawable at all.
#[test]
fn a_wrapped_tile_is_still_placed() {
    let (z, x, y, _) = TILE;
    let home = DrawableEntry::for_tile(&view(), ProjectionMode::Globe, z, x, y, 0, 0, 0)
        .unwrap()
        .matrix;
    let east = DrawableEntry::for_tile(&view(), ProjectionMode::Globe, z, x, y, 1, 0, 0)
        .unwrap()
        .matrix;
    assert!((east[12] - home[12] - 1.0).abs() < 1e-6);
    assert_eq!(east[13], home[13]);
}

/// A view with no area is refused on the plane and not on a globe, and that difference is real.
///
/// The plane's matrix *is* the camera, so an unusable viewport has no answer. A globe's placement
/// does not consult the camera at all -- it is a pure function of the tile address -- so there is
/// nothing to fail at, and inventing a failure would mean a globe refusing tiles a plane only
/// refuses because of arithmetic it does not share. The viewport is guarded once, in
/// `globe::clip_matrix`, which hands back a square frustum rather than a matrix of NaNs.
#[test]
fn an_empty_viewport_fails_where_it_is_the_cameras_to_fail() {
    let (z, x, y, wrap) = TILE;
    let mut empty = view();
    empty.width = 0.0;
    assert!(
        DrawableEntry::for_tile(&empty, ProjectionMode::Mercator, z, x, y, wrap, 0, 0).is_err()
    );

    let globe = DrawableEntry::for_tile(&empty, ProjectionMode::Globe, z, x, y, wrap, 0, 0)
        .expect("a globe's placement does not need a viewport");
    let wide =
        DrawableEntry::for_tile(&view(), ProjectionMode::Globe, z, x, y, wrap, 0, 0).unwrap();
    assert_eq!(
        globe.matrix, wide.matrix,
        "the viewport reached the placement"
    );
}

/// A globe draws its background per tile, never as one quad over the viewport.
///
/// The viewport quad is placed by a matrix that does not consult the camera, which is right for
/// something standing in for a clear and is why it cannot be bent: there is no tile behind it
/// whose Mercator span a vertex stage could turn into a patch of sphere. Left in, a globe draws a
/// rectangle with a curved coastline on it.
#[test]
fn a_globe_refuses_the_viewport_background() {
    const STYLE: &str = r##"{
        "version": 8,
        "sources": {},
        "layers": [{ "id": "bg", "type": "background",
                     "paint": { "background-color": "#f4f1ea" } }]
    }"##;
    let style = tessella_style::Style::parse(STYLE).expect("the style parses");
    assert!(
        tessella_orchestrate::tile::background_covers_viewport(
            &style,
            4.0,
            ProjectionMode::Mercator
        ),
        "a plane with a background first layer takes the shortcut",
    );
    assert!(
        !tessella_orchestrate::tile::background_covers_viewport(&style, 4.0, ProjectionMode::Globe),
        "a globe took the viewport shortcut, which cannot be bent",
    );
}
