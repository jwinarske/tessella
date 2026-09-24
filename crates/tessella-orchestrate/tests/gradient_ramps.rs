// SPDX-License-Identifier: BSD-2-Clause
//! A `line-gradient` layer's ramp and its shader, against the oracle.
//!
//! # What the oracle gives
//!
//! `tests/golden/gradient_style.dump`: three polylines under a plain line layer and two gradient
//! ones, over a `lineMetrics: true` GeoJSON source. Two of the roads cross z13 tile boundaries,
//! which is where `lineMetrics` earns its place -- a line cut at an edge has to spread its progress
//! over the whole line rather than over its own piece.
//!
//! # Why this capture exists
//!
//! `line-gradient` was implemented, unit-tested in `line_metrics.rs`, and compared against the
//! oracle nowhere: no golden fixture and no parity scene in the tree set `line-gradient` or
//! `lineMetrics` at all. What it pins:
//!
//! - **The shader changes.** A layer with a gradient takes `LineGradientShader` (`sh0026`) where a
//!   plain one takes `LineShader` (`sh0025`).
//! - **The ramp is 256 texels wide whatever the stops are.** mbgl bakes `line-gradient` into one
//!   256x1 image per layer -- `RenderLineLayer`'s `colorRamp` -- so a two-stop ramp and a
//!   five-stop ramp are the same size and differ only in content. That is the opposite of
//!   `color-relief`, whose ramp is *stop-count* wide and which `relief_shapes.rs` pins at 5x1 for
//!   five stops. Two families, two conventions, and reading one for the other gives a ramp of the
//!   wrong width.
//! - **One ramp per gradient layer, none for a plain line.**
//!
//! # What is deliberately not compared: the vertex counts
//!
//! They do not match, and the reason is tessella#274 rather than anything about gradients. mbgl
//! simplifies GeoJSON geometry as it cuts tiles (`geojson-vt`'s `tolerance = 3`) and this build does
//! not, so a line carries every point its author wrote. On this fixture the oracle spends 44 line
//! vertices where this spends 76; on a 400-point route-shaped line it is 44 against 2,316.
//!
//! The pixels are what agree: this fixture reads **0 gross of 786,432** against `mbgl-render`, with
//! 17,029 non-background pixels and some 6,800 distinct colors on both sides -- so the ramp really
//! is drawn and really is sampled across the line. Asserting the vertex counts here would pin a
//! number that is expected to move the day #274 is closed.

use tessella_orchestrate::gradient::Gradients;
use tessella_orchestrate::tile::{TileId, build_tile};
use tessella_source::tiling::TilingOptions;
use tessella_style::Style;

const DUMP: &str = include_str!("../../../tests/golden/gradient_style.dump");
const STYLE: &str = include_str!("../../tessella-style/tests/gradient_style.json");

/// The z13 cover of the capture's camera.
const TILES: [(u32, u32); 6] = [
    (4092, 2723),
    (4092, 2724),
    (4093, 2723),
    (4093, 2724),
    (4094, 2723),
    (4094, 2724),
];

/// `LineShader` and `LineGradientShader`, counting from `BuiltIn::None`.
const LINE: &str = "sh0025";
const LINE_GRADIENT: &str = "sh0026";

/// Every `texture WxH` the capture recorded.
fn oracle_textures() -> Vec<(u32, u32)> {
    DUMP.lines()
        .filter_map(|line| line.strip_prefix("texture "))
        .filter_map(|rest| rest.split_whitespace().next())
        .filter_map(|size| {
            let (w, h) = size.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .collect()
}

/// This frame's ramps, keyed by the layer that owns one.
fn our_gradients() -> (Gradients, Style) {
    let style = Style::parse(STYLE).expect("the style parses");
    let tessella_style::Source::Geojson(source) = style.source("probe").expect("a source") else {
        panic!("the fixture has one geojson source")
    };
    let features = tessella_source::geojson::read(&source.data).expect("features read");

    let mut buckets = Vec::new();
    for (x, y) in TILES {
        let tile = TileId::new(13, x, y);
        let built = build_tile(&style, "probe", tile, &features, TilingOptions::default())
            .expect("the tile builds");
        buckets.push((tile, std::sync::Arc::new(built)));
    }
    let dashes = tessella_orchestrate::dash::Dashes::default();
    let gradients = Gradients::for_buckets(&style, &buckets, &dashes, 100);
    (gradients, style)
}

/// The source's `lineMetrics` is read, which is what makes a cut line's progress whole-line.
#[test]
fn line_metrics_reaches_the_source() {
    let style = Style::parse(STYLE).expect("the style parses");
    let tessella_style::Source::Geojson(source) = style.source("probe").expect("a source") else {
        panic!("the fixture has one geojson source")
    };
    assert_eq!(
        source.line_metrics,
        Some(true),
        "the fixture sets lineMetrics, and a gradient over a cut line needs it"
    );
}

/// A gradient layer takes the gradient shader; a plain line does not.
#[test]
fn a_gradient_layer_takes_the_gradient_shader() {
    let style = Style::parse(STYLE).expect("the style parses");
    let index = |want: &str| {
        style
            .layers
            .iter()
            .position(|layer| layer.id == want)
            .unwrap_or_else(|| panic!("no layer {want}"))
    };
    let shaders_of = |layer: usize| -> Vec<&str> {
        DUMP.lines()
            .filter(|line| line.starts_with("drawable L"))
            .filter(|line| line.contains(&format!("L{layer:05}.")))
            .filter_map(|line| {
                if line.contains(LINE_GRADIENT) {
                    Some(LINE_GRADIENT)
                } else if line.contains(LINE) {
                    Some(LINE)
                } else {
                    None
                }
            })
            .collect()
    };

    let plain = shaders_of(index("line-plain"));
    assert!(!plain.is_empty(), "the plain layer drew nothing");
    assert!(
        plain.iter().all(|&shader| shader == LINE),
        "a plain line takes LineShader: {plain:?}"
    );

    for named in ["line-gradient-five", "line-gradient-two"] {
        let drawn = shaders_of(index(named));
        assert!(!drawn.is_empty(), "{named} drew nothing");
        assert!(
            drawn.iter().all(|&shader| shader == LINE_GRADIENT),
            "{named} should take LineGradientShader: {drawn:?}"
        );
    }
}

/// One 256-texel ramp per gradient layer, whatever its stop count, and none for a plain line.
///
/// The width is the fact worth pinning: `color-relief`'s ramp is as wide as the style has stops
/// (`relief_shapes.rs` pins 5x1 for five), and a line gradient's is always 256 because mbgl bakes
/// it rather than carrying the stops.
#[test]
fn every_gradient_layer_bakes_a_256_texel_ramp() {
    let (gradients, style) = our_gradients();
    let index = |want: &str| {
        style
            .layers
            .iter()
            .position(|layer| layer.id == want)
            .unwrap_or_else(|| panic!("no layer {want}"))
    };

    let five = gradients
        .get(index("line-gradient-five"))
        .expect("the five-stop layer has a ramp");
    let two = gradients
        .get(index("line-gradient-two"))
        .expect("the two-stop layer has a ramp");
    for (named, ramp) in [("five", five), ("two", two)] {
        assert_eq!(
            ramp.pixels.len(),
            256 * 4,
            "the {named}-stop ramp should be 256 RGBA texels"
        );
    }
    assert!(
        gradients.get(index("line-plain")).is_none(),
        "a plain line layer has no ramp to upload"
    );

    // The oracle uploads the same two, and at the same size.
    let rows: Vec<(u32, u32)> = oracle_textures()
        .into_iter()
        .filter(|&(width, height)| height == 1 && width == 256)
        .collect();
    assert_eq!(
        rows.len(),
        2,
        "two gradient layers, two 256x1 ramps in the capture: {:?}",
        oracle_textures()
    );

    // And the stops reach the texels: two different ramps are two different images.
    assert_ne!(
        five.pixels, two.pixels,
        "a five-stop ramp and a two-stop one should not bake to the same 256 texels"
    );
    // The five-stop ramp runs blue to red through cyan, green and yellow, so its ends differ.
    let (first, last) = (&five.pixels[..4], &five.pixels[252 * 4..256 * 4]);
    assert_ne!(first, last, "a ramp whose ends agree is not a ramp");
}
