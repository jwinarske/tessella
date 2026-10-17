// SPDX-License-Identifier: BSD-2-Clause
//! A label set down the screen rather than across it.
//!
//! `text-writing-mode: vertical` is a whole writing system and none of it had ever been drawn:
//! the scene `tools/parity/scenes/vertical_p.json` is the first thing in the tree to ask for one.
//! What it found is below.

use tessella_glyph::atlas::Rect;
use tessella_glyph::pbf::Metrics;
use tessella_layout::symbol_bucket::{Glyphs, Label, SymbolOptions, build_symbols};

/// A font that answers for every codepoint, so a fixture can be written in the script the
/// mechanism is for. The vendored `TestFont` is ASCII, and an ASCII label has no upright
/// character in it — which is exactly the case that is *not* set down the screen.
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

fn options(allow_vertical_placement: bool) -> SymbolOptions {
    SymbolOptions {
        allow_vertical_placement,
        ..SymbolOptions::default()
    }
}

/// The quarter turn that makes a row of glyphs a column.
///
/// A vertical shaping is laid out along the *same axis* a horizontal one is — mbgl's shaper
/// advances `x` either way — and the quad builder turns each glyph that keeps its upright form a
/// quarter turn the other way. What makes the label a column is a third turn, of the whole of it,
/// which mbgl carries on the placed symbol: `placedSymbol.angle = allowVerticalPlacement &&
/// writingMode == Vertical ? M_PI_2 : 0`. Nothing here wrote it, so a label the style asked to
/// set down drew as a row with every glyph in it on its side.
#[test]
fn the_vertical_half_is_turned_a_quarter_turn() {
    let (buffers, laid) = build_symbols(&[label("東京")], &AnyGlyph, None, &options(true));
    let vertical = laid[0].vertical.expect("a second shaping");

    let (across, down) = buffers.dynamic.split_at(vertical.at);
    assert!(!across.is_empty() && !down.is_empty(), "both halves drew");
    assert!(
        across.iter().all(|vertex| vertex[2] == 0.0),
        "the half set across is not turned"
    );
    assert!(
        down.iter()
            .all(|vertex| (vertex[2] - core::f32::consts::FRAC_PI_2).abs() < 1e-6),
        "and the half set down is turned a quarter turn"
    );
}

/// Both halves carry their own angle, whichever one placement ends up drawing: mbgl sets it in
/// `addToBuffers`, where neither has been chosen yet.
#[test]
fn the_angle_is_written_before_either_half_is_chosen() {
    let (buffers, laid) = build_symbols(&[label("東京")], &AnyGlyph, None, &options(true));
    let vertical = laid[0].vertical.expect("a second shaping");
    assert_eq!(
        buffers.dynamic[vertical.at - 1][2],
        0.0,
        "the last vertex of the first half"
    );
    assert!(
        (buffers.dynamic[vertical.at][2] - core::f32::consts::FRAC_PI_2).abs() < 1e-6,
        "and the first of the second"
    );
}

/// A layer that did not ask for vertical placement has one shaping and no turn.
#[test]
fn a_label_the_layer_did_not_ask_to_turn_has_one_half() {
    let (buffers, laid) = build_symbols(&[label("東京")], &AnyGlyph, None, &options(false));
    assert!(laid[0].vertical.is_none(), "no second shaping");
    assert!(buffers.dynamic.iter().all(|vertex| vertex[2] == 0.0));
}

/// And a name with no upright character in it has nothing to set down, whatever the layer asks.
#[test]
fn a_latin_name_is_not_set_down() {
    let (_, laid) = build_symbols(&[label("TOKYO")], &AnyGlyph, None, &options(true));
    assert!(laid[0].vertical.is_none());
}

/// The vertical form of a punctuation mark has to be asked for.
///
/// A comma between two ideographs is not drawn as a comma: `verticalizePunctuation` replaces
/// U+3001 with U+FE11, which is a glyph in another *range*. The shaping did the replacing and
/// nothing added the replacement to what the tile fetches, so the mark reached the atlas as a
/// codepoint the font was never asked for and the label lost it — a column of three characters
/// drawn as two with a gap.
mod dependencies {
    use tessella_layout::symbol_layout::SymbolLayout;
    use tessella_style::expression::Feature;
    use tessella_style::{Layer, Value};

    struct Place(&'static str);

    impl Feature for Place {
        fn property(&self, key: &str) -> Option<Value> {
            (key == "name").then(|| Value::String(self.0.to_string()))
        }
        fn geometry_type(&self) -> &str {
            "Point"
        }
    }

    /// What one feature's layer asks the glyph store for.
    fn wanted(layout_json: &str, text: &'static str) -> Vec<u32> {
        let layer: Layer = serde_json::from_str(&format!(
            r#"{{"id": "l", "type": "symbol", "source": "s", "layout": {layout_json}}}"#
        ))
        .expect("a layer");
        let mut layout = SymbolLayout::new(&layer, 14.0, 1.0);
        layout.push(
            &layer,
            14.0,
            &Place(text),
            &[vec![(100.0, 100.0)]],
            tessella_layout::PaintValues::default(),
        );
        layout
            .dependencies()
            .into_values()
            .flatten()
            .collect::<Vec<u32>>()
    }

    const VERTICAL: &str = r#"{"text-field": ["get", "name"], "text-font": ["F"],
                               "text-writing-mode": ["vertical"]}"#;
    const HORIZONTAL: &str = r#"{"text-field": ["get", "name"], "text-font": ["F"]}"#;

    /// U+3001 IDEOGRAPHIC COMMA, and U+FE11 its vertical form.
    #[test]
    fn a_vertical_layer_asks_for_the_mark_it_will_draw() {
        let asked = wanted(VERTICAL, "東、京");
        assert!(asked.contains(&0x3001), "the mark as the style wrote it");
        assert!(asked.contains(&0xFE11), "and the form it is drawn in");
    }

    /// A layer that will never set a label down has no use for them, and mbgl does not ask.
    #[test]
    fn a_horizontal_layer_asks_for_neither() {
        let asked = wanted(HORIZONTAL, "東、京");
        assert!(asked.contains(&0x3001));
        assert!(!asked.contains(&0xFE11));
    }

    /// Nor does a vertical layer whose label has nothing upright in it to set down.
    #[test]
    fn a_latin_label_asks_for_no_vertical_form() {
        let asked = wanted(VERTICAL, "A, B");
        assert!(!asked.contains(&0xFE10), "the vertical comma");
    }
}
