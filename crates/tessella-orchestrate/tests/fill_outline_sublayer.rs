//! Where a fill's outline sorts, and what the buffer that feeds it has to look like.
//!
//! mbgl decides it in one line of `render_fill_layer.cpp`:
//!
//! ```text
//! builder->setSubLayerIndex(unevaluated.get<FillOutlineColor>().isUndefined() ? 2 : 0);
//! ```
//!
//! An outline the style did not ask for is the fill's own antialiasing, drawn in the fill's own
//! color, and it belongs on top. One the style *did* ask for is a different color and goes
//! underneath, so the fill covers its inner half; drawn on top instead, that half is not covered
//! and the line reads twice as wide.
//!
//! The half worth a test is not the number. It is that three places have to agree about it --
//! `bindings_for` numbers the drawables, `frame::part_of` maps a number back to the record it
//! draws, and the drawable UBO buffer is packed in the order `ubo_index` counts. Getting the
//! first right and the last wrong is what this catches: every outline then reads the *fill's*
//! block, which at a fill layer's scale is a white sheet over the whole viewport, and the last
//! drawable reads past the end and is dropped. 96% of the frame, from one buffer packed in the
//! wrong order.

use tessella_orchestrate::order::bindings_for;
use tessella_orchestrate::tile::{TileId, build_tile};
use tessella_orchestrate::{Content, LayerBucket};
use tessella_source::tiling::TilingOptions;
use tessella_style::Style;

const TILE: TileId = TileId::new(0, 0, 0);

fn wire_tile() -> tessella_capture_abi::envelope::TileId {
    tessella_orchestrate::order::tile_of(0, 0, 0)
}

fn style_with(paint: &str) -> Style {
    let json = format!(
        r#"{{"version":8,
            "sources":{{"s":{{"type":"geojson","data":{{
              "type":"FeatureCollection","features":[
                {{"type":"Feature","properties":{{}},"geometry":{{"type":"Polygon","coordinates":[
                  [[-10,-10],[10,-10],[10,10],[-10,10],[-10,-10]]]}}}}]}}}}}},
            "layers":[{{"id":"f","type":"fill","source":"s","paint":{{{paint}}}}}]}}"#
    );
    Style::parse(&json).expect("the style parses")
}

fn buckets(style: &Style) -> Vec<LayerBucket> {
    let tessella_style::Source::Geojson(source) = style.source("s").expect("a source") else {
        panic!("the fixture has one geojson source");
    };
    let features = tessella_source::geojson::read(&source.data).expect("features read");
    build_tile(style, "s", TILE, &features, TilingOptions::default()).expect("the tile builds")
}

fn sub_layers(style: &Style) -> Vec<i32> {
    let built = buckets(style);
    assert!(
        matches!(built[0].content, Content::Fill(_)),
        "the fixture layer is a fill"
    );
    let mut next = 0;
    bindings_for(
        tessella_capture_abi::envelope::ViewId(0),
        wire_tile(),
        &built,
        &mut next,
        true,
    )
    .into_iter()
    .map(|binding| binding.sub_layer_index)
    .collect()
}

/// No `fill-outline-color`: the outline is the antialiasing and draws over the fill.
#[test]
fn an_unasked_for_outline_draws_over_the_fill() {
    let style = style_with(r##""fill-color":"#3f6fff""##);
    assert!(!tessella_orchestrate::ubo::fill_outline_under_fill(
        &style.layers[0]
    ));
    assert_eq!(sub_layers(&style), [1, 2]);
}

/// `fill-outline-color` set: the outline draws under the fill, at sub-layer zero.
#[test]
fn an_outline_the_style_asked_for_draws_under_the_fill() {
    let style = style_with(r##""fill-color":"#3f6fff","fill-outline-color":"#ffffff""##);
    assert!(tessella_orchestrate::ubo::fill_outline_under_fill(
        &style.layers[0]
    ));
    assert_eq!(sub_layers(&style), [1, 0]);
}

/// The rule reads the style's own value, not what it evaluates to. A layer setting the outline
/// to the same color as its fill still asked for one.
#[test]
fn the_rule_is_whether_the_style_wrote_it() {
    let same = style_with(r##""fill-color":"#3f6fff","fill-outline-color":"#3f6fff""##);
    assert!(tessella_orchestrate::ubo::fill_outline_under_fill(
        &same.layers[0]
    ));
}

/// Whichever way it sorts, a fill is two drawables and no more.
#[test]
fn a_fill_is_two_drawables_either_way() {
    for paint in [
        r##""fill-color":"#3f6fff""##,
        r##""fill-color":"#3f6fff","fill-outline-color":"#ffffff""##,
    ] {
        let style = style_with(paint);
        let built = buckets(&style);
        assert_eq!(built[0].drawable_count(), 2);
        assert_eq!(sub_layers(&style).len(), 2);
    }
}

/// `fill-antialias: false` draws no outline at all, so the sub-layer question does not arise.
#[test]
fn a_layer_with_antialias_off_has_no_outline_to_sort() {
    let style = style_with(r##""fill-color":"#3f6fff","fill-antialias":false"##);
    assert_eq!(sub_layers(&style), [1]);
}
