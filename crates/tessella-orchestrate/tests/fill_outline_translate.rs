//! A fill's outline takes the layer's `fill-translate`, whichever form the outline is drawn in.
//!
//! mbgl builds *one* matrix per tile in `FillLayerTweaker::execute`:
//!
//! ```text
//! const auto matrix = getTileMatrix(tileID, parameters, translation, anchor, nearClipped, ...);
//! ```
//!
//! and every `FillVariant` writes that same matrix into its block — `Fill`, `FillOutline`,
//! `FillPattern`, `FillOutlinePattern` and `FillOutlineTriangulated` alike. The translate is a
//! property of the *layer*, not of one of its drawables.
//!
//! # What this was
//!
//! `FillOutlineTriangulatedEntry::for_tile` took no translate and built a plain tile matrix, so
//! on the triangulated path — the one a constant `fill-outline-color` and `fill-opacity` select —
//! the interior moved and its outline stayed on the footprint. The plain line-index path did not
//! have the bug, because it is packed from the same `for_tile_translated` entries the interior is.
//!
//! The style that shows it is the ordinary fake-third-dimension one: a building top drawn up and
//! left of its footprint by `fill-translate: [-2, -2]`, whose outline then sat two pixels down and
//! right of its own edge. Drawn *under* the fill, half of it was covered and the rest ran along
//! the wrong side. Measured against `mbgl-render`, the outline's pixels overlapped the oracle's in
//! 77 places before and 4,799 after, and the best-fit shift between the two masks moved from
//! (-2, -2) to (0, 0).

use tessella_capture_abi::ProjectionMode;
use tessella_orchestrate::ubo::{DrawableEntry, FillOutlineTriangulatedEntry};
use tessella_tile::cover::ViewTransform;

fn view() -> ViewTransform {
    ViewTransform {
        longitude: -90.734_14,
        latitude: 14.555_24,
        zoom: 16.0,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
        ground_below: 0.0,
    }
}

fn outline(translate: [f64; 2]) -> FillOutlineTriangulatedEntry {
    FillOutlineTriangulatedEntry::for_tile(
        &view(),
        ProjectionMode::Mercator,
        14,
        4062,
        7522,
        0,
        7,
        0,
        translate,
    )
    .expect("a view with area")
}

#[test]
fn a_triangulated_outline_moves_with_its_layers_translate() {
    let still = outline([0.0, 0.0]);
    let moved = outline([-2.0, -2.0]);
    assert_ne!(
        still.matrix, moved.matrix,
        "the outline ignored fill-translate"
    );
    // Everything but the placement is the same: a translate moves the drawable, it does not
    // rescale the line's extrusion.
    assert!(
        (still.ratio - moved.ratio).abs() < f32::EPSILON,
        "the translate must not disturb the outline's ratio"
    );
}

/// And it moves by exactly what the interior moves by, which is the half that keeps them together.
///
/// Compared against `DrawableEntry::for_tile_translated` rather than against an expected number:
/// the interior is what the outline has to stay registered with, so the test asks whether the two
/// agree rather than whether either matches an arithmetic this test would be restating.
#[test]
fn a_triangulated_outline_moves_by_what_the_interior_moves_by() {
    let translate = [-2.0, -2.0];
    let interior = |translate| {
        DrawableEntry::for_tile_translated(
            &view(),
            ProjectionMode::Mercator,
            14,
            4062,
            7522,
            0,
            7,
            0,
            [0.0, 0.0],
            translate,
        )
        .expect("a view with area")
        .matrix
    };

    // The same sub-layer for both, so the depth nudge cancels and what is left is the translate.
    let interior_shift: Vec<f32> = interior(translate)
        .iter()
        .zip(interior([0.0, 0.0]).iter())
        .map(|(moved, still)| moved - still)
        .collect();
    let outline_shift: Vec<f32> = outline(translate)
        .matrix
        .iter()
        .zip(outline([0.0, 0.0]).matrix.iter())
        .map(|(moved, still)| moved - still)
        .collect();

    for (index, (fill, line)) in interior_shift.iter().zip(&outline_shift).enumerate() {
        assert!(
            (fill - line).abs() <= f32::EPSILON.max(fill.abs() * 1e-5),
            "element {index}: the interior moved by {fill} and its outline by {line}"
        );
    }
}
