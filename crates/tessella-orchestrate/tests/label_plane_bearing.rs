// SPDX-License-Identifier: BSD-2-Clause
//! The bearing a label's plane is built from, which is not the one a camera carries.
//!
//! A symbol that lies flat on the ground but does not turn with it has the map's bearing taken
//! back out of its label plane, so the text stays upright while the ground turns under it. That
//! is the one place in the symbol UBO where the bearing is used as an *angle* rather than through
//! a matrix that already has it, and it is the one place the convention shows: mbgl's
//! `state.getBearing()` is radians and negated against the compass degrees a camera carries.

use tessella_capture_abi::ProjectionMode;
use tessella_layout::size::EvaluatedSize;
use tessella_layout::symbol_layout::{Alignment, Alignments, Placement};
use tessella_orchestrate::ubo::SymbolDrawableEntry;
use tessella_tile::camera;
use tessella_tile::cover::ViewTransform;

const TILE: (u8, u32, u32, i32) = (14, 8802, 5373, 0);

fn view(bearing: f64) -> ViewTransform {
    ViewTransform {
        longitude: 13.405,
        latitude: 52.52,
        zoom: 14.0,
        width: 1024.0,
        height: 768.0,
        bearing,
        pitch: 60.0,
        ground_below: 0.0,
    }
}

fn size() -> EvaluatedSize {
    EvaluatedSize {
        zoom_constant: true,
        feature_constant: true,
        size_t: 0.0,
        size: 18.0,
    }
}

/// The entry for a label with these alignments, at this bearing.
fn entry(bearing: f64, rotation: Alignment, pitch: Alignment) -> SymbolDrawableEntry {
    let (z, x, y, wrap) = TILE;
    SymbolDrawableEntry::for_tile(
        &view(bearing),
        z,
        x,
        y,
        wrap,
        3,
        0,
        [512.0, 512.0],
        [512.0, 512.0],
        size(),
        true,
        Alignments { rotation, pitch },
        Placement::Point,
        ProjectionMode::Mercator,
        false,
        [0.0, 0.0],
    )
    .expect("the entry builds")
}

fn plane(bearing: f64, rotation: Alignment, pitch: Alignment) -> [f32; 16] {
    entry(bearing, rotation, pitch).label_plane_matrix
}

/// The plane is `getLabelPlaneMatrix`'s, with mbgl's own bearing.
#[test]
fn a_flat_upright_label_takes_the_bearing_in_radians_and_negated() {
    let (z, ..) = TILE;
    let bearing = 200.0;
    let want =
        camera::label_plane_matrix_on_map(z, 14.0, camera::bearing_radians(&view(bearing)), false);

    #[allow(clippy::cast_possible_truncation)]
    let want: [f32; 16] = core::array::from_fn(|index| want[index] as f32);
    assert_eq!(
        plane(bearing, Alignment::Viewport, Alignment::Map),
        want,
        "the plane was not built from mbgl's bearing"
    );
}

/// And so is the trip back: `getGlCoordMatrix` takes the same bearing with the opposite sign, and
/// the two have to agree or the offset a glyph is drawn at is rotated against the anchor it is
/// drawn from.
#[test]
fn the_matrix_back_to_clip_takes_the_same_bearing() {
    let (z, x, y, wrap) = TILE;
    let bearing = 200.0;
    let view = view(bearing);
    let mut projection = camera::proj_matrix(&view).expect("a projection");
    projection[14] -= f64::from(tessella_orchestrate::ubo::depth_offset(3, 0));
    let tile = camera::multiply(
        &projection,
        &camera::matrix_for_tile(z, x, y, wrap, view.zoom),
    );
    let want =
        camera::gl_coord_matrix_on_map(&tile, z, 14.0, camera::bearing_radians(&view), false);

    #[allow(clippy::cast_possible_truncation)]
    let want: [f32; 16] = core::array::from_fn(|index| want[index] as f32);
    assert_eq!(
        entry(bearing, Alignment::Viewport, Alignment::Map).coord_matrix,
        want
    );
}

/// The compass degrees a camera carries are neither the unit nor the sign, and this is what they
/// would have given: a label turned by nearly twice the bearing rather than left upright.
#[test]
fn the_cameras_own_degrees_would_be_a_different_plane() {
    let (z, ..) = TILE;
    let bearing = 200.0;
    #[allow(clippy::cast_possible_truncation)]
    let degrees: [f32; 16] = {
        let m = camera::label_plane_matrix_on_map(z, 14.0, bearing, false);
        core::array::from_fn(|index| m[index] as f32)
    };
    assert_ne!(plane(bearing, Alignment::Viewport, Alignment::Map), degrees);
}

/// A label that turns *with* the map has no bearing in its plane at all, so the convention cannot
/// show there -- which is why every other alignment read zero while this one did not.
#[test]
fn a_label_that_turns_with_the_map_is_the_same_at_every_bearing() {
    let flat = |bearing| plane(bearing, Alignment::Map, Alignment::Map);
    assert_eq!(flat(0.0), flat(200.0));
}

/// And one that stands up on screen takes the viewport plane, where the bearing arrives through
/// the tile matrix.
#[test]
fn an_upright_label_takes_the_viewport_plane() {
    let (z, x, y, wrap) = TILE;
    let bearing = 200.0;
    let view = view(bearing);
    let mut projection = camera::proj_matrix(&view).expect("a projection");
    projection[14] -= f64::from(tessella_orchestrate::ubo::depth_offset(3, 0));
    let tile = camera::multiply(
        &projection,
        &camera::matrix_for_tile(z, x, y, wrap, view.zoom),
    );
    let want = camera::label_plane_matrix(&tile, view.width, view.height);

    #[allow(clippy::cast_possible_truncation)]
    let want: [f32; 16] = core::array::from_fn(|index| want[index] as f32);
    assert_eq!(plane(bearing, Alignment::Map, Alignment::Viewport), want);
}
