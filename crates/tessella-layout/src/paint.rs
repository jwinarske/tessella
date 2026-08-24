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
//! # What is not implemented
//!
//! Zoom-interpolated (`composite`) functions, which supply two values per property to be mixed
//! by a `_t` uniform, and would double each slot's width. [`Slot::interpolated`] records where
//! that applies; [`PaintBinder::push`] refuses rather than silently writing half a value,
//! because half a composite attribute is a plausible-looking buffer that draws wrong colours.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use tessella_style::expression::Feature;
use tessella_style::property::{PropertyKind, PropertySpec, ResolvedProperty, as_color};
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
    /// A zoom-interpolated property, which supplies two values per vertex.
    #[error("property `{name}` is zoom-interpolated, which is not implemented")]
    Interpolated {
        /// Which property.
        name: &'static str,
    },
}

/// The interleaved paint attribute buffer for one layer of one tile.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PaintBinder {
    slots: Vec<Slot>,
    stride: usize,
    data: Vec<u8>,
}

/// Bytes a property of this kind occupies when it varies per feature but not with zoom.
///
/// A colour is two floats because of the channel packing, everything numeric is one. Anything
/// else has no attribute form — a pattern is a sprite lookup and an enum is a uniform — and
/// contributes no slot.
fn slot_width(kind: PropertyKind) -> Option<usize> {
    match kind {
        PropertyKind::Color => Some(8),
        PropertyKind::Number => Some(4),
        PropertyKind::Boolean
        | PropertyKind::Enum
        | PropertyKind::Image
        | PropertyKind::NumberArray(_) => None,
    }
}

impl PaintBinder {
    /// Lays out the buffer for a layer's resolved paint properties.
    ///
    /// `specs` must be the layer's spec table *in mbgl's declaration order*: that order is the
    /// layout, so passing a sorted or filtered list silently moves every attribute after the
    /// first difference.
    #[must_use]
    pub fn new(
        specs: &[PropertySpec],
        resolved: &BTreeMap<&'static str, ResolvedProperty>,
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
            let Some(width) = slot_width(spec.kind) else {
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
            data: Vec::new(),
        }
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
        if self.stride == 0 {
            0
        } else {
            self.data.len() / self.stride
        }
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

        let mut vertex = alloc::vec![0u8; self.stride];
        for slot in &self.slots {
            if slot.interpolated {
                return Err(BinderError::Interpolated { name: slot.name });
            }
            let property = resolved
                .get(slot.name)
                .expect("a slot exists only for a resolved property");
            let value = property
                .expression
                .evaluate(None, Some(feature))
                .map_err(|error| BinderError::Evaluate {
                    name: slot.name,
                    message: alloc::format!("{error}"),
                })?;
            let bytes = encode(slot, &value)?;
            vertex[slot.offset..slot.offset + slot.width].copy_from_slice(&bytes);
        }

        let start = self.vertex_count();
        self.data.reserve((vertex_count - start) * self.stride);
        for _ in start..vertex_count {
            self.data.extend_from_slice(&vertex);
        }
        Ok(())
    }
}

/// Encodes one evaluated value into its slot's bytes.
fn encode(slot: &Slot, value: &Value) -> Result<Vec<u8>, BinderError> {
    match slot.width {
        8 => {
            let color = as_color(value).map_err(|_| BinderError::Type {
                name: slot.name,
                expected: "color",
            })?;
            let packed = pack_color(color);
            let mut bytes = Vec::with_capacity(8);
            bytes.extend_from_slice(&packed[0].to_le_bytes());
            bytes.extend_from_slice(&packed[1].to_le_bytes());
            Ok(bytes)
        }
        4 => {
            let number = value.as_number().ok_or(BinderError::Type {
                name: slot.name,
                expected: "number",
            })?;
            #[allow(clippy::cast_possible_truncation)]
            Ok((number as f32).to_le_bytes().to_vec())
        }
        // `slot_width` produces only 4 and 8.
        _ => unreachable!("a slot is four or eight bytes"),
    }
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
        let binder = PaintBinder::new(&specs, &resolved);
        assert_eq!(binder.stride(), 0);
        assert!(binder.slots().is_empty());
    }

    /// A zero-stride binder accepts pushes and stays empty, so a caller need not special-case
    /// the constant layer it is building.
    #[test]
    fn pushing_to_an_empty_binder_is_a_no_op() {
        let (specs, resolved) = layer(r##"{"fill-color": "#ff0000"}"##);
        let mut binder = PaintBinder::new(&specs, &resolved);
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
        let mut binder = PaintBinder::new(&specs, &resolved);

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
        let mut binder = PaintBinder::new(&specs, &resolved);
        binder.push(1, &resolved, &kind_a()).expect("pushes");

        let mut first = [0u8; 4];
        let mut second = [0u8; 4];
        first.copy_from_slice(&binder.data()[0..4]);
        second.copy_from_slice(&binder.data()[4..8]);
        // 0xe0 = 224, 0xd0 = 208, 0x40 = 64, alpha 255.
        assert_eq!(f32::from_le_bytes(first), 224.0 * 256.0 + 208.0);
        assert_eq!(f32::from_le_bytes(second), 64.0 * 256.0 + 255.0);
    }

    /// A property that evaluates to the wrong type is refused rather than written as zero.
    #[test]
    fn a_wrong_typed_value_is_refused() {
        let (specs, resolved) =
            layer(r#"{"fill-opacity": ["match", ["get", "kind"], "a", 0.5, 0.9]}"#);
        let binder = PaintBinder::new(&specs, &resolved);
        assert_eq!(binder.stride(), 4);

        // A feature the match cannot resolve falls to the fallback, so force the failure at the
        // encoder instead: a slot fed a string.
        let slot = Slot {
            name: "fill-opacity",
            offset: 0,
            width: 4,
            interpolated: false,
        };
        assert!(matches!(
            encode(&slot, &Value::String(String::from("wide"))),
            Err(BinderError::Type { .. })
        ));
    }

    /// A zoom-interpolated property is refused, not half-written.
    ///
    /// Half a composite attribute is a buffer of the right length holding the min value where
    /// the shader expects a min/max pair, which draws plausible-but-wrong colours rather than
    /// failing.
    #[test]
    fn an_interpolated_property_is_refused() {
        let (specs, resolved) = layer(
            r#"{"fill-opacity": ["interpolate", ["linear"], ["zoom"],
                 0, ["match", ["get", "kind"], "a", 0.1, 0.2],
                 10, ["match", ["get", "kind"], "a", 0.8, 0.9]]}"#,
        );
        let binder = PaintBinder::new(&specs, &resolved);
        let slot = binder
            .slots()
            .first()
            .expect("a composite property takes a slot");
        assert!(slot.interpolated, "the expression reads zoom and a feature");

        let mut binder = binder;
        assert!(matches!(
            binder.push(1, &resolved, &kind_a()),
            Err(BinderError::Interpolated { .. })
        ));
    }
}
