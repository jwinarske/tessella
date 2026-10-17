// SPDX-License-Identifier: BSD-2-Clause
//! `text-rotate`: the label's own turn, about its own anchor.
//!
//! The quad builder has applied it since it was written and nothing ever told it to: the property
//! was read nowhere, so the field it sets was zero in every frame the renderer has drawn. A
//! shield angled to its road came out square to the screen.

use tessella_glyph::atlas::Rect;
use tessella_glyph::pbf::Metrics;
use tessella_layout::symbol_bucket::{Glyphs, Label, SymbolOptions, build_symbols};
use tessella_layout::symbol_layout::SymbolLayout;
use tessella_style::Layer;

/// A font that answers for every codepoint with a square em.
struct AnyGlyph;

impl Glyphs for AnyGlyph {
    fn metrics(&self, _codepoint: u32) -> Option<(Metrics, bool)> {
        Some((
            Metrics {
                width: 24,
                height: 24,
                left: 0,
                top: -8,
                advance: 24,
            },
            true,
        ))
    }

    fn rect(&self, _codepoint: u32) -> Option<Rect> {
        Some(Rect {
            x: 0,
            y: 0,
            width: 26,
            height: 26,
        })
    }
}

fn label(text: &str) -> Label {
    Label {
        pending: 0,
        sections: vec![tessella_layout::symbol::Section {
            text: text.to_string(),
            scale: 1.0,
            image: None,
            fonts: None,
        }],
        text: text.to_string(),
        anchor: (100.0, 100.0),
    }
}

fn layout_of(layout: &str) -> SymbolLayout {
    let layer: Layer = serde_json::from_str(&format!(
        r#"{{"id": "l", "type": "symbol", "source": "s", "layout": {layout}}}"#
    ))
    .expect("a layer");
    SymbolLayout::new(&layer, 14.0, 1.0)
}

/// Degrees on the wire, radians in the quad builder -- mbgl's `util::deg2radf`.
#[test]
fn the_property_reaches_the_shaping_in_radians() {
    let layout = layout_of(
        r#"{"text-field": "AB", "text-font": ["F"], "text-size": 16, "text-rotate": 30}"#,
    );
    assert!((layout.symbol.rotate - 30.0f32.to_radians()).abs() < 1e-6);
}

/// And a layer that does not write it turns nothing.
#[test]
fn an_unstated_rotation_is_none() {
    let layout = layout_of(r#"{"text-field": "AB", "text-font": ["F"], "text-size": 16}"#);
    assert_eq!(layout.symbol.rotate, 0.0);
}

/// The quads turn with it, about the anchor rather than about each glyph.
///
/// A quarter turn sends the row of glyphs down the screen: what was a wide, short extent becomes
/// a narrow, tall one, and the glyph that was to the right of the anchor is below it.
#[test]
fn a_quarter_turn_stands_the_label_on_end() {
    let options = |rotate: f32| SymbolOptions {
        rotate,
        ..SymbolOptions::default()
    };
    let extent = |rotate: f32| {
        let (buffers, _) = build_symbols(&[label("ABC")], &AnyGlyph, None, &options(rotate));
        // The corner offset, in thirty-seconds of a pixel: `pos_offset` is the anchor and then
        // the corner.
        let xs: Vec<f32> = buffers
            .vertices
            .iter()
            .map(|vertex| f32::from(vertex.pos_offset[2]))
            .collect();
        let ys: Vec<f32> = buffers
            .vertices
            .iter()
            .map(|vertex| f32::from(vertex.pos_offset[3]))
            .collect();
        let span = |values: &[f32]| {
            let (mut low, mut high) = (f32::MAX, f32::MIN);
            for &value in values {
                low = low.min(value);
                high = high.max(value);
            }
            high - low
        };
        (span(&xs), span(&ys))
    };

    let (wide, short) = extent(0.0);
    let (narrow, tall) = extent(core::f32::consts::FRAC_PI_2);
    assert!(wide > short, "a row of glyphs is wider than it is tall");
    assert!(
        (narrow - short).abs() < 2.0 && (tall - wide).abs() < 2.0,
        "a quarter turn did not swap the label's axes: {wide}x{short} became {narrow}x{tall}"
    );
}
