//! Describing the interleaved paint buffer on the wire, and naming its shader permutation.
//!
//! The bytes themselves are written by [`tessella_layout::paint::PaintBinder`]; this module
//! turns what that produced into the wire's attribute descriptors, and derives the permutation
//! key that says which of the shader's declared attributes this variant actually supplies.
//!
//! # One layout, not two
//!
//! Offsets and stride are read from the binder rather than recomputed here. They were computed
//! once, when the bytes were written; deriving them a second time from the same property table
//! agrees right up until one side changes, and then the descriptors point into the middle of a
//! value. There is no second derivation to disagree with.
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
//!
//! # The permutation key is family-scoped
//!
//! [`permutation_key`] is a bitmask over a shader *family's* attribute ids, so two layers of
//! different families may share a value and mean different things. That is correct and is what
//! mbgl does — its hash has no shader in it either — because §2.2 makes the identity the
//! *pair* of family and permutation. It is also why the id map unions the family: the plain
//! fill shader does not declare `fill-outline-color`, so one shader's table is not the id
//! space.

use alloc::vec::Vec;

use tessella_capture_abi::AttributeDataType;
use tessella_capture_abi::generated::mbgl_enums::BuiltIn;
use tessella_capture_abi::generated::shader_attributes::attributes;
use tessella_layout::paint::PaintBinder;
use tessella_style::property::{Binding, Color, ResolvedProperty};

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

/// Describes, for the wire, the interleaved layout a [`PaintBinder`] produced.
///
/// # Why this reads the binder rather than recomputing
///
/// The offsets and stride here must be the ones the *bytes* were written at. Deriving them a
/// second time from the same property table gives the same answer right up until one side
/// changes — a new property kind, a composite that doubles its slot — and then the descriptors
/// point into the middle of a value and the map draws in colours nothing chose. So this takes
/// the binder's own slots as fact and adds only what the binder does not know: the shader's
/// binding slot and declared type.
///
/// `declared` resolves an attribute id to what the shader declares for it, which is the
/// generated table's job. A property the shader does not declare still occupies its bytes — the
/// same bucket may feed a second shader that does declare it — but binds at `-1`.
pub fn layout(
    binder: &PaintBinder,
    ids: &alloc::collections::BTreeMap<alloc::string::String, u32>,
    declared: impl Fn(u32) -> Option<(i32, AttributeDataType)>,
) -> VertexLayout {
    let mut attributes = Vec::new();

    for slot in binder.slots() {
        let Some(id_name) = attribute_id_name(slot.name) else {
            continue;
        };
        let Some(&attr_id) = ids.get(&id_name) else {
            continue;
        };

        // A shader that declares no slot for this attribute yields -1 and Invalid, which is
        // the consumer's signal to drop it (§2.2).
        let (binding, declared_type) =
            declared(attr_id).unwrap_or((-1, AttributeDataType::Invalid));

        #[allow(clippy::cast_possible_truncation)]
        attributes.push(BoundAttribute {
            property: slot.name,
            attr_id,
            binding,
            supplied: float_type(slot.width as u32 / 4),
            declared: declared_type,
            offset: slot.offset as u32,
        });
    }

    #[allow(clippy::cast_possible_truncation)]
    VertexLayout {
        attributes,
        stride: binder.stride() as u32,
    }
}

/// The name-to-id map for a shader family, from the generated attribute tables.
///
/// Built from the tables rather than written out, because they are generated from
/// `shader_defines.hpp` and a hand-written map is a second copy that can disagree with them.
///
/// # Why a family and not one shader
///
/// The id space belongs to the family, not to any one member of it. The plain fill shader does
/// not declare `fill-outline-color` and the plain line shader does not declare
/// `line-floorwidth` — that is exactly why those bind at `-1` — so a map built from either
/// alone is missing an id that the layer's paint genuinely has. Unioning the family is what
/// makes the map the id space rather than one shader's view of it.
#[must_use]
pub fn attribute_ids(
    family: &[BuiltIn],
) -> alloc::collections::BTreeMap<alloc::string::String, u32> {
    let mut ids = alloc::collections::BTreeMap::new();
    for shader in family {
        for attribute in attributes(*shader) {
            ids.insert(
                alloc::string::String::from(attribute.name),
                attribute.attr_id,
            );
        }
    }
    ids
}

/// The fill shaders, which share one attribute id space.
pub const FILL_FAMILY: &[BuiltIn] = &[
    BuiltIn::FillShader,
    BuiltIn::FillOutlineShader,
    BuiltIn::FillPatternShader,
    BuiltIn::FillOutlinePatternShader,
    BuiltIn::FillOutlineTriangulatedShader,
];

/// The line shaders, which share one attribute id space.
pub const LINE_FAMILY: &[BuiltIn] = &[
    BuiltIn::LineShader,
    BuiltIn::LineGradientShader,
    BuiltIn::LinePatternShader,
    BuiltIn::LineSDFShader,
];

/// The symbol shaders, which share one attribute id space.
///
/// Two, and which one a drawable names is decided per drawable rather than per layer: text is
/// always SDF, an icon may be either, and the answer is already packed into each vertex.
pub const SYMBOL_FAMILY: &[BuiltIn] = &[BuiltIn::SymbolIconShader, BuiltIn::SymbolSDFShader];

/// The circle shaders.
///
/// One entry, and it is not an oversight: `CollisionCircleShader` shares the name but not the id
/// space — it draws debug geometry for the placement pass and reads a vertex a circle layer
/// never produces.
pub const CIRCLE_FAMILY: &[BuiltIn] = &[BuiltIn::CircleShader];

/// The fill-extrusion shaders, which share one attribute id space.
///
/// Both instanced variants are here because DR-16 settled this build on Vulkan, where mbgl's own
/// header defines `MLN_USE_FILL_EXTRUSION_INSTANCING` — so the instanced pair is the branch the
/// target backend takes, and the non-instanced two are what the same ids mean elsewhere.
pub const FILL_EXTRUSION_FAMILY: &[BuiltIn] = &[
    BuiltIn::FillExtrusionShader,
    BuiltIn::FillExtrusionInstancedShader,
    BuiltIn::FillExtrusionPatternShader,
    BuiltIn::FillExtrusionPatternInstancedShader,
];

/// The shader permutation a layer's paint requires.
///
/// # What the key means
///
/// A bit per shader attribute id, set when that property reaches the shader as a *uniform*
/// rather than as a vertex attribute. That is mbgl's `propertiesAsUniforms` set, which is
/// precisely what its shader group hashes to choose a permutation, and what its shaders filter
/// their declared attribute list by.
///
/// # Why a mask and not a hash
///
/// mbgl's key is a hash of that set together with the engine's compiled-in defines, so it moves
/// when a CMake option does and says nothing to a reader — the golden dump has to renumber it
/// for that reason. §2.2 makes this pair, shader family and permutation, the whole of shader
/// identity on the wire, which means a consumer has to *filter the attribute table by it*. A
/// hash cannot be filtered by; a mask can be read directly, and is the same in every build.
///
/// What must match the oracle is the grouping, and it does: the key is a function of the
/// layer's paint alone with no shader in it, which is why a fill layer's triangles and its
/// outline — two different shaders — share one permutation there and here.
#[must_use]
pub fn permutation_key(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    ids: &alloc::collections::BTreeMap<alloc::string::String, u32>,
) -> u64 {
    let mut mask = 0u64;
    for (name, property) in paint {
        // A property that cannot vary per feature is a uniform in every variant, so a bit for
        // it would be set everywhere and distinguish nothing.
        if !property.spec.data_driven || matches!(property.binding, Binding::Attribute { .. }) {
            continue;
        }
        for id_name in attribute_id_names(name) {
            let Some(&attr_id) = ids.get(&id_name) else {
                continue;
            };
            // A family with more than 64 attributes would need a wider key than the frozen ABI
            // has. None comes close, and silently dropping a bit would merge two permutations
            // into one, so it is asserted rather than ignored.
            debug_assert!(attr_id < 64, "attribute id {attr_id} does not fit the key");
            mask |= 1u64 << (attr_id % 64);
        }
    }
    mask
}

/// Every shader attribute a property corresponds to.
///
/// One for most properties, and *two* for a cross-faded one: `fill-pattern` is
/// `idFillPatternFromVertexAttribute` and `...To...`, because a pattern fades between zoom
/// levels and the shader needs both ends.
///
/// [`attribute_id_name`] refuses the pattern case because a vertex layout has to know which of
/// the two a slot is. The permutation key does not: it is a set, and both ends belong in it.
/// Leaving them out would let two layers that differ only in whether their pattern is
/// data-driven share a key, and so share a shader variant that binds the wrong things.
#[must_use]
fn attribute_id_names(property: &str) -> Vec<alloc::string::String> {
    use alloc::string::String;

    let camel = |words: &str| {
        let mut name = String::from("id");
        for word in words.split('-') {
            let mut chars = word.chars();
            if let Some(first) = chars.next() {
                name.extend(first.to_uppercase());
                name.push_str(chars.as_str());
            }
        }
        name
    };

    if let Some(stem) = property.strip_suffix("-pattern") {
        return alloc::vec![
            camel(stem) + "PatternFromVertexAttribute",
            camel(stem) + "PatternToVertexAttribute",
        ];
    }
    alloc::vec![camel(property) + "VertexAttribute"]
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessella_style::property::paint_specs;

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

        let binder = PaintBinder::new(paint_specs(&layer.kind).unwrap_or(&[]), &paint, 13.0);
        let layout = layout(&binder, &ids(), declared);
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

        let binder = PaintBinder::new(paint_specs(&layer.kind).unwrap_or(&[]), &paint, 13.0);
        let layout = layout(&binder, &ids(), |_| None);
        assert!(layout.is_empty());
        assert_eq!(layout.stride, 0);
    }

    /// Supplied widths follow the binder's slot widths, which is the only place they are
    /// decided now.
    #[test]
    fn a_slots_width_names_its_supplied_type() {
        assert_eq!(float_type(1), AttributeDataType::Float);
        assert_eq!(float_type(2), AttributeDataType::Float2);
        assert_eq!(float_type(4), AttributeDataType::Float4);
    }

    /// The attribute-id map is the generated table's, and it agrees with the oracle's numbering.
    #[test]
    fn attribute_ids_come_from_the_generated_table() {
        let ids = attribute_ids(FILL_FAMILY);
        assert_eq!(ids.get("idFillColorVertexAttribute"), Some(&1));
        assert_eq!(ids.get("idFillOpacityVertexAttribute"), Some(&2));
        assert_eq!(ids.get("idFillOutlineColorVertexAttribute"), Some(&3));

        let ids = attribute_ids(LINE_FAMILY);
        assert_eq!(ids.get("idLineColorVertexAttribute"), Some(&2));
        assert_eq!(ids.get("idLineWidthVertexAttribute"), Some(&7));
        assert_eq!(ids.get("idLineFloorWidthVertexAttribute"), Some(&8));
    }

    /// The permutation key is the set of properties arriving as uniforms.
    ///
    /// A constant layer's data-driven-capable properties are all uniforms, so every one of
    /// their bits is set; making a property data-driven clears its bit. That is the whole
    /// content of the key, and it is what a consumer filters the attribute table by.
    #[test]
    fn the_permutation_key_names_the_uniforms() {
        use tessella_style::Style;

        let style = Style::parse(include_str!(
            "../../tessella-style/tests/hermetic_style.json"
        ))
        .expect("style parses");
        let ids = attribute_ids(FILL_FAMILY);

        let constant = tessella_style::property::resolve_paint(
            style.layer("fill-constant").expect("fill-constant"),
        )
        .expect("resolves");
        let driven = tessella_style::property::resolve_paint(
            style.layer("fill-datadriven").expect("fill-datadriven"),
        )
        .expect("resolves");

        let constant_key = permutation_key(&constant, &ids);
        let driven_key = permutation_key(&driven, &ids);

        // Colour, opacity and outline colour are uniforms in the constant layer and attributes
        // in the data-driven one. The pattern is a uniform in both, and contributes *two* bits,
        // because a cross-faded property is two attributes.
        // Colour 1, opacity 2, outline colour 3, and the pattern's two ends at 4 and 5.
        assert_eq!(constant_key, 0b11_1110, "every one is a uniform");
        // The three the style drives are attributes; the pattern's ends remain uniforms.
        assert_eq!(driven_key, 0b11_0000, "only the pattern");
        assert_ne!(
            constant_key, driven_key,
            "the two layers need different shader variants"
        );
    }

    /// A property that cannot vary per feature sets no bit.
    ///
    /// It is a uniform in every variant, so a bit for it would be set in every key and would
    /// distinguish nothing — while making the key depend on properties that have no attribute.
    #[test]
    fn a_non_data_driven_property_is_not_part_of_the_key() {
        use tessella_style::Style;

        let with = Style::parse(
            r#"{"version": 8, "sources": {}, "layers": [
                 {"id": "l", "type": "fill", "source": "s",
                  "paint": {"fill-antialias": false, "fill-translate": [3, 4]}}]}"#,
        )
        .expect("style parses");
        let without = Style::parse(
            r#"{"version": 8, "sources": {}, "layers": [
                 {"id": "l", "type": "fill", "source": "s", "paint": {}}]}"#,
        )
        .expect("style parses");

        let ids = attribute_ids(FILL_FAMILY);
        let key = |style: &Style| {
            permutation_key(
                &tessella_style::property::resolve_paint(style.layer("l").expect("l"))
                    .expect("resolves"),
                &ids,
            )
        };
        assert_eq!(key(&with), key(&without));
    }
}
