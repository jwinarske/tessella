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

/// The other half of mbgl's rule: when a layer's symbols are drawn in screen-y order.
///
/// `sortFeaturesByY` is four lines of `symbol_layout.cpp` and wider than the property's name:
///
/// ```text
/// zOrderByViewportY = symbolZOrder == ViewportY ||
///                     (symbolZOrder == Auto && !sortFeaturesByKey);
/// sortFeaturesByY   = zOrderByViewportY && (TextAllowOverlap  || IconAllowOverlap ||
///                                           TextIgnorePlacement || IconIgnorePlacement);
/// ```
///
/// So `auto` — the default — orders by viewport y whenever no sort key is in force and any of the
/// four overlap flags is set, which is most POI layers rather than the rare one that writes
/// `viewport-y` by hand. tessella#428, where the sweep read 4,776 pixels on a scene that wrote no
/// `symbol-z-order` at all.
///
/// Each case here differs from the one before it in one property, so what is being read is the
/// rule rather than a particular style.
#[test]
fn the_viewport_y_order_follows_the_overlap_flags() {
    let asked = |extra: &str| SymbolLayout::new(&layer(extra), 14.0, 1.0).sort_by_viewport_y;

    // Nothing set: `auto`, no key, no flags.
    assert!(!asked(""), "a layer with no overlap flag is not sorted");
    assert!(
        asked(r#", "text-allow-overlap": true"#),
        "`auto` with no key orders by viewport y once overlap is allowed"
    );
    for flag in [
        "icon-allow-overlap",
        "text-ignore-placement",
        "icon-ignore-placement",
    ] {
        assert!(
            asked(&format!(r#", "{flag}": true"#)),
            "{flag} asks for the same ordering"
        );
    }
    assert!(
        !asked(r#", "text-allow-overlap": false"#),
        "a flag written false is a flag that is not set"
    );

    // A sort key takes `auto` the other way, and `viewport-y` takes it back.
    assert!(
        !asked(r#", "text-allow-overlap": true, "symbol-sort-key": ["get", "rank"]"#),
        "a key in force is the ordering, so viewport y is not"
    );
    assert!(
        asked(
            r#", "text-allow-overlap": true, "symbol-sort-key": ["get", "rank"],
                "symbol-z-order": "viewport-y""#
        ),
        "`viewport-y` declines the key and orders by the screen"
    );
    assert!(
        !asked(r#", "text-allow-overlap": true, "symbol-z-order": "source""#),
        "`source` is the source's order, whatever the flags say"
    );
    assert!(
        asked(r#", "text-allow-overlap": true, "symbol-z-order": "auto""#),
        "`auto` written out is `auto` left unwritten"
    );
}
