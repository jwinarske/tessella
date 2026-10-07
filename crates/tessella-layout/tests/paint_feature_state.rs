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

use tessella_layout::paint::{FeatureKey, PaintBinder, StateLookup};
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

/// State for one source layer: feature key to key to value.
#[derive(Debug, Default)]
struct States(BTreeMap<FeatureKey, BTreeMap<String, Value>>);

impl States {
    fn with(id: u64, key: &str, value: Value) -> Arc<Self> {
        Self::named(FeatureKey::Number(id), key, value)
    }

    /// The same, for a feature named by a string rather than a number.
    fn named(id: FeatureKey, key: &str, value: Value) -> Arc<Self> {
        let mut outer = BTreeMap::new();
        outer.insert(id, [(key.to_string(), value)].into_iter().collect());
        Arc::new(Self(outer))
    }
}

impl StateLookup for States {
    fn state(&self, id: &FeatureKey, key: &str) -> Option<Value> {
        self.0.get(id)?.get(key).cloned()
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
fn a_string_id_is_named_by_the_lookup() {
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

    // Feature 7's state is not this feature's: a number and a string are different keys, which is
    // the half of #361 that keeps `Number(7)` from marking `Text("motorway-7")`.
    let someone_else = bytes(Some(States::with(7, "hover", Value::Bool(true))), &Named);
    assert_eq!(
        plain, someone_else,
        "a feature whose id is a string took the state of feature 7"
    );

    // Its own name does mark it, which it could not before #361 -- a string id fell in beside the
    // features that have none and lost its state silently.
    let marked = bytes(
        Some(States::named(
            FeatureKey::Text("motorway-7".into()),
            "hover",
            Value::Bool(true),
        )),
        &Named,
    );
    assert_ne!(
        plain, marked,
        "a feature named by a string was not marked, so a query can find it and nothing can mark it"
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

/// Re-painting in place writes exactly what a fresh build with the same state writes.
///
/// The assertion the whole mechanism rests on. A hover changes which features are highlighted, so
/// what has to be redone is those slots over those features' vertices -- and if the patched buffer
/// is byte-identical to a rebuild, the in-place path is correct by construction rather than by
/// inspection.
#[test]
fn restating_writes_what_a_rebuild_writes() {
    let layer = layer();
    let paint = resolved(&layer);
    let roads = [
        Road { id: Some(7) },
        Road { id: Some(8) },
        Road { id: None },
    ];
    let hover = States::with(8, "hover", Value::Bool(true));

    // Built without state, then restated with it.
    let mut patched = PaintBinder::new(specs(&layer), &paint, 14.0);
    let mut filled = 0;
    for road in &roads {
        filled += 2;
        patched.push(filled, &paint, road).expect("pushes");
    }
    patched
        .restate(&paint, Some(hover.clone() as Arc<dyn StateLookup>))
        .expect("restates");

    // And built with it from the start.
    let mut fresh = PaintBinder::new(specs(&layer), &paint, 14.0)
        .with_state(Some(hover as Arc<dyn StateLookup>));
    let mut again = 0;
    for road in &roads {
        again += 2;
        fresh.push(again, &paint, road).expect("pushes");
    }

    assert_eq!(
        patched.data(),
        fresh.data(),
        "the patched buffer is not what a rebuild produces"
    );
    assert_eq!(
        patched, fresh,
        "and the binders compare equal, which is the bytes"
    );
}

/// Restating back to no state returns the buffer it started with.
#[test]
fn restating_is_reversible() {
    let layer = layer();
    let paint = resolved(&layer);
    let road = Road { id: Some(7) };

    let mut binder = PaintBinder::new(specs(&layer), &paint, 14.0);
    binder.push(2, &paint, &road).expect("pushes");
    let plain = binder.data().to_vec();

    binder
        .restate(
            &paint,
            Some(States::with(7, "hover", Value::Bool(true)) as Arc<dyn StateLookup>),
        )
        .expect("restates");
    assert_ne!(binder.data(), plain.as_slice(), "the hover did nothing");

    binder.restate(&paint, None).expect("restates back");
    assert_eq!(
        binder.data(),
        plain.as_slice(),
        "unhovering did not undo it"
    );
}

/// Only the layer whose paint reads state records its features.
#[test]
fn the_index_is_only_for_a_layer_that_reads_state() {
    let paint_state = resolved(&layer());
    let mut reads = PaintBinder::new(specs(&layer()), &paint_state, 14.0);
    reads
        .push(2, &paint_state, &Road { id: Some(7) })
        .expect("pushes");
    assert_eq!(
        reads.index().len(),
        1,
        "a highlight layer records its features"
    );
    assert_eq!(reads.index()[0].id, Some(FeatureKey::Number(7)));
    assert_eq!(reads.index()[0].vertices, 0..2);

    // The same layer kind with ordinary data-driven paint records nothing.
    let plain: Layer = serde_json::from_str(
        r##"{"id": "roads", "type": "line", "source": "s", "source-layer": "roads",
             "paint": {"line-color": ["case", ["==", ["get", "kind"], "road"],
                                      "#ff0000", "#204060"], "line-width": 2}}"##,
    )
    .expect("the layer parses");
    let ordinary = resolve_paint(&plain).expect("the paint resolves");
    let mut ignores = PaintBinder::new(specs(&plain), &ordinary, 14.0);
    ignores
        .push(2, &ordinary, &Road { id: Some(7) })
        .expect("pushes");
    assert!(
        ignores.index().is_empty(),
        "a layer that cannot be affected by state recorded its features anyway"
    );
    // And restating it is a no-op rather than a rewrite.
    let before = ignores.data().to_vec();
    ignores
        .restate(
            &ordinary,
            Some(States::with(7, "hover", Value::Bool(true)) as Arc<dyn StateLookup>),
        )
        .expect("restates");
    assert_eq!(ignores.data(), before.as_slice());
}

/// A state-reading property that also reads geometry is not indexed, so it is not restated.
///
/// A recorded feature has its tags and not its coordinates -- a ring list per feature is the tile
/// over again -- so `["within", …]` cannot be answered a second time. Refusing to index is what
/// makes the difference visible as "the highlight waits for a rebuild" rather than as a feature
/// that silently became outside every polygon.
#[test]
fn a_state_expression_that_reads_geometry_is_not_indexed() {
    let layer: Layer = serde_json::from_str(
        r##"{"id": "roads", "type": "line", "source": "s", "source-layer": "roads",
             "paint": {"line-color": ["case",
                 ["all", ["==", ["feature-state", "hover"], true],
                         ["within", {"type": "Polygon", "coordinates":
                             [[[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0], [0.0, 0.0]]]}]],
                 "#ff0000", "#204060"], "line-width": 2}}"##,
    )
    .expect("the layer parses");
    let paint = resolve_paint(&layer).expect("the paint resolves");
    let mut binder = PaintBinder::new(specs(&layer), &paint, 14.0);
    binder
        .push(2, &paint, &Road { id: Some(7) })
        .expect("pushes");

    assert!(
        binder.index().is_empty(),
        "a state expression that reads geometry was indexed, so restating it would answer `within` \
         from a feature that has no coordinates"
    );
    let before = binder.data().to_vec();
    binder
        .restate(
            &paint,
            Some(States::with(7, "hover", Value::Bool(true)) as Arc<dyn StateLookup>),
        )
        .expect("restates");
    assert_eq!(binder.data(), before.as_slice(), "it was restated anyway");
}

/// Restating leaves the slots that do not read state exactly as the build wrote them.
///
/// The case that makes the skip load-bearing rather than tidy: a layer whose color reads state and
/// whose width reads *geometry*. The layer is indexed, because the state-reading slot is answerable
/// from a record -- but the width is not, and re-evaluating it from a feature with no coordinates
/// would write a different number than the tile was built with. Skipping it is what keeps a hover
/// from quietly changing a line's width.
#[test]
fn restating_does_not_rewrite_the_slots_it_did_not_change() {
    let layer: Layer = serde_json::from_str(
        r##"{"id": "roads", "type": "line", "source": "s", "source-layer": "roads",
             "paint": {
               "line-color": ["case", ["==", ["feature-state", "hover"], true],
                              "#ff0000", "#204060"],
               "line-width": ["case", ["within", {"type": "Polygon", "coordinates":
                       [[[0.0, 0.0], [8192.0, 0.0], [8192.0, 8192.0], [0.0, 8192.0],
                         [0.0, 0.0]]]}], 8, 2]
             }}"##,
    )
    .expect("the layer parses");
    let paint = resolve_paint(&layer).expect("the paint resolves");

    /// A feature inside the polygon above, so the width evaluates to the wide arm at build.
    struct Inside;
    impl Feature for Inside {
        fn property(&self, _key: &str) -> Option<Value> {
            None
        }
        fn geometry_type(&self) -> &str {
            "LineString"
        }
        fn id(&self) -> Option<Value> {
            Some(Value::Number(7.0))
        }
        fn geometry(&self) -> Option<tessella_style::expression::FeatureGeometry> {
            Some(tessella_style::expression::FeatureGeometry::Lines(vec![
                vec![[10.0, 10.0], [20.0, 20.0]],
            ]))
        }
    }

    let mut binder = PaintBinder::new(specs(&layer), &paint, 14.0);
    binder.push(2, &paint, &Inside).expect("pushes");
    assert_eq!(
        binder.index().len(),
        1,
        "the color is answerable from a record"
    );
    let built = binder.data().to_vec();

    binder
        .restate(
            &paint,
            Some(States::with(7, "hover", Value::Bool(true)) as Arc<dyn StateLookup>),
        )
        .expect("restates");

    // The color changed and the width did not. Compared as the two halves of the vertex rather
    // than as the whole, because the whole changing is what is expected.
    let stride = binder.stride();
    let width_slot = binder
        .slots()
        .iter()
        .find(|slot| slot.name == "line-width")
        .expect("a width slot");
    let before = &built[width_slot.offset..width_slot.offset + width_slot.width];
    let after = &binder.data()[width_slot.offset..width_slot.offset + width_slot.width];
    assert_eq!(
        before, after,
        "a hover rewrote the width, which is evaluated from a geometry the record does not have"
    );
    assert_ne!(built, binder.data(), "the hover did nothing at all");
    assert_eq!(binder.data().len(), stride * 2, "the buffer changed size");
}

/// What a feature's own id makes of a key, including what it refuses.
///
/// `FeatureKey::of` is the one place an id becomes a key, so what it drops is what a host can never
/// mark. The fractional and negative rows are the ones with no test before #361: a truncating
/// conversion would quietly route `1.5` onto feature `1` and mark the wrong one.
#[test]
fn an_id_becomes_a_key_or_nothing() {
    assert_eq!(
        FeatureKey::of(&Value::Number(7.0)),
        Some(FeatureKey::Number(7))
    );
    assert_eq!(
        FeatureKey::of(&Value::String("ribbon".into())),
        Some(FeatureKey::Text("ribbon".into()))
    );
    assert_eq!(
        FeatureKey::of(&Value::Number(1.5)),
        None,
        "a fractional id was truncated onto a neighboring feature"
    );
    assert_eq!(
        FeatureKey::of(&Value::Number(-1.0)),
        None,
        "a negative id was cast to a u64, which wraps to something enormous"
    );
    assert_eq!(FeatureKey::of(&Value::Null), None);
    assert_eq!(FeatureKey::of(&Value::Bool(true)), None);

    // And a number is not its own spelling, which is the distinction mbgl's string key loses.
    assert_ne!(
        FeatureKey::of(&Value::Number(7.0)),
        FeatureKey::of(&Value::String("7".into())),
        "feature 7 and feature \"7\" became one key"
    );
}
