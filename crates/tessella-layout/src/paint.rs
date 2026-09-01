//! Data-driven paint properties: one interleaved vertex buffer per layer per tile.
//!
//! Transcribed from mbgl's `PaintPropertyBinders` (`renderer/paint_property_binder.hpp`). A
//! property that varies per feature cannot be a uniform, so its evaluated value is written once
//! per *vertex* — every vertex a feature produced carries that feature's value — and all such
//! properties for a layer share one interleaved buffer.
//!
//! # Three things about this are not guessable
//!
//! Each was established against the golden dump's buffer hashes rather than reasoned out, and
//! each changes the bytes:
//!
//! - **The order is mbgl's property declaration order**, not attribute-id order and not the
//!   order a style writes them. The line layer's attribute ids are colour 2, width 7 and
//!   floorwidth 8, and their offsets run colour 0, floorwidth 8, width 12 — declaration order.
//! - **Colours are packed two channels to a float.** `packUint8Pair(255r, 255g)` and
//!   `packUint8Pair(255b, 255a)`, each widened to `f32`. A colour is eight bytes here, not
//!   sixteen, and the shader unpacks it.
//! - **Properties the shader does not read still take their slot.** The plain line shader does
//!   not bind floorwidth and the fill shader does not bind the outline colour, but both occupy
//!   space in the buffer, because the buffer is per *layer* and the two fill sublayers are two
//!   shaders reading one buffer at different offsets.
//!
//! # Why the value is per vertex and not per feature
//!
//! There is no indirection: the GPU reads an attribute per vertex, so a feature's value is
//! repeated across every vertex it produced. That makes the buffer's length track the geometry
//! exactly, and it makes the binder's correctness depend on the *vertex ranges* being right —
//! a feature whose range is off by one paints one vertex of its neighbour. So the ranges are
//! taken from the bucket after each feature is added rather than predicted from its geometry.
//!
//! # A property that varies with zoom as well
//!
//! It cannot be a uniform, because it varies per feature; and it cannot be one value in the
//! vertex, because the vertex is shared by views at different zooms. So the slot doubles and
//! carries the property's value at each end of the tile's zoom range — `[bucket zoom, bucket
//! zoom + 1]` — with a per-view `_t` uniform mixing between them at draw time.
//!
//! Two things about that are load-bearing. The ends are stored *grouped*, `[min…, max…]`, not
//! interleaved per component. And the range is the *bucket's* zoom, which is the tile's
//! overscaled zoom rather than the camera's — so the endpoints stay camera-free and shareable,
//! and the bucket's identity gains that zoom (see `tessella_tile::store::TileKey`).
//!
//! # What is not implemented
//!
//! Cross-faded properties — patterns, which are two attributes and a sprite lookup rather than
//! one slot. The slot-width rule returns nothing for them, so they take no space and are absent from
//! the buffer rather than present and wrong.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use tessella_style::expression::Feature;
use tessella_style::property::{
    DefaultValue, PropertyKind, PropertySpec, ResolvedProperty, as_color,
};
use tessella_style::{Binding, Value};

/// One data-driven property's place in the interleaved buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    /// Spec name.
    pub name: &'static str,
    /// Byte offset within a vertex.
    pub offset: usize,
    /// Bytes this property occupies.
    pub width: usize,
    /// Whether the shader is fed a min/max pair to mix. Not yet supplied.
    pub interpolated: bool,
}

/// Something the binder could not do.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BinderError {
    /// An expression did not evaluate for a feature.
    #[error("property `{name}`: {message}")]
    Evaluate {
        /// Which property.
        name: &'static str,
        /// What went wrong.
        message: alloc::string::String,
    },
    /// A property evaluated to something its slot cannot hold.
    #[error("property `{name}` evaluated to a value that is not a {expected}")]
    Type {
        /// Which property.
        name: &'static str,
        /// What the slot needs.
        expected: &'static str,
    },
}

/// The interleaved paint attribute buffer for one layer of one tile.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PaintBinder {
    slots: Vec<Slot>,
    stride: usize,
    zoom: f64,
    data: Vec<u8>,
    /// One feature's bytes, reused across features.
    ///
    /// [`Self::push`] runs once per feature and used to allocate this each time, along with a
    /// `Vec` per slot inside `encode` and another inside that. Three properties over a
    /// seventeen-thousand-feature layer is a hundred and fifty thousand allocations to build
    /// bytes that are copied out immediately — none of it expression evaluation, though it only
    /// happens when a property is data-driven and so was easy to read as evaluation cost.
    scratch: Vec<u8>,
}

/// Bytes a property of this kind occupies.
///
/// A colour is two floats because of the channel packing, everything numeric is one — and both
/// double when the property varies with zoom as well as per feature, because the slot then
/// carries the value at each end of the zoom range for the shader to mix between.
///
/// Anything else has no attribute form — a pattern is a sprite lookup and an enum is a uniform
/// — and contributes no slot.
fn slot_width(kind: PropertyKind, interpolated: bool) -> Option<usize> {
    let unit = match kind {
        PropertyKind::Color => 8,
        PropertyKind::Number => 4,
        PropertyKind::Boolean
        | PropertyKind::Enum
        | PropertyKind::Image
        | PropertyKind::NumberArray(_) => return None,
    };
    Some(if interpolated { unit * 2 } else { unit })
}

impl PaintBinder {
    /// Lays out the buffer for a layer's resolved paint properties.
    ///
    /// `specs` must be the layer's spec table *in mbgl's declaration order*: that order is the
    /// layout, so passing a sorted or filtered list silently moves every attribute after the
    /// first difference.
    /// `zoom` is the bucket's zoom — mbgl's `tileID.overscaledZ`, not the camera's. A property
    /// that also varies with zoom is stored as its value at `zoom` and at `zoom + 1`, and the
    /// shader mixes between them, so this is the one number that decides what a composite slot
    /// contains. Passing the camera's zoom instead would bake one view's position into geometry
    /// every view shares.
    #[must_use]
    pub fn new(
        specs: &[PropertySpec],
        resolved: &BTreeMap<&'static str, ResolvedProperty>,
        zoom: f64,
    ) -> Self {
        let mut slots = Vec::new();
        let mut stride = 0usize;
        for spec in specs {
            let Some(property) = resolved.get(spec.name) else {
                continue;
            };
            let Binding::Attribute { interpolated } = property.binding else {
                continue;
            };
            let Some(width) = slot_width(spec.kind, interpolated) else {
                continue;
            };
            // Every slot is a run of floats, so the buffer is four-byte aligned throughout and
            // mbgl's alignment step never inserts padding. Asserted rather than assumed,
            // because a future slot of another width would change the offsets silently.
            debug_assert_eq!(stride % 4, 0, "the buffer is four-byte aligned");
            slots.push(Slot {
                name: spec.name,
                offset: stride,
                width,
                interpolated,
            });
            stride += width;
        }
        Self {
            slots,
            stride,
            zoom,
            data: Vec::new(),
            scratch: Vec::new(),
        }
    }

    /// The bucket zoom this binder's composite endpoints were evaluated at.
    #[must_use]
    pub fn zoom(&self) -> f64 {
        self.zoom
    }

    /// The properties that take a slot, in buffer order.
    #[must_use]
    pub fn slots(&self) -> &[Slot] {
        &self.slots
    }

    /// Bytes per vertex. Zero when no property is data-driven, in which case there is no buffer.
    #[must_use]
    pub fn stride(&self) -> usize {
        self.stride
    }

    /// The buffer.
    #[must_use]
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Vertices written so far.
    #[must_use]
    pub fn vertex_count(&self) -> usize {
        // A stride of zero is the ordinary case, not an edge one: a layer whose paint is
        // entirely uniform binds no per-vertex data at all, so the buffer is empty and there is
        // nothing to divide.
        self.data.len().checked_div(self.stride).unwrap_or(0)
    }

    /// Writes one feature's values for every vertex up to `vertex_count`.
    ///
    /// Called after the feature's geometry has been added, with the bucket's new vertex count.
    /// Vertices already written are left alone, so a feature that produced no geometry writes
    /// nothing and a feature that produced ten vertices fills ten — which is why the count comes
    /// from the bucket rather than from the feature.
    ///
    /// # Errors
    ///
    /// [`BinderError`] when a property does not evaluate for this feature, evaluates to the
    /// wrong type, or is zoom-interpolated.
    pub fn push(
        &mut self,
        vertex_count: usize,
        resolved: &BTreeMap<&'static str, ResolvedProperty>,
        feature: &dyn Feature,
    ) -> Result<(), BinderError> {
        if self.stride == 0 || vertex_count <= self.vertex_count() {
            return Ok(());
        }

        self.scratch.clear();
        self.scratch.resize(self.stride, 0);
        for slot in &self.slots {
            let property = resolved
                .get(slot.name)
                .expect("a slot exists only for a resolved property");
            let at = |zoom: Option<f64>| {
                property
                    .expression
                    .evaluate(zoom, Some(feature))
                    .map_err(|error| BinderError::Evaluate {
                        name: slot.name,
                        message: alloc::format!("{error}"),
                    })
            };

            // A source-only property is evaluated with no zoom at all, not with the bucket's:
            // it does not read one, and offering it would let a mis-classified expression
            // silently start depending on it. A composite property is evaluated at both ends
            // of the range instead.
            let out = &mut self.scratch[slot.offset..slot.offset + slot.width];
            if slot.interpolated {
                let min = at(Some(self.zoom))?;
                let max = at(Some(self.zoom + 1.0))?;
                encode(slot, &min, Some(&max), out, &property.spec.default)?;
            } else {
                encode(slot, &at(None)?, None, out, &property.spec.default)?;
            }
        }

        let start = self.vertex_count();
        self.data.reserve((vertex_count - start) * self.stride);
        for _ in start..vertex_count {
            self.data.extend_from_slice(&self.scratch);
        }
        Ok(())
    }
}

/// Encodes a slot's value — or its two zoom endpoints — into that slot's bytes.
///
/// The two endpoints are written *grouped by end*, not interleaved per component: mbgl's
/// `zoomInterpolatedAttributeValue` lays out `[min…, max…]`, so a composite colour is the two
/// floats of the low end followed by the two of the high end. Interleaving them component-wise
/// produces a buffer of the right length that the shader reads as nonsense.
fn encode(
    slot: &Slot,
    value: &Value,
    upper: Option<&Value>,
    out: &mut [u8],
    fallback: &DefaultValue,
) -> Result<(), BinderError> {
    // Into a fixed buffer rather than a `Vec`: a slot is one float or two, this runs once per
    // slot per feature, and the values are copied straight out again.
    let floats = |value: &Value, into: &mut [f32; 2]| -> Result<usize, BinderError> {
        // The unit width, not the slot's: a composite slot is two of these.
        match slot.width / if slot.interpolated { 2 } else { 1 } {
            8 => {
                // The property's default when the feature's own value will not coerce, which is
                // what the oracle does: `PropertyExpression::evaluate` takes the default when the
                // expression fails *or* when `fromExpressionValue` cannot type the result. A
                // `["get", ...]` of a property the feature does not carry lands in the second
                // case, and it is ordinary rather than exceptional -- most features in a real
                // extract are missing most optional properties.
                let color = as_color(value)
                    .ok()
                    .or(match fallback {
                        DefaultValue::Color(color) => Some(*color),
                        _ => None,
                    })
                    .ok_or(BinderError::Type {
                        name: slot.name,
                        expected: "color",
                    })?;
                *into = pack_color(color);
                Ok(2)
            }
            4 => {
                let number = value
                    .as_number()
                    .or(match fallback {
                        DefaultValue::Number(number) => Some(*number),
                        _ => None,
                    })
                    .ok_or(BinderError::Type {
                        name: slot.name,
                        expected: "number",
                    })?;
                #[allow(clippy::cast_possible_truncation)]
                {
                    into[0] = number as f32;
                }
                Ok(1)
            }
            // `slot_width` produces only these two units.
            _ => unreachable!("a slot unit is four or eight bytes"),
        }
    };

    let mut unit = [0f32; 2];
    let mut written = 0usize;
    let mut put = |values: &[f32], written: &mut usize| {
        for float in values {
            out[*written..*written + 4].copy_from_slice(&float.to_le_bytes());
            *written += 4;
        }
    };

    let used = floats(value, &mut unit)?;
    put(&unit[..used], &mut written);
    if let Some(upper) = upper {
        let used = floats(upper, &mut unit)?;
        put(&unit[..used], &mut written);
    }

    debug_assert_eq!(written, slot.width, "slot `{}`", slot.name);
    Ok(())
}

/// Packs a colour into two floats, two channels each.
///
/// mbgl's `attributeValue(const Color&)`. The channels are already premultiplied and in 0..1,
/// so this scales back to 0..255 and truncates — `static_cast<uint16_t>`, not a round — which
/// matters because `224.0 / 255.0 * 255.0` is not exactly 224 in `f32` and a round would
/// disagree with the oracle on any channel that lands just under an integer.
fn pack_color(color: tessella_style::Color) -> [f32; 2] {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let byte = |channel: f32| (255.0f32 * channel) as u16;
    let pack = |a: f32, b: f32| f32::from(byte(a)) * 256.0 + f32::from(byte(b));
    [pack(color.r, color.g), pack(color.b, color.a)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    use tessella_style::Style;
    use tessella_style::property::paint_specs;

    struct TestFeature(&'static str, Value);

    impl Feature for TestFeature {
        fn property(&self, key: &str) -> Option<Value> {
            (key == self.0).then(|| self.1.clone())
        }
        fn geometry_type(&self) -> &str {
            "Polygon"
        }
    }

    fn layer(
        paint: &str,
    ) -> (
        alloc::vec::Vec<PropertySpec>,
        BTreeMap<&'static str, ResolvedProperty>,
    ) {
        let text = alloc::format!(
            r#"{{"version": 8, "sources": {{}}, "layers": [
                 {{"id": "l", "type": "fill", "source": "s", "paint": {paint}}}]}}"#
        );
        let style = Style::parse(&text).expect("style parses");
        let layer = style.layer("l").expect("l").clone();
        let resolved = tessella_style::property::resolve_paint(&layer).expect("resolves");
        (paint_specs(&layer.kind).unwrap_or(&[]).to_vec(), resolved)
    }

    fn kind_a() -> TestFeature {
        TestFeature("kind", Value::String(String::from("a")))
    }

    #[test]
    fn a_layer_of_constants_lays_out_nothing() {
        let (specs, resolved) = layer(r##"{"fill-color": "#ff0000", "fill-opacity": 0.5}"##);
        let binder = PaintBinder::new(&specs, &resolved, 13.0);
        assert_eq!(binder.stride(), 0);
        assert!(binder.slots().is_empty());
    }

    /// A zero-stride binder accepts pushes and stays empty, so a caller need not special-case
    /// the constant layer it is building.
    #[test]
    fn pushing_to_an_empty_binder_is_a_no_op() {
        let (specs, resolved) = layer(r##"{"fill-color": "#ff0000"}"##);
        let mut binder = PaintBinder::new(&specs, &resolved, 13.0);
        binder.push(10, &resolved, &kind_a()).expect("pushes");
        assert!(binder.data().is_empty());
        assert_eq!(binder.vertex_count(), 0);
    }

    /// A feature whose geometry was entirely clipped away contributes no vertices, so the
    /// binder must write nothing for it — and the *next* feature must still start where the
    /// previous one ended. Writing a row per feature rather than per vertex would shift every
    /// value after the first empty one onto the wrong geometry.
    #[test]
    fn a_feature_with_no_vertices_writes_nothing() {
        let (specs, resolved) =
            layer(r##"{"fill-color": ["match", ["get", "kind"], "a", "#ff0000", "#0000ff"]}"##);
        let mut binder = PaintBinder::new(&specs, &resolved, 13.0);

        binder.push(2, &resolved, &kind_a()).expect("first");
        // The second feature produced nothing: the count did not move.
        let b = TestFeature("kind", Value::String(String::from("b")));
        binder.push(2, &resolved, &b).expect("second");
        assert_eq!(binder.vertex_count(), 2);
        binder.push(4, &resolved, &b).expect("third");
        assert_eq!(binder.vertex_count(), 4);

        // Two red vertices then two blue, with nothing from the feature that drew nothing.
        // Chunked by the binder's own stride, which is 16 here and not 8: `fill-outline-color`
        // inherits the data-driven `fill-color` and takes a slot of its own.
        assert_eq!(binder.stride(), 16);
        let rows: alloc::vec::Vec<&[u8]> = binder.data().chunks_exact(binder.stride()).collect();
        assert_eq!(rows[0], rows[1]);
        assert_eq!(rows[2], rows[3]);
        assert_ne!(rows[0], rows[2]);
    }

    /// Colour channels are truncated, not rounded, because mbgl casts rather than rounds.
    ///
    /// `224/255 * 255` is not exactly 224 in `f32`. Whichever side of the integer it lands, a
    /// round and a truncate disagree, and the oracle does the truncate.
    #[test]
    fn colour_channels_truncate() {
        let (specs, resolved) =
            layer(r##"{"fill-color": ["match", ["get", "kind"], "a", "#e0d040", "#000000"]}"##);
        let mut binder = PaintBinder::new(&specs, &resolved, 13.0);
        binder.push(1, &resolved, &kind_a()).expect("pushes");

        let mut first = [0u8; 4];
        let mut second = [0u8; 4];
        first.copy_from_slice(&binder.data()[0..4]);
        second.copy_from_slice(&binder.data()[4..8]);
        // 0xe0 = 224, 0xd0 = 208, 0x40 = 64, alpha 255.
        assert_eq!(f32::from_le_bytes(first), 224.0 * 256.0 + 208.0);
        assert_eq!(f32::from_le_bytes(second), 64.0 * 256.0 + 255.0);
    }

    /// A value the slot cannot type takes the property's default, as the oracle does.
    ///
    /// `PropertyExpression::evaluate` falls back when the expression fails *or* when
    /// `fromExpressionValue` cannot type what it returned, and the second case is the ordinary
    /// one: `["get", "render_min_height"]` over an extract where most buildings do not carry
    /// that property returns null for most of them. Refusing it loses the feature, and because a
    /// tile builds as a unit, refusing it lost the whole tile -- which is how OpenFreeMap's
    /// `liberty` came to draw nothing at all rather than draw its buildings flat.
    #[test]
    fn a_wrong_typed_value_takes_the_default() {
        let (specs, resolved) =
            layer(r#"{"fill-opacity": ["match", ["get", "kind"], "a", 0.5, 0.9]}"#);
        let binder = PaintBinder::new(&specs, &resolved, 13.0);
        assert_eq!(binder.stride(), 4);

        let slot = Slot {
            name: "fill-opacity",
            offset: 0,
            width: 4,
            interpolated: false,
        };
        let mut out = [0u8; 4];
        encode(
            &slot,
            &Value::String(String::from("wide")),
            None,
            &mut out,
            &DefaultValue::Number(0.25),
        )
        .expect("a string falls back to the numeric default");
        assert!((f32::from_le_bytes(out) - 0.25).abs() < f32::EPSILON);
    }

    /// With no default of a usable type there is nothing to fall back to, and it is refused.
    ///
    /// The other half of the oracle's rule: the fallback is `defaultValue ? *defaultValue :
    /// finalDefaultValue`, so a property whose default is `None` has no number to reach for and
    /// the encoder has nothing to write but a lie.
    #[test]
    fn a_wrong_typed_value_with_no_default_is_still_refused() {
        let slot = Slot {
            name: "fill-opacity",
            offset: 0,
            width: 4,
            interpolated: false,
        };
        let mut out = [0u8; 4];
        assert!(matches!(
            encode(
                &slot,
                &Value::String(String::from("wide")),
                None,
                &mut out,
                &DefaultValue::None,
            ),
            Err(BinderError::Type { .. })
        ));
    }

    /// A zoom-interpolated property takes a double-width slot holding both endpoints.
    ///
    /// The two ends are grouped, `[min…, max…]`, not interleaved per component — mbgl's
    /// `zoomInterpolatedAttributeValue` — and the low end is the value at the bucket zoom.
    #[test]
    fn an_interpolated_property_carries_both_endpoints() {
        let (specs, resolved) = layer(
            r##"{"fill-opacity": ["interpolate", ["linear"], ["zoom"],
                 13, ["match", ["get", "kind"], "a", 0.1, 0.2],
                 15, ["match", ["get", "kind"], "a", 0.9, 0.8]]}"##,
        );
        let mut binder = PaintBinder::new(&specs, &resolved, 13.0);

        let slot = *binder.slots().first().expect("a slot");
        assert!(slot.interpolated, "the expression reads zoom and a feature");
        assert_eq!(slot.width, 8, "two floats, not one");
        assert_eq!(binder.stride(), 8);

        binder.push(1, &resolved, &kind_a()).expect("pushes");
        let min = f32::from_le_bytes(binder.data()[0..4].try_into().expect("four bytes"));
        let max = f32::from_le_bytes(binder.data()[4..8].try_into().expect("four bytes"));
        // The curve runs 0.1 at zoom 13 to 0.9 at 15, so a bucket at 13 spans [0.1, 0.5].
        assert!((min - 0.1).abs() < 1e-6, "{min}");
        assert!((max - 0.5).abs() < 1e-6, "{max}");
    }

    /// The bucket zoom moves both endpoints, which is why it is part of a bucket's identity.
    #[test]
    fn the_bucket_zoom_selects_the_range() {
        let (specs, resolved) = layer(
            r##"{"fill-opacity": ["interpolate", ["linear"], ["zoom"],
                 13, ["match", ["get", "kind"], "a", 0.1, 0.2],
                 15, ["match", ["get", "kind"], "a", 0.9, 0.8]]}"##,
        );
        let ends = |zoom: f64| {
            let mut binder = PaintBinder::new(&specs, &resolved, zoom);
            binder.push(1, &resolved, &kind_a()).expect("pushes");
            (
                f32::from_le_bytes(binder.data()[0..4].try_into().expect("four bytes")),
                f32::from_le_bytes(binder.data()[4..8].try_into().expect("four bytes")),
            )
        };
        let (a_min, a_max) = ends(13.0);
        let (b_min, b_max) = ends(14.0);
        assert!(a_max > a_min && b_max > b_min);
        assert!(b_min > a_min, "a later bucket starts higher up the curve");
        assert!(
            (b_min - a_max).abs() < 1e-6,
            "and starts where the earlier one ended"
        );
    }
}
