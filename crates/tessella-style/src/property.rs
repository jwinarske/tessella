//! The typed property view: what a paint property is, and how it binds.
//!
//! This is where DR-11's classification cashes out. A property's dependency decides how it
//! reaches the GPU, and that decision is the one §2.2's whole attribute-descriptor contract
//! rests on:
//!
//! - **Constant or camera-only** → a uniform. One value serves every feature in the layer, and
//!   a camera-only expression is evaluated once per `(layer, zoom interval)` process-wide
//!   rather than per feature or per frame (§12.1).
//! - **Data-driven** → a vertex attribute. The value varies per feature, so it is evaluated at
//!   bucket build and written into the vertex stream.
//!
//! # Why a data-driven attribute is sometimes two values and sometimes one
//!
//! A shader always declares the zoom-interpolated width of a data-driven property, because it
//! has to handle a property that varies with zoom: fill's color is declared `Float4`, a packed
//! min/max pair mixed by a `color_t` uniform. But the binder only supplies both halves when the
//! property is *actually* interpolated. A `match` on a feature property is per-feature and
//! constant across zoom, so it supplies `Float2` and the tweaker sets `color_t = 0`, leaving
//! the shader reading `.xy` and never touching `.zw`.
//!
//! That is why the stream carries both a supplied type and a declared type, and why §2.2 says
//! to bind the declared one with the supplied offset and stride. [`Binding::Attribute`] records
//! which case a property is in, and it is the dependency that decides: interpolated exactly
//! when the expression reads the zoom as well as the feature.
//!
//! # Scope
//!
//! Background and fill only, which is R0 (§10). The tables are transcribed from
//! `src/mbgl/style/layers/*_layer_properties.hpp` in the pinned tree — including which
//! properties are data-driven-capable, which is a per-property fact rather than a per-type one
//! and not guessable.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::document::{Layer, LayerKind, PropertyValue};
use crate::expression::{Dependency, Expression};
use crate::value::Value;

/// A color, as the stream carries it.
///
/// Straight sRGB components in 0..1, not premultiplied and not linearized, with any layer
/// opacity travelling separately. That is not an assumption: the golden dump's fill layer
/// carries `#2f6f4f` as `0.184314, 0.435294, 0.309804, 1.0` — exactly `0x2f/255` and so on —
/// with the layer's `fill-opacity` of `0.8` as its own scalar beside it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    /// Red, 0..1.
    pub r: f32,
    /// Green, 0..1.
    pub g: f32,
    /// Blue, 0..1.
    pub b: f32,
    /// Alpha, 0..1.
    pub a: f32,
}

impl Color {
    /// Opaque black, which is the default for every color property in the spec.
    #[must_use]
    pub const fn black() -> Self {
        Self {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        }
    }

    /// Fully transparent.
    #[must_use]
    pub const fn transparent() -> Self {
        Self {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.0,
        }
    }

    /// Parses any CSS color the spec allows.
    ///
    /// # Errors
    ///
    /// [`PropertyError::Color`] when the string is not a color.
    pub fn parse(text: &str) -> Result<Self, PropertyError> {
        let parsed: csscolorparser::Color = text.parse().map_err(|_| PropertyError::Color {
            text: text.to_string(),
        })?;
        // mbgl stores colors *premultiplied*, and does the premultiply and the 0..255 normalize
        // in a single multiply: `channel * (alpha / 255)` over the integer channel. Dividing by
        // 255 and multiplying by alpha separately is the same real number and a different f32 —
        // `#2f6f4f`'s green comes out one ULP low — so the expression is copied rather than
        // rearranged, for the same reason the projection's is.
        //
        // Premultiplying matters beyond the last bit. A half-transparent red is stored as
        // `(0.5, 0, 0, 0.5)`, not `(1, 0, 0, 0.5)`, and a consumer blending the second as though
        // it were the first draws it at twice the intensity. The golden dump's colors are all
        // opaque, so this is a difference the oracle cannot show and the spec decides.
        let [r, g, b, _] = parsed.to_rgba8();
        let factor = parsed.a / 255.0;
        Ok(Self {
            r: f32::from(r) * factor,
            g: f32::from(g) * factor,
            b: f32::from(b) * factor,
            a: parsed.a,
        })
    }
}

/// What type a property holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertyKind {
    /// A CSS color.
    Color,
    /// A number.
    Number,
    /// A boolean.
    Boolean,
    /// One of a fixed set of names.
    Enum,
    /// A sprite image name.
    Image,
    /// A fixed-length array of numbers.
    NumberArray(usize),
}

/// A property's default, in a form that can live in a static table.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DefaultValue {
    /// A color.
    Color(Color),
    /// A number.
    Number(f64),
    /// A boolean.
    Boolean(bool),
    /// A name from the property's enum.
    Enum(&'static str),
    /// Two numbers, for translate-style properties.
    NumberPair(f64, f64),
    /// No value. The layer decides what absence means — `fill-outline-color` falls back to
    /// `fill-color`, which is layer logic rather than a default.
    None,
}

/// One property's definition.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PropertySpec {
    /// Spec name, as written in a style.
    pub name: &'static str,
    /// What it holds.
    pub kind: PropertyKind,
    /// What it is when a style does not set it.
    pub default: DefaultValue,
    /// Whether the spec allows this property to vary per feature.
    ///
    /// Per property, not per type: `fill-color` is data-driven-capable and `fill-translate` is
    /// not, though both are paint properties of the same layer. Transcribed from whether mbgl
    /// declares each as `DataDrivenPaintProperty` or plain `PaintProperty`.
    pub data_driven: bool,
}

/// How a property's value reaches the GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binding {
    /// One value for the whole layer, in a uniform buffer.
    ///
    /// Covers both constants and camera-only expressions: a camera-only value is the same for
    /// every feature at a given zoom, so it is a uniform that changes when the zoom interval
    /// does, not an attribute.
    Uniform,
    /// A value per feature, in the vertex stream.
    Attribute {
        /// Whether the attribute carries a packed min/max pair to be mixed by a `_t` uniform.
        ///
        /// True exactly when the expression reads the zoom as well as the feature. False when
        /// it varies per feature but not with zoom, in which case the binder supplies half the
        /// declared width and the tweaker sets the mix factor to zero (§2.2).
        interpolated: bool,
    },
}

/// A property that could not be resolved.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PropertyError {
    /// A color string that is not a color.
    #[error("`{text}` is not a color")]
    Color {
        /// What was written.
        text: String,
    },
    /// A value of the wrong type for its property.
    #[error("`{property}` expects {expected}, got {got}")]
    Type {
        /// Property name.
        property: String,
        /// What the spec says it holds.
        expected: &'static str,
        /// What the style wrote.
        got: &'static str,
    },
    /// A property varies per feature that the spec does not allow to.
    ///
    /// Worth its own error rather than being tolerated: a data-driven expression on a property
    /// with no attribute slot has nowhere to go, and evaluating it once and treating the result
    /// as a uniform would give every feature the first feature's value.
    #[error("`{property}` cannot be data-driven")]
    NotDataDriven {
        /// Property name.
        property: String,
    },
    /// The property's expression did not parse.
    #[error("`{property}`: {source}")]
    Expression {
        /// Property name.
        property: String,
        /// What went wrong.
        source: crate::expression::ParseError,
    },
}

/// A property resolved against its spec.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedProperty {
    /// What the property is.
    pub spec: PropertySpec,
    /// The compiled expression. A style that did not set the property gets its default as a
    /// constant, so every property has one and nothing downstream special-cases absence.
    pub expression: Expression,
    /// How it reaches the GPU.
    pub binding: Binding,
}

impl ResolvedProperty {
    /// The value, when it does not depend on anything.
    #[must_use]
    pub fn as_constant(&self) -> Option<Value> {
        self.expression.as_constant()
    }
}

const BACKGROUND_PAINT: &[PropertySpec] = &[
    PropertySpec {
        name: "background-color",
        kind: PropertyKind::Color,
        default: DefaultValue::Color(Color::black()),
        data_driven: false,
    },
    PropertySpec {
        name: "background-opacity",
        kind: PropertyKind::Number,
        default: DefaultValue::Number(1.0),
        data_driven: false,
    },
    PropertySpec {
        name: "background-pattern",
        kind: PropertyKind::Image,
        default: DefaultValue::None,
        data_driven: false,
    },
];

const FILL_PAINT: &[PropertySpec] = &[
    PropertySpec {
        name: "fill-antialias",
        kind: PropertyKind::Boolean,
        default: DefaultValue::Boolean(true),
        data_driven: false,
    },
    PropertySpec {
        name: "fill-color",
        kind: PropertyKind::Color,
        default: DefaultValue::Color(Color::black()),
        data_driven: true,
    },
    PropertySpec {
        name: "fill-opacity",
        kind: PropertyKind::Number,
        default: DefaultValue::Number(1.0),
        data_driven: true,
    },
    PropertySpec {
        // mbgl defaults this to a default-constructed Color, which is transparent rather than
        // black. The spec's "defaults to fill-color" is resolved by the fill layer at draw
        // time, not by this table.
        name: "fill-outline-color",
        kind: PropertyKind::Color,
        default: DefaultValue::Color(Color::transparent()),
        data_driven: true,
    },
    PropertySpec {
        name: "fill-pattern",
        kind: PropertyKind::Image,
        default: DefaultValue::None,
        data_driven: true,
    },
    PropertySpec {
        name: "fill-translate",
        kind: PropertyKind::NumberArray(2),
        default: DefaultValue::NumberPair(0.0, 0.0),
        data_driven: false,
    },
    PropertySpec {
        name: "fill-translate-anchor",
        kind: PropertyKind::Enum,
        default: DefaultValue::Enum("map"),
        data_driven: false,
    },
];

const FILL_LAYOUT: &[PropertySpec] = &[PropertySpec {
    name: "fill-sort-key",
    kind: PropertyKind::Number,
    default: DefaultValue::Number(0.0),
    data_driven: true,
}];

/// The paint properties a layer type accepts, or `None` for a type R0 does not implement.
#[must_use]
pub fn paint_specs(kind: &LayerKind) -> Option<&'static [PropertySpec]> {
    match kind {
        LayerKind::Background => Some(BACKGROUND_PAINT),
        LayerKind::Fill => Some(FILL_PAINT),
        _ => None,
    }
}

/// The layout properties a layer type accepts, or `None` for a type R0 does not implement.
#[must_use]
pub fn layout_specs(kind: &LayerKind) -> Option<&'static [PropertySpec]> {
    match kind {
        LayerKind::Background => Some(&[]),
        LayerKind::Fill => Some(FILL_LAYOUT),
        _ => None,
    }
}

/// Every paint property of a layer, resolved against its spec.
///
/// Properties the style did not set are present with their defaults, so nothing downstream has
/// to distinguish "unset" from "set to the default" — the spec does not, and a binder that did
/// would produce a different uniform for two styles that mean the same thing.
///
/// # Errors
///
/// [`PropertyError`] when a value has the wrong type, is not a color, fails to parse as an
/// expression, or is data-driven on a property that cannot be.
pub fn resolve_paint(
    layer: &Layer,
) -> Result<BTreeMap<&'static str, ResolvedProperty>, PropertyError> {
    let specs = paint_specs(&layer.kind).unwrap_or(&[]);
    let mut resolved = resolve(specs, &layer.paint)?;
    apply_layer_rules(layer, &mut resolved);
    Ok(resolved)
}

/// Applies the defaults that are layer logic rather than table entries.
///
/// The spec says `fill-outline-color` defaults to `fill-color`, and mbgl implements that in the
/// fill layer rather than in the property's default — which is why the table gives it
/// transparent. Copying the *resolved* fill-color across, expression and binding together, is
/// what makes it right: when `fill-color` is data-driven the outline is data-driven too, and
/// needs a vertex attribute rather than a uniform.
///
/// Found by the oracle rather than by reading the spec. The golden dump's data-driven fill
/// drawable carries `fill-outline-color` as an attribute at offset 12 even though the style
/// never mentions it, which is only explicable if it inherited the binding along with the
/// value. Treating it as its own constant default gave a stride of 12 where the oracle has 20.
fn apply_layer_rules(layer: &Layer, resolved: &mut BTreeMap<&'static str, ResolvedProperty>) {
    if layer.kind != LayerKind::Fill || layer.paint.contains_key("fill-outline-color") {
        return;
    }
    let Some(fill_color) = resolved.get("fill-color").cloned() else {
        return;
    };
    if let Some(outline) = resolved.get_mut("fill-outline-color") {
        // The spec entry belongs to `fill-outline-color`; only the value and how it binds are
        // inherited.
        outline.expression = fill_color.expression;
        outline.binding = fill_color.binding;
    }
}

/// Every layout property of a layer, resolved against its spec.
///
/// # Errors
///
/// As [`resolve_paint`].
pub fn resolve_layout(
    layer: &Layer,
) -> Result<BTreeMap<&'static str, ResolvedProperty>, PropertyError> {
    let specs = layout_specs(&layer.kind).unwrap_or(&[]);
    resolve(specs, &layer.layout)
}

fn resolve(
    specs: &'static [PropertySpec],
    written: &BTreeMap<String, PropertyValue>,
) -> Result<BTreeMap<&'static str, ResolvedProperty>, PropertyError> {
    let mut resolved = BTreeMap::new();
    for spec in specs {
        let expression = match written.get(spec.name) {
            Some(PropertyValue::Expression(expression)) => Expression::parse(expression.value())
                .map_err(|source| PropertyError::Expression {
                    property: spec.name.to_string(),
                    source,
                })?,
            Some(PropertyValue::Literal(value)) => {
                check_literal(spec, value)?;
                Expression::parse(value).map_err(|source| PropertyError::Expression {
                    property: spec.name.to_string(),
                    source,
                })?
            }
            None => Expression::parse(&default_value(spec)).map_err(|source| {
                PropertyError::Expression {
                    property: spec.name.to_string(),
                    source,
                }
            })?,
        };

        let dependency = expression.dependency();
        if dependency.needs_feature() && !spec.data_driven {
            return Err(PropertyError::NotDataDriven {
                property: spec.name.to_string(),
            });
        }

        let binding = if dependency.needs_feature() {
            Binding::Attribute {
                interpolated: dependency.needs_zoom(),
            }
        } else {
            Binding::Uniform
        };

        resolved.insert(
            spec.name,
            ResolvedProperty {
                spec: *spec,
                expression,
                binding,
            },
        );
    }
    Ok(resolved)
}

/// Checks a literal against its property's declared type.
///
/// Only literals are checked. An expression's result type is not known until it is evaluated,
/// and type-checking expressions is the spec's own separate machinery — worth having, and not
/// worth faking with a shallow guess here.
fn check_literal(spec: &PropertySpec, value: &Value) -> Result<(), PropertyError> {
    let ok = match spec.kind {
        // A color is written as a string, and whether it is a *valid* color is settled by
        // parsing it rather than by its JSON type.
        PropertyKind::Color => match value.as_str() {
            Some(text) => return Color::parse(text).map(|_| ()),
            None => false,
        },
        PropertyKind::Number => value.as_number().is_some(),
        PropertyKind::Boolean => value.as_bool().is_some(),
        PropertyKind::Enum | PropertyKind::Image => value.as_str().is_some(),
        PropertyKind::NumberArray(len) => value.as_array().is_some_and(|items| {
            items.len() == len && items.iter().all(|i| i.as_number().is_some())
        }),
    };

    if ok {
        Ok(())
    } else {
        Err(PropertyError::Type {
            property: spec.name.to_string(),
            expected: match spec.kind {
                PropertyKind::Color => "a color",
                PropertyKind::Number => "a number",
                PropertyKind::Boolean => "a boolean",
                PropertyKind::Enum => "a name",
                PropertyKind::Image => "an image name",
                PropertyKind::NumberArray(_) => "an array of numbers",
            },
            got: value.type_name(),
        })
    }
}

fn default_value(spec: &PropertySpec) -> Value {
    match spec.default {
        // A default color goes back through the same string path a style would take, so the
        // default and an explicitly written equivalent produce the same value rather than two
        // that differ in the last bit.
        DefaultValue::Color(color) => Value::String(alloc::format!(
            "rgba({},{},{},{})",
            (color.r * 255.0).round() as u8,
            (color.g * 255.0).round() as u8,
            (color.b * 255.0).round() as u8,
            color.a
        )),
        DefaultValue::Number(number) => Value::Number(number),
        DefaultValue::Boolean(flag) => Value::Bool(flag),
        DefaultValue::Enum(name) => Value::String(name.to_string()),
        DefaultValue::NumberPair(a, b) => {
            Value::Array(alloc::vec![Value::Number(a), Value::Number(b)])
        }
        DefaultValue::None => Value::Null,
    }
}

/// Resolves a property's value to a color.
///
/// # Errors
///
/// [`PropertyError::Color`] when the value is not a color string.
pub fn as_color(value: &Value) -> Result<Color, PropertyError> {
    match value.as_str() {
        Some(text) => Color::parse(text),
        None => Err(PropertyError::Type {
            property: String::new(),
            expected: "a color",
            got: value.type_name(),
        }),
    }
}

/// The names of a layer's data-driven properties, in spec order.
///
/// These are the ones that become vertex attributes, which is what a bucket builder needs to
/// know before it lays out a vertex.
#[must_use]
pub fn attribute_properties(
    resolved: &BTreeMap<&'static str, ResolvedProperty>,
) -> Vec<&'static str> {
    resolved
        .iter()
        .filter(|(_, property)| matches!(property.binding, Binding::Attribute { .. }))
        .map(|(name, _)| *name)
        .collect()
}

/// True when every property of a layer is a uniform.
///
/// A layer in this state needs no per-feature vertex attributes at all, which is the common
/// case and the cheap one.
#[must_use]
pub fn is_all_uniform(resolved: &BTreeMap<&'static str, ResolvedProperty>) -> bool {
    resolved
        .values()
        .all(|property| property.binding == Binding::Uniform)
}

/// What a resolved property depends on.
#[must_use]
pub fn dependency_of(property: &ResolvedProperty) -> Dependency {
    property.expression.dependency()
}
