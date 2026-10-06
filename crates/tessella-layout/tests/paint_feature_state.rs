// SPDX-License-Identifier: BSD-2-Clause
//! A paint binder that reads per-feature state, which is what makes a highlight a highlight.
//!
//! `["feature-state", key]` evaluates against the feature being painted (tessella#337). The
//! operator reads it off the feature, as `["get", …]` reads a property, so the state has to arrive
//! that way -- and the binder is where it is attached, because that is the scope it is constant
//! over: one style layer, one source layer, keyed by the feature's own id.
//!
//! What is checked here is the only thing that matters downstream: the same feature, the same
//! style, two different states, and **different bytes in the vertex buffer**. A binder that
//! accepted a lookup and did not consult it would pass every test that read its slots or its
//! stride.

use std::collections::BTreeMap;
use std::sync::Arc;

use tessella_layout::paint::{PaintBinder, StateLookup};
use tessella_style::expression::Feature;
use tessella_style::property::{PropertySpec, ResolvedProperty, paint_specs};
use tessella_style::{Layer, Value, property::resolve_paint};

/// A feature with an id and one tag, which is what a tile's features are.
struct Road {
    id: Option<u64>,
}

impl Feature for Road {
    fn property(&self, key: &str) -> Option<Value> {
        (key == "kind").then(|| Value::String("road".into()))
    }

    fn geometry_type(&self) -> &str {
        "LineString"
    }

    fn id(&self) -> Option<Value> {
        #[allow(clippy::cast_precision_loss)]
        self.id.map(|id| Value::Number(id as f64))
    }
}

/// State for one source layer: feature id to key to value.
#[derive(Debug, Default)]
struct States(BTreeMap<u64, BTreeMap<String, Value>>);

impl States {
    fn with(id: u64, key: &str, value: Value) -> Arc<Self> {
        let mut outer = BTreeMap::new();
        outer.insert(id, [(key.to_string(), value)].into_iter().collect());
        Arc::new(Self(outer))
    }
}

impl StateLookup for States {
    fn state(&self, id: u64, key: &str) -> Option<Value> {
        self.0.get(&id)?.get(key).cloned()
    }
}

/// A line layer whose color is a highlight through state.
fn layer() -> Layer {
    serde_json::from_str(
        r##"{
            "id": "roads", "type": "line", "source": "s", "source-layer": "roads",
            "paint": {
                "line-color": ["case", ["==", ["feature-state", "hover"], true],
                               "#ff0000", "#204060"],
                "line-width": 2
            }
        }"##,
    )
    .expect("the layer parses")
}

fn resolved(layer: &Layer) -> BTreeMap<&'static str, ResolvedProperty> {
    resolve_paint(layer).expect("the paint resolves")
}

fn specs(layer: &Layer) -> &'static [PropertySpec] {
    paint_specs(&layer.kind).expect("a line layer has paint specs")
}

/// The buffer a binder writes for one feature, with and without a state lookup.
fn bytes(states: Option<Arc<States>>, feature: &dyn Feature) -> Vec<u8> {
    let layer = layer();
    let paint = resolved(&layer);
    let mut binder = PaintBinder::new(specs(&layer), &paint, 14.0)
        .with_state(states.map(|states| states as Arc<dyn StateLookup>));
    binder.push(2, &paint, feature).expect("the feature pushes");
    binder.data().to_vec()
}

/// A hovered feature paints differently from the same feature unhovered.
///
/// The whole point, and the one assertion that cannot be satisfied by a binder which takes the
/// lookup and never asks it.
#[test]
fn state_changes_the_bytes_a_feature_paints() {
    let road = Road { id: Some(7) };
    let plain = bytes(None, &road);
    let hovered = bytes(Some(States::with(7, "hover", Value::Bool(true))), &road);

    assert!(
        !plain.is_empty(),
        "the line's color is data-driven, so there are bytes"
    );
    assert_eq!(
        plain.len(),
        hovered.len(),
        "the layout is the same either way"
    );
    assert_ne!(
        plain, hovered,
        "a hovered feature painted the same bytes as an unhovered one"
    );
}

/// State for another feature is not this feature's.
#[test]
fn the_state_is_keyed_by_the_features_own_id() {
    let road = Road { id: Some(7) };
    let plain = bytes(None, &road);
    let other = bytes(Some(States::with(8, "hover", Value::Bool(true))), &road);
    assert_eq!(plain, other, "state set on feature 8 reached feature 7");
}

/// And a feature with no id has no state, because that is what the specification keys it by.
#[test]
fn a_feature_without_an_id_has_no_state() {
    let anonymous = Road { id: None };
    let plain = bytes(None, &anonymous);
    let marked = bytes(
        Some(States::with(7, "hover", Value::Bool(true))),
        &anonymous,
    );
    assert_eq!(plain, marked, "a feature with no id was given state anyway");
}

/// A feature whose id is a *string* has no state either, which is a limit rather than a rule.
///
/// MVT states a feature's id as a `uint64`, which is what the lookup is keyed by, and that covers
/// every feature in a vector tile. A GeoJSON source may give an id as a string, and such a feature
/// cannot be named by this key -- so it paints as unmarked rather than taking somebody else's state,
/// which is the safe end to fail at. Naming them would mean a second key, and that is the place to
/// start if a host needs it.
#[test]
fn a_string_id_cannot_be_named_by_the_lookup() {
    struct Named;
    impl Feature for Named {
        fn property(&self, _key: &str) -> Option<Value> {
            None
        }
        fn geometry_type(&self) -> &str {
            "LineString"
        }
        fn id(&self) -> Option<Value> {
            Some(Value::String("motorway-7".into()))
        }
    }

    let plain = bytes(None, &Named);
    let marked = bytes(Some(States::with(7, "hover", Value::Bool(true))), &Named);
    assert_eq!(
        plain, marked,
        "a feature whose id is a string was given the state of feature 7"
    );
}

/// A lookup that holds a different key leaves the feature alone.
#[test]
fn a_different_key_is_not_the_one_the_style_reads() {
    let road = Road { id: Some(7) };
    let plain = bytes(None, &road);
    let elsewhere = bytes(Some(States::with(7, "selected", Value::Bool(true))), &road);
    assert_eq!(plain, elsewhere, "the style reads `hover`, not `selected`");
}

/// Two binders that wrote the same bytes are equal, whichever lookup they read.
///
/// The hand-written `PartialEq` says what a caller comparing binders means: the same buffer. The
/// bytes are the answer to "did it come out the same", so a lookup that produced identical output
/// is not a difference -- and one that produced different output is already visible as the bytes.
#[test]
fn equality_is_the_buffer_rather_than_the_lookup() {
    let road = Road { id: Some(7) };
    let layer = layer();
    let paint = resolved(&layer);

    let mut plain = PaintBinder::new(specs(&layer), &paint, 14.0);
    plain.push(2, &paint, &road).expect("pushes");

    // The same feature through a lookup that has nothing for it: identical bytes, so equal.
    let mut empty = PaintBinder::new(specs(&layer), &paint, 14.0)
        .with_state(Some(Arc::new(States::default()) as Arc<dyn StateLookup>));
    empty.push(2, &paint, &road).expect("pushes");
    assert_eq!(plain, empty);

    // And a lookup that does change the paint is not equal, because the bytes are not.
    let mut hovered = PaintBinder::new(specs(&layer), &paint, 14.0).with_state(Some(States::with(
        7,
        "hover",
        Value::Bool(true),
    )
        as Arc<dyn StateLookup>));
    hovered.push(2, &paint, &road).expect("pushes");
    assert_ne!(plain, hovered);
}
