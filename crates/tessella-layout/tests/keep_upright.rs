// SPDX-License-Identifier: BSD-2-Clause
//! `text-keep-upright` and `icon-keep-upright` reach the layout, with mbgl's asymmetric defaults.
//!
//! # What the oracle does
//!
//! A line-placed label whose road runs right to left reads backwards, so mbgl walks it the other
//! way instead. `placement.cpp` passes the property into `reprojectLineLabels`:
//!
//! ```cpp
//! const bool keepUpright = layout.get<style::TextKeepUpright>();
//! reprojectLineLabels(..., keepUpright, ...);
//! ```
//!
//! **The two defaults differ**, in `symbol_layer_properties.hpp`: `text-keep-upright` is `true`
//! and `icon-keep-upright` is `false`. This build read neither -- `LineOffsets::default()` says
//! `keep_upright: true` and nothing overrode it -- so the text half happened to match at its
//! default and a style asking for `false` was ignored.
//!
//! # What it was worth
//!
//! Four roads, two drawn west to east and two east to west, labelled along the line. Measured
//! with `tools/parity/parity.sh` at 51.509/-0.11 z14 over 600x600:
//!
//! | style | gross |
//! |---|---|
//! | default (`true`) | 0 |
//! | `text-keep-upright: false`, before | **957** of 360,000 (0.266%) |
//! | `text-keep-upright: false`, after | 0 |
//!
//! Half the labels: the two backwards roads, flipped when the style had asked for them not to be.
//! No scene in the parity sweep sets the property, which is why the sweep never saw it.
//!
//! # The icon half is carried but not measured
//!
//! `icon-keep-upright`'s default is the other way round, so this build was wrong about it too --
//! and unobservably. The flip test compares the first glyph's screen x against the last, and a
//! single-quad icon makes those the same point, so neither renderer can flip one. Confirmed:
//! `icon-keep-upright: false` moves not one pixel of `mbgl-render`'s output. Carried because the
//! property exists and the default differs, not because a scene has been found where it shows.

use tessella_layout::symbol_layout::SymbolLayout;
use tessella_style::Layer;

/// A symbol layer with `layout` spliced in.
fn layer_with(layout: &str) -> Layer {
    let text = format!(
        r#"{{"id": "labels", "type": "symbol", "source": "s",
             "layout": {{"text-field": ["get", "name"], "text-font": ["Noto Sans Regular"],
                         "icon-image": "dot", "symbol-placement": "line"{layout}}}}}"#
    );
    serde_json::from_str(&text).expect("a layer")
}

/// The defaults are mbgl's, and they are not the same for the two halves.
///
/// The asymmetry is the whole reason a single shared default was wrong: whichever value it took,
/// one of the two halves disagreed with the oracle.
#[test]
fn the_defaults_are_mbgls_asymmetric_pair() {
    let layout = SymbolLayout::new(&layer_with(""), 15.0, 1.0);
    assert!(
        layout.text_keep_upright,
        "text-keep-upright defaults to true in mbgl"
    );
    assert!(
        !layout.icon_keep_upright,
        "icon-keep-upright defaults to false in mbgl, where the text one is true"
    );
}

/// An explicit value is honored, both ways and for both halves.
#[test]
fn an_explicit_value_reaches_the_layout() {
    let off = SymbolLayout::new(
        &layer_with(r#", "text-keep-upright": false, "icon-keep-upright": false"#),
        15.0,
        1.0,
    );
    assert!(
        !off.text_keep_upright,
        "a style asking not to flip its labels was ignored -- the 957-pixel defect"
    );
    assert!(!off.icon_keep_upright);

    let on = SymbolLayout::new(
        &layer_with(r#", "text-keep-upright": true, "icon-keep-upright": true"#),
        15.0,
        1.0,
    );
    assert!(on.text_keep_upright);
    assert!(
        on.icon_keep_upright,
        "an icon layer asking to flip should, whatever the default says"
    );
}

/// The flag is read as a layout property, so a non-boolean falls back rather than panicking.
///
/// `layout_value` types the result and `as_bool` rejects anything else, which is the same path
/// every other flag in `placement_rules` takes.
#[test]
fn a_nonsense_value_takes_the_default() {
    let layout = SymbolLayout::new(&layer_with(r#", "text-keep-upright": "yes""#), 15.0, 1.0);
    assert!(
        layout.text_keep_upright,
        "a string where a boolean belongs should leave the default standing"
    );
}
