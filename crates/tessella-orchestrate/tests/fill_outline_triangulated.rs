//! Which of a fill's two outline paths a layer takes.
//!
//! # What mbgl decides, and where
//!
//! `render_fill_layer.cpp`, in the branch where the style names no `fill-pattern`:
//!
//! ```text
//! dataDrivenOutline = !evaluated.get<FillOutlineColor>().isConstant() ||
//!                     !evaluated.get<FillOpacity>().isConstant()
//! ```
//!
//! and the triangulated outline is used when `doOutline && !dataDrivenOutline`. The reason is
//! the shader: `FillOutlineTriangulatedShader` declares the line family's two attributes and no
//! paint at all, so a colour that varies per feature has nowhere to travel and the layer keeps
//! `FillOutlineShader` over the fill's own vertices.
//!
//! The subtle half is the colour's *fallback*. `fill-outline-color` undefined does not mean
//! constant: thirty lines earlier the same file assigns
//! `evaluated.get<FillOutlineColor>() = evaluated.get<FillColor>()`, so a layer with a
//! data-driven `fill-color` and no outline colour of its own has a data-driven outline. That is
//! the common case in a real basemap -- a park layer coloured by `kind` -- and getting it wrong
//! sends the geometry to a shader with no attribute to read it from.

use tessella_orchestrate::ubo::fill_outline_triangulates;
use tessella_style::Style;

fn paint(paint_json: &str) -> Resolved {
    let style = format!(
        r##"{{
          "version": 8,
          "sources": {{}},
          "layers": [
            {{ "id": "f", "type": "fill", "source": "s", "source-layer": "l",
               "paint": {paint_json} }}
          ]
        }}"##
    );
    let style = Style::parse(&style).expect("style parses");
    let layer = style.layer("f").expect("the fill layer").clone();
    tessella_style::property::resolve_paint(&layer).expect("paint resolves")
}

type Resolved = std::collections::BTreeMap<&'static str, tessella_style::ResolvedProperty>;

/// A layer whose colour and opacity are the layer's own takes the polyline.
#[test]
fn a_constant_outline_triangulates() {
    let resolved = paint(r##"{ "fill-color": "#ff0000", "fill-opacity": 0.5 }"##);
    assert!(fill_outline_triangulates(&resolved, false));
}

/// An outline colour that varies per feature does not.
#[test]
fn a_data_driven_outline_colour_keeps_the_line_path() {
    let resolved = paint(
        r##"{ "fill-color": "#ff0000",
              "fill-outline-color": ["match", ["get", "kind"], "park", "#00ff00", "#0000ff"] }"##,
    );
    assert!(!fill_outline_triangulates(&resolved, false));
}

/// Nor does one inherited from a data-driven `fill-color`, which is mbgl's own fallback.
#[test]
fn an_inherited_data_driven_colour_keeps_the_line_path() {
    let resolved =
        paint(r##"{ "fill-color": ["match", ["get", "kind"], "park", "#00ff00", "#0000ff"] }"##);
    assert!(
        !fill_outline_triangulates(&resolved, false),
        "an undefined outline colour is the fill's, binding and all"
    );
}

/// Nor does a per-feature opacity, which the shader has no attribute for either.
#[test]
fn a_data_driven_opacity_keeps_the_line_path() {
    let resolved = paint(
        r##"{ "fill-color": "#ff0000",
              "fill-opacity": ["match", ["get", "kind"], "park", 0.5, 1.0] }"##,
    );
    assert!(!fill_outline_triangulates(&resolved, false));
}

/// A zoom curve is not per-feature: mbgl's `isConstant` is about the data, not the camera.
#[test]
fn a_zoom_curve_still_triangulates() {
    let resolved = paint(
        r##"{ "fill-color": "#ff0000",
              "fill-opacity": ["interpolate", ["linear"], ["zoom"], 6, 0, 11, 1] }"##,
    );
    assert!(fill_outline_triangulates(&resolved, false));
}

/// And a patterned fill takes its own outline shader whatever its colour is.
#[test]
fn a_pattern_keeps_its_own_outline() {
    let resolved = paint(r##"{ "fill-color": "#ff0000" }"##);
    assert!(!fill_outline_triangulates(&resolved, true));
}
