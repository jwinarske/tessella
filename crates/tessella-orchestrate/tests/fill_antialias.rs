//! Whether a fill draws an outline at all -- mbgl's `doOutline`.
//!
//! ```text
//! doOutline = evaluated.get<FillAntialias>() &&
//!             (unevaluated.get<FillPattern>().isUndefined() ||
//!              unevaluated.get<FillOutlineColor>().isUndefined())
//! ```
//!
//! A fill's outline is its antialiasing, so `fill-antialias: false` is one drawable and not two.
//! The second half is mbgl's own comment -- "Outline does not default to fill in the pattern
//! case": a patterned fill whose outline colour the style wrote asks for a colour the pattern
//! shaders have no uniform for, and mbgl draws no outline rather than the wrong one.
//!
//! Counted as drawables rather than as pixels because that is what the rule decides. A layer
//! that draws an outline it should not is a hairline in the fill's own colour around every
//! polygon, which at threshold 48 against a matching fill colour is nothing at all.

use tessella_orchestrate::order;
use tessella_orchestrate::tile::{TileId, build_tile};
use tessella_source::geojson;
use tessella_source::tiling::TilingOptions;
use tessella_style::{Source, Style};

/// One square, and a fill layer over it with whatever paint the case is about.
///
/// Twenty degrees a side, which at z0 is hundreds of tile units. A hundredth of a degree is a
/// quarter of one, so every corner rounds to the same integer and the polyline generator returns
/// on mbgl's own `len < 3` -- which would make this a test about degenerate geometry rather than
/// about `doOutline`.
fn drawables(paint: &str) -> Vec<i32> {
    let style = format!(
        r##"{{
          "version": 8,
          "sources": {{ "probe": {{ "type": "geojson", "data": {{
            "type": "Feature", "properties": {{}},
            "geometry": {{ "type": "Polygon", "coordinates": [[
              [-20.0, -20.0], [20.0, -20.0], [20.0, 20.0], [-20.0, 20.0], [-20.0, -20.0]
            ]] }} }} }} }},
          "layers": [
            {{ "id": "f", "type": "fill", "source": "probe", "paint": {paint} }}
          ]
        }}"##
    );
    let style = Style::parse(&style).expect("style parses");
    let Some(Source::Geojson(source)) = style.source("probe") else {
        panic!("a geojson source");
    };
    let features = geojson::read(&source.data).expect("features read");
    let buckets = build_tile(
        &style,
        "probe",
        TileId::new(0, 0, 0),
        &features,
        TilingOptions::default(),
    )
    .expect("tile builds");

    let mut next_id = 0;
    order::bindings_for(
        tessella_capture_abi::envelope::ViewId(0),
        order::tile_of(0, 0, 0),
        &buckets,
        &mut next_id,
        true,
    )
    .iter()
    .map(|binding| binding.sub_layer_index)
    .collect()
}

/// The default: triangles at sub-layer 1, outline at 2.
#[test]
fn a_plain_fill_draws_both() {
    assert_eq!(drawables(r##"{ "fill-color": "#ff0000" }"##), [1, 2]);
}

/// `fill-antialias: false` drops the outline, and only the outline.
#[test]
fn antialias_off_drops_the_outline() {
    assert_eq!(
        drawables(r##"{ "fill-color": "#ff0000", "fill-antialias": false }"##),
        [1],
        "the fill still draws"
    );
}

/// A patterned fill keeps its outline while the style writes no colour for it.
#[test]
fn a_pattern_alone_keeps_its_outline() {
    assert_eq!(drawables(r##"{ "fill-pattern": "sand" }"##), [1, 2]);
}

/// And loses it when the style does, which is the rule that is not obvious.
#[test]
fn a_pattern_with_an_outline_colour_draws_no_outline() {
    assert_eq!(
        drawables(r##"{ "fill-pattern": "sand", "fill-outline-color": "#00ff00" }"##),
        [1],
        "mbgl: the outline does not default to the fill in the pattern case"
    );
}
