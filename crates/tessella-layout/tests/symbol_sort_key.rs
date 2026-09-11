//! `symbol-sort-key` holds a layer's features in the order the style asked for.
//!
//! # Why the order is the whole of the feature
//!
//! Nothing about a sort key is visible in one label. What it decides is which of *several*
//! labels wins, and it decides it three times over: the repeat-distance filter keeps the first
//! anchor it sees carrying a given name and drops every later one within half a
//! `symbol-spacing`; placement competes labels in list order; and the vertices are emitted in
//! list order, which is what puts one label over another.
//!
//! Found by measurement. The Protomaps road-label layers set `["get", "min_zoom"]`, and every
//! anchor this build generated for them matched `mbgl-render`'s exactly — all forty-seven — while
//! two labels still drew somewhere else. The repeat filter had seen the same anchors in a
//! different order and kept different ones.

use std::collections::BTreeMap;

use tessella_layout::symbol_layout::SymbolLayout;
use tessella_style::expression::Feature;
use tessella_style::{Layer, Value};

/// A feature with a name and a rank.
struct Road(BTreeMap<String, Value>);

impl Road {
    fn new(name: &str, rank: f64) -> Self {
        let mut properties = BTreeMap::new();
        properties.insert("name".to_string(), Value::String(name.to_string()));
        properties.insert("rank".to_string(), Value::Number(rank));
        Self(properties)
    }
}

impl Feature for Road {
    fn property(&self, key: &str) -> Option<Value> {
        self.0.get(key).cloned()
    }
    fn geometry_type(&self) -> &str {
        "LineString"
    }
}

fn layer(extra: &str) -> Layer {
    let text = format!(
        r#"{{"id": "roads", "type": "symbol", "source": "s",
             "layout": {{"text-field": ["get", "name"], "symbol-placement": "line"{extra}}}}}"#
    );
    serde_json::from_str(&text).expect("a layer")
}

/// A line long enough to be a line, in tile units.
fn line() -> Vec<Vec<(f32, f32)>> {
    vec![vec![(100.0, 100.0), (4000.0, 100.0)]]
}

/// The names a layout holds, in the order it holds them.
fn order(layer: &Layer, roads: &[(&str, f64)]) -> Vec<String> {
    let mut layout = SymbolLayout::new(layer, 15.0, 1.0);
    for (name, rank) in roads {
        layout.push(
            layer,
            15.0,
            &Road::new(name, *rank),
            &line(),
            tessella_layout::PaintValues::default(),
        );
    }
    layout
        .pending
        .iter()
        .map(|pending| pending.text.clone())
        .collect()
}

/// Without a sort key the features stay in the order the tile gave them.
#[test]
fn no_sort_key_keeps_the_tile_s_order() {
    let plain = layer("");
    assert_eq!(
        order(&plain, &[("c", 3.0), ("a", 1.0), ("b", 2.0)]),
        ["c", "a", "b"]
    );
}

/// With one, the lowest key comes first.
#[test]
fn a_sort_key_orders_the_features() {
    let sorted = layer(r#", "symbol-sort-key": ["get", "rank"]"#);
    assert_eq!(
        order(&sorted, &[("c", 3.0), ("a", 1.0), ("b", 2.0)]),
        ["a", "b", "c"]
    );
}

/// Ties insert *before* what they tie with, which reverses their relative order.
///
/// mbgl's `std::lower_bound` with a comparison on the key alone, and this is a property of the
/// arrangement rather than an accident of it: `lower_bound` answers the first element not less
/// than the new one, and inserting there puts the new one in front of every equal key.
#[test]
fn ties_insert_in_front_of_what_they_tie_with() {
    let sorted = layer(r#", "symbol-sort-key": ["get", "rank"]"#);
    assert_eq!(
        order(&sorted, &[("a", 1.0), ("b", 1.0), ("c", 1.0)]),
        ["c", "b", "a"]
    );
}

/// `symbol-z-order: viewport-y` asks for a different ordering, so the key is not applied.
///
/// mbgl's `sortFeaturesByKey = symbolZOrder != ViewportY && hasSymbolSortKey` — the two are
/// alternatives. `auto` and `source` both leave the key in force.
#[test]
fn viewport_y_declines_the_sort_key() {
    let by_viewport =
        layer(r#", "symbol-sort-key": ["get", "rank"], "symbol-z-order": "viewport-y""#);
    assert_eq!(
        order(&by_viewport, &[("c", 3.0), ("a", 1.0), ("b", 2.0)]),
        ["c", "a", "b"],
        "the tile's order stands"
    );
    for named in ["auto", "source"] {
        let held = layer(&format!(
            r#", "symbol-sort-key": ["get", "rank"], "symbol-z-order": "{named}""#
        ));
        assert_eq!(
            order(&held, &[("c", 3.0), ("a", 1.0), ("b", 2.0)]),
            ["a", "b", "c"],
            "{named} keeps the key"
        );
    }
}

/// A key the feature does not carry is zero, not a reason to drop the label.
#[test]
fn a_missing_key_sorts_as_zero() {
    let sorted = layer(r#", "symbol-sort-key": ["get", "missing"]"#);
    assert_eq!(
        order(&sorted, &[("a", 1.0), ("b", 2.0)]),
        ["b", "a"],
        "every key is zero, so every one ties and the order reverses"
    );
}
