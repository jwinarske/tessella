//! Packing data-driven paint properties into the interleaved vertex buffer.
//!
//! # The layout, measured
//!
//! The golden dump's data-driven fill drawable says exactly what this has to produce:
//!
//! ```text
//! id=1 bind=1  dt=26 ddt=28  off=0  stride=20   fill-color
//! id=2 bind=2  dt=25 ddt=26  off=8  stride=20   fill-opacity
//! id=3 bind=-1 dt=26 ddt=255 off=12 stride=20   fill-outline-color
//! ```
//!
//! One interleaved buffer, every data-driven property in spec order, each contributing its
//! *supplied* width: a colour is two floats, a number one, doubled when the property is
//! zoom-interpolated. 8 + 4 + 8 = 20.
//!
//! # Supplied and declared differ on purpose
//!
//! `dt` is what the buffer holds and `ddt` is what the shader declares, and §2.2 says to bind
//! the declared type with the supplied offset and stride. A shader always declares the
//! interpolated width — `Float4` for a colour — because it has to handle a property that varies
//! with zoom. The binder supplies half that when the property varies per feature but not with
//! zoom, and the tweaker sets the mix factor to zero so the shader reads `.xy` and never touches
//! `.zw`.
//!
//! `fill-outline-color` shows the other half of the rule: the plain fill shader has no slot for
//! it, so it binds at `-1` with declared type `Invalid` and the consumer drops it. Its bytes are
//! still written into the buffer — that is why the stride is 20 rather than 12 — because the
//! same bucket feeds the outline shader, which does declare it.
//!
//! # Colours are packed, not stored
//!
//! A colour reaches the GPU as two floats, not four. Each float carries two 8-bit components:
//! `packUint8Pair(a, b) = a * 256 + b`, applied to `255 * component`. That cast truncates rather
//! than rounds, which would be a bug if `255 * (n / 255)` ever came out below `n` — it does not,
//! for any of the 256 values, in either f32 or f64, and there is a test that checks all of them
//! rather than the handful that happen to appear in this style.

use alloc::vec::Vec;

use tessella_capture_abi::AttributeDataType;
use tessella_style::property::{Binding, Color, PropertyKind, ResolvedProperty};

/// Packs two 8-bit values into one float's integer range, as mbgl does.
///
/// The result is exact: `a * 256 + b` for `a`, `b` in 0..=255 is at most 65535, well inside
/// what an f32 represents exactly.
#[must_use]
pub fn pack_u8_pair(a: u8, b: u8) -> f32 {
    f32::from(u16::from(a) * 256 + u16::from(b))
}

/// Packs a colour into the two floats the vertex buffer carries.
///
/// Components are scaled by 255 and truncated, which is what mbgl's `static_cast<uint16_t>`
/// does. Truncation is safe here — see the module note — but it is truncation, not rounding,
/// and rounding instead would differ for any component whose scaled value landed just below an
/// integer.
#[must_use]
pub fn pack_color(color: Color) -> [f32; 2] {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let scale = |component: f32| (255.0 * component) as u8;
    [
        pack_u8_pair(scale(color.r), scale(color.g)),
        pack_u8_pair(scale(color.b), scale(color.a)),
    ]
}

/// One data-driven property's place in the interleaved vertex.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundAttribute {
    /// Style property name.
    pub property: &'static str,
    /// Shader-side attribute id.
    pub attr_id: u32,
    /// Binding slot the shader declares, or `-1` when it declares none and the consumer must
    /// drop it (§2.2).
    pub binding: i32,
    /// The type the buffer supplies.
    pub supplied: AttributeDataType,
    /// The type the shader declares, or `Invalid` when it declares none.
    pub declared: AttributeDataType,
    /// Byte offset within the vertex.
    pub offset: u32,
}

/// The interleaved layout for one layer's data-driven properties.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VertexLayout {
    /// Attributes, in the order they occupy the vertex.
    pub attributes: Vec<BoundAttribute>,
    /// Bytes per vertex.
    pub stride: u32,
}

impl VertexLayout {
    /// True when nothing is data-driven, so no interleaved buffer is needed at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.attributes.is_empty()
    }
}

/// How many floats the buffer supplies for a property.
///
/// A colour is two, a number one, doubled when zoom-interpolated because the buffer then
/// carries a packed min/max pair for the shader to mix.
fn supplied_floats(kind: PropertyKind, interpolated: bool) -> Option<u32> {
    let base = match kind {
        PropertyKind::Color => 2,
        PropertyKind::Number => 1,
        // Only colours and numbers are data-driven-capable in a way that becomes a vertex
        // attribute. A pattern is cross-faded and occupies two attributes; it is not handled.
        _ => return None,
    };
    Some(if interpolated { base * 2 } else { base })
}

/// The attribute data type for a count of floats.
fn float_type(count: u32) -> AttributeDataType {
    match count {
        1 => AttributeDataType::Float,
        2 => AttributeDataType::Float2,
        3 => AttributeDataType::Float3,
        _ => AttributeDataType::Float4,
    }
}

/// Derives the shader attribute id name from a style property name.
///
/// `fill-color` becomes `idFillColorVertexAttribute`, which is the convention `shader_defines.hpp`
/// follows without exception for the properties that become one attribute.
///
/// Cross-faded properties break it: `fill-pattern` corresponds to *two* attributes,
/// `idFillPatternFromVertexAttribute` and `...To...`, because a pattern fades between zoom
/// levels. Those return `None` rather than being guessed at.
#[must_use]
pub fn attribute_id_name(property: &str) -> Option<alloc::string::String> {
    use alloc::string::String;

    if property.ends_with("-pattern") {
        return None;
    }
    let mut name = String::from("id");
    for word in property.split('-') {
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            name.extend(first.to_uppercase());
            name.push_str(chars.as_str());
        }
    }
    name.push_str("VertexAttribute");
    Some(name)
}

/// Builds the interleaved layout for a layer's data-driven properties.
///
/// `declared` resolves a property's attribute id to what the shader declares for it, which is
/// the generated table's job. A property the shader does not declare still occupies its bytes —
/// the same bucket may feed a second shader that does declare it — but binds at `-1`.
pub fn layout(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    ids: &alloc::collections::BTreeMap<alloc::string::String, u32>,
    declared: impl Fn(u32) -> Option<(i32, AttributeDataType)>,
) -> VertexLayout {
    let mut attributes = Vec::new();
    let mut offset = 0u32;

    // Spec order, which is what the map's key order gives and what the oracle's offsets follow.
    for (name, property) in paint {
        let Binding::Attribute { interpolated } = property.binding else {
            continue;
        };
        let Some(floats) = supplied_floats(property.spec.kind, interpolated) else {
            continue;
        };
        let Some(id_name) = attribute_id_name(name) else {
            continue;
        };
        let Some(&attr_id) = ids.get(&id_name) else {
            continue;
        };

        // A shader that declares no slot for this attribute yields -1 and Invalid, which is
        // the consumer's signal to drop it (§2.2).
        let (binding, declared_type) =
            declared(attr_id).unwrap_or((-1, AttributeDataType::Invalid));

        attributes.push(BoundAttribute {
            property: name,
            attr_id,
            binding,
            supplied: float_type(floats),
            declared: declared_type,
            offset,
        });
        offset += floats * 4;
    }

    VertexLayout {
        attributes,
        stride: offset,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// mbgl truncates rather than rounds when scaling a colour component, which is only safe if
    /// `255 * (n / 255)` never lands below `n`. Checked for every value rather than the few this
    /// style happens to use — one that rounded down would silently shift a colour by a step.
    #[test]
    fn scaling_a_colour_component_never_loses_a_step() {
        for n in 0..=255u16 {
            #[allow(clippy::cast_precision_loss)]
            let component = f32::from(n) / 255.0;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let scaled = (255.0 * component) as u16;
            assert_eq!(scaled, n, "255 * ({n}/255) truncated to {scaled}");
        }
    }

    #[test]
    fn packing_matches_mbgls_formula() {
        assert_eq!(pack_u8_pair(0, 0), 0.0);
        assert_eq!(pack_u8_pair(1, 0), 256.0);
        assert_eq!(pack_u8_pair(0, 1), 1.0);
        assert_eq!(pack_u8_pair(255, 255), 65535.0);

        // #2f6f4f opaque: r=47 g=111 b=79 a=255.
        let packed = pack_color(Color::parse("#2f6f4f").expect("a colour"));
        assert_eq!(packed[0], f32::from(47u16 * 256 + 111));
        assert_eq!(packed[1], f32::from(79u16 * 256 + 255));
    }

    /// The largest packed value is 65535, which an f32 holds exactly. If it did not, two
    /// distinct colours could pack to one float.
    #[test]
    fn every_packed_pair_is_exact_in_f32() {
        for a in [0u8, 1, 127, 128, 254, 255] {
            for b in [0u8, 1, 127, 128, 254, 255] {
                let packed = pack_u8_pair(a, b);
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let round_tripped = packed as u16;
                assert_eq!(round_tripped, u16::from(a) * 256 + u16::from(b));
            }
        }
    }

    fn ids() -> alloc::collections::BTreeMap<alloc::string::String, u32> {
        // The fill family's ids, as shader_defines.hpp orders them.
        [
            ("idFillPosVertexAttribute", 0u32),
            ("idFillColorVertexAttribute", 1),
            ("idFillOpacityVertexAttribute", 2),
            ("idFillOutlineColorVertexAttribute", 3),
        ]
        .into_iter()
        .map(|(name, id)| (alloc::string::String::from(name), id))
        .collect()
    }

    #[test]
    fn property_names_derive_their_attribute_ids() {
        assert_eq!(
            attribute_id_name("fill-color").as_deref(),
            Some("idFillColorVertexAttribute")
        );
        assert_eq!(
            attribute_id_name("fill-outline-color").as_deref(),
            Some("idFillOutlineColorVertexAttribute")
        );
        // Cross-faded properties map to two attributes, so the derivation refuses rather than
        // guessing which half is meant.
        assert_eq!(attribute_id_name("fill-pattern"), None);
    }

    /// The layout the oracle emits: colour at 0, opacity at 8, outline colour at 12, stride 20.
    #[test]
    fn the_layout_matches_the_oracle() {
        use tessella_style::Style;

        let style = Style::parse(include_str!(
            "../../tessella-style/tests/hermetic_style.json"
        ))
        .expect("style parses");
        let layer = style.layer("fill-datadriven").expect("the layer");
        let paint = tessella_style::property::resolve_paint(layer).expect("resolves");

        // What the plain fill shader declares: position, colour at 1, opacity at 2. Nothing
        // for outline colour.
        let declared = |attr_id: u32| match attr_id {
            1 => Some((1, AttributeDataType::Float4)),
            2 => Some((2, AttributeDataType::Float2)),
            _ => None,
        };

        let layout = layout(&paint, &ids(), declared);
        assert_eq!(layout.stride, 20, "8 + 4 + 8");

        let names: Vec<&str> = layout.attributes.iter().map(|a| a.property).collect();
        assert_eq!(names, ["fill-color", "fill-opacity", "fill-outline-color"]);

        let color = &layout.attributes[0];
        assert_eq!(color.attr_id, 1);
        assert_eq!(color.binding, 1);
        assert_eq!(color.offset, 0);
        assert_eq!(color.supplied, AttributeDataType::Float2);
        assert_eq!(color.declared, AttributeDataType::Float4);

        let opacity = &layout.attributes[1];
        assert_eq!(opacity.attr_id, 2);
        assert_eq!(opacity.binding, 2);
        assert_eq!(opacity.offset, 8);
        assert_eq!(opacity.supplied, AttributeDataType::Float);
        assert_eq!(opacity.declared, AttributeDataType::Float2);

        // The drop-undeclared-override rule, and the reason the stride is 20 rather than 12:
        // the bytes are written even though this shader drops them, because the outline shader
        // reads the same bucket.
        let outline = &layout.attributes[2];
        assert_eq!(outline.attr_id, 3);
        assert_eq!(outline.binding, -1);
        assert_eq!(outline.offset, 12);
        assert_eq!(outline.supplied, AttributeDataType::Float2);
        assert_eq!(outline.declared, AttributeDataType::Invalid);
    }

    /// A layer whose paint is all constant needs no interleaved buffer at all.
    #[test]
    fn a_constant_layer_has_an_empty_layout() {
        use tessella_style::Style;

        let style = Style::parse(include_str!(
            "../../tessella-style/tests/hermetic_style.json"
        ))
        .expect("style parses");
        let layer = style.layer("fill-constant").expect("the layer");
        let paint = tessella_style::property::resolve_paint(layer).expect("resolves");

        let layout = layout(&paint, &ids(), |_| None);
        assert!(layout.is_empty());
        assert_eq!(layout.stride, 0);
    }

    /// A zoom-interpolated property doubles its width: the buffer carries a packed min/max pair
    /// for the shader to mix, rather than one value.
    #[test]
    fn interpolation_doubles_the_supplied_width() {
        assert_eq!(supplied_floats(PropertyKind::Color, false), Some(2));
        assert_eq!(supplied_floats(PropertyKind::Color, true), Some(4));
        assert_eq!(supplied_floats(PropertyKind::Number, false), Some(1));
        assert_eq!(supplied_floats(PropertyKind::Number, true), Some(2));

        assert_eq!(float_type(1), AttributeDataType::Float);
        assert_eq!(float_type(2), AttributeDataType::Float2);
        assert_eq!(float_type(4), AttributeDataType::Float4);
    }
}
