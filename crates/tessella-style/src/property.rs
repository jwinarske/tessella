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
use crate::expression::{self, Dependency, Expression};
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

/// Line paint properties, in mbgl's `LinePaintProperties` declaration order.
///
/// The order is load-bearing, not cosmetic: it is the order the data-driven properties are
/// packed into the interleaved paint attribute buffer, so a table sorted differently produces a
/// buffer the shader reads at the wrong offsets. It happens to coincide with alphabetical
/// order here, which is why it must be stated rather than relied on.
///
/// `line-floorwidth` is in the table and is not in the style spec. mbgl carries it as a real
/// paint property that mirrors `line-width` — `setLineWidth` assigns both — so it takes a slot
/// in the buffer whenever the width is data-driven, and the golden dump shows it at offset 8 of
/// a stride-16 line vertex. Omitting it gives a stride of 12 and every attribute after it
/// misread.
const LINE_PAINT: &[PropertySpec] = &[
    PropertySpec {
        name: "line-blur",
        kind: PropertyKind::Number,
        default: DefaultValue::Number(0.0),
        data_driven: true,
    },
    PropertySpec {
        name: "line-color",
        kind: PropertyKind::Color,
        default: DefaultValue::Color(Color::black()),
        data_driven: true,
    },
    PropertySpec {
        name: "line-dasharray",
        kind: PropertyKind::NumberArray(0),
        default: DefaultValue::None,
        data_driven: false,
    },
    PropertySpec {
        name: "line-floorwidth",
        kind: PropertyKind::Number,
        default: DefaultValue::Number(1.0),
        data_driven: true,
    },
    PropertySpec {
        name: "line-gap-width",
        kind: PropertyKind::Number,
        default: DefaultValue::Number(0.0),
        data_driven: true,
    },
    PropertySpec {
        name: "line-gradient",
        kind: PropertyKind::Color,
        default: DefaultValue::None,
        data_driven: false,
    },
    PropertySpec {
        name: "line-offset",
        kind: PropertyKind::Number,
        default: DefaultValue::Number(0.0),
        data_driven: true,
    },
    PropertySpec {
        name: "line-opacity",
        kind: PropertyKind::Number,
        default: DefaultValue::Number(1.0),
        data_driven: true,
    },
    PropertySpec {
        name: "line-pattern",
        kind: PropertyKind::Image,
        default: DefaultValue::None,
        data_driven: true,
    },
    PropertySpec {
        name: "line-translate",
        kind: PropertyKind::NumberArray(2),
        default: DefaultValue::NumberPair(0.0, 0.0),
        data_driven: false,
    },
    PropertySpec {
        name: "line-translate-anchor",
        kind: PropertyKind::Enum,
        default: DefaultValue::Enum("map"),
        data_driven: false,
    },
    PropertySpec {
        name: "line-width",
        kind: PropertyKind::Number,
        default: DefaultValue::Number(1.0),
        data_driven: true,
    },
];

const LINE_LAYOUT: &[PropertySpec] = &[
    PropertySpec {
        name: "line-cap",
        kind: PropertyKind::Enum,
        default: DefaultValue::Enum("butt"),
        data_driven: false,
    },
    PropertySpec {
        name: "line-join",
        kind: PropertyKind::Enum,
        default: DefaultValue::Enum("miter"),
        data_driven: true,
    },
    PropertySpec {
        name: "line-miter-limit",
        kind: PropertyKind::Number,
        default: DefaultValue::Number(2.0),
        data_driven: false,
    },
    PropertySpec {
        name: "line-round-limit",
        kind: PropertyKind::Number,
        default: DefaultValue::Number(1.05),
        data_driven: false,
    },
    PropertySpec {
        name: "line-sort-key",
        kind: PropertyKind::Number,
        default: DefaultValue::Number(0.0),
        data_driven: true,
    },
];

/// Circle paint properties, in mbgl's `CirclePaintProperties` declaration order.
///
/// As with the line table, the order is the interleaved paint buffer's layout and so is
/// load-bearing rather than cosmetic.
const CIRCLE_PAINT: &[PropertySpec] = &[
    PropertySpec {
        name: "circle-blur",
        kind: PropertyKind::Number,
        default: DefaultValue::Number(0.0),
        data_driven: true,
    },
    PropertySpec {
        name: "circle-color",
        kind: PropertyKind::Color,
        default: DefaultValue::Color(Color::black()),
        data_driven: true,
    },
    PropertySpec {
        name: "circle-opacity",
        kind: PropertyKind::Number,
        default: DefaultValue::Number(1.0),
        data_driven: true,
    },
    PropertySpec {
        // Viewport, not map — the odd one out among the anchor-style enums, and what makes a
        // circle keep its screen size under pitch.
        name: "circle-pitch-alignment",
        kind: PropertyKind::Enum,
        default: DefaultValue::Enum("viewport"),
        data_driven: false,
    },
    PropertySpec {
        name: "circle-pitch-scale",
        kind: PropertyKind::Enum,
        default: DefaultValue::Enum("map"),
        data_driven: false,
    },
    PropertySpec {
        name: "circle-radius",
        kind: PropertyKind::Number,
        default: DefaultValue::Number(5.0),
        data_driven: true,
    },
    PropertySpec {
        name: "circle-stroke-color",
        kind: PropertyKind::Color,
        default: DefaultValue::Color(Color::black()),
        data_driven: true,
    },
    PropertySpec {
        name: "circle-stroke-opacity",
        kind: PropertyKind::Number,
        default: DefaultValue::Number(1.0),
        data_driven: true,
    },
    PropertySpec {
        name: "circle-stroke-width",
        kind: PropertyKind::Number,
        default: DefaultValue::Number(0.0),
        data_driven: true,
    },
    PropertySpec {
        name: "circle-translate",
        kind: PropertyKind::NumberArray(2),
        default: DefaultValue::NumberPair(0.0, 0.0),
        data_driven: false,
    },
    PropertySpec {
        name: "circle-translate-anchor",
        kind: PropertyKind::Enum,
        default: DefaultValue::Enum("map"),
        data_driven: false,
    },
];

const CIRCLE_LAYOUT: &[PropertySpec] = &[PropertySpec {
    name: "circle-sort-key",
    kind: PropertyKind::Number,
    default: DefaultValue::Number(0.0),
    data_driven: true,
}];

const FILL_LAYOUT: &[PropertySpec] = &[PropertySpec {
    name: "fill-sort-key",
    kind: PropertyKind::Number,
    default: DefaultValue::Number(0.0),
    data_driven: true,
}];

/// A raster layer's paint properties.
///
/// All eight are uniforms — none is data-driven, and none *can* be: a raster tile is an image
/// rather than a set of features, so there is no feature for a property to vary over. That is why
/// the layer has no paint binder while every other tiled layer does.
const RASTER_PAINT: &[PropertySpec] = &[
    PropertySpec {
        name: "raster-brightness-max",
        kind: PropertyKind::Number,
        default: DefaultValue::Number(1.0),
        data_driven: false,
    },
    PropertySpec {
        name: "raster-brightness-min",
        kind: PropertyKind::Number,
        default: DefaultValue::Number(0.0),
        data_driven: false,
    },
    PropertySpec {
        name: "raster-contrast",
        kind: PropertyKind::Number,
        default: DefaultValue::Number(0.0),
        data_driven: false,
    },
    PropertySpec {
        // Milliseconds, and the one property here the shader never sees: it feeds the fade
        // between a tile and the parent standing in for it, which is a frame-loop quantity.
        name: "raster-fade-duration",
        kind: PropertyKind::Number,
        default: DefaultValue::Number(300.0),
        data_driven: false,
    },
    PropertySpec {
        name: "raster-hue-rotate",
        kind: PropertyKind::Number,
        default: DefaultValue::Number(0.0),
        data_driven: false,
    },
    PropertySpec {
        name: "raster-opacity",
        kind: PropertyKind::Number,
        default: DefaultValue::Number(1.0),
        data_driven: false,
    },
    PropertySpec {
        // How the image is sampled between pixels. `linear` by default, and `nearest` for data
        // a style does not want interpolated — a categorical raster where a blend of two
        // categories is a third that means nothing.
        name: "raster-resampling",
        kind: PropertyKind::Enum,
        default: DefaultValue::Enum("linear"),
        data_driven: false,
    },
    PropertySpec {
        name: "raster-saturation",
        kind: PropertyKind::Number,
        default: DefaultValue::Number(0.0),
        data_driven: false,
    },
];

/// A symbol layer's paint properties.
///
/// Ten of them reach the evaluated-props buffer, five for text and five for icons, and the icon
/// half is present whether or not the layer draws icons — one shader serves both and the buffer
/// is its interface. `icon-color` and `text-color` default to *opaque black*, not to nothing:
/// a layer naming neither still writes both, and zeroing the half a layer does not use puts a
/// transparent black on the wire where the oracle has an opaque one.
///
/// `text-translate` and its anchor are here for completeness of the spec surface; nothing reads
/// them yet.
const SYMBOL_PAINT: &[PropertySpec] = &[
    PropertySpec {
        name: "icon-color",
        kind: PropertyKind::Color,
        default: DefaultValue::Color(Color::black()),
        data_driven: true,
    },
    PropertySpec {
        name: "icon-halo-blur",
        kind: PropertyKind::Number,
        default: DefaultValue::Number(0.0),
        data_driven: true,
    },
    PropertySpec {
        name: "icon-halo-color",
        kind: PropertyKind::Color,
        default: DefaultValue::Color(Color::transparent()),
        data_driven: true,
    },
    PropertySpec {
        name: "icon-halo-width",
        kind: PropertyKind::Number,
        default: DefaultValue::Number(0.0),
        data_driven: true,
    },
    PropertySpec {
        name: "icon-opacity",
        kind: PropertyKind::Number,
        default: DefaultValue::Number(1.0),
        data_driven: true,
    },
    PropertySpec {
        name: "text-color",
        kind: PropertyKind::Color,
        default: DefaultValue::Color(Color::black()),
        data_driven: true,
    },
    PropertySpec {
        name: "text-halo-blur",
        kind: PropertyKind::Number,
        default: DefaultValue::Number(0.0),
        data_driven: true,
    },
    PropertySpec {
        name: "text-halo-color",
        kind: PropertyKind::Color,
        default: DefaultValue::Color(Color::transparent()),
        data_driven: true,
    },
    PropertySpec {
        name: "text-halo-width",
        kind: PropertyKind::Number,
        default: DefaultValue::Number(0.0),
        data_driven: true,
    },
    PropertySpec {
        name: "text-opacity",
        kind: PropertyKind::Number,
        default: DefaultValue::Number(1.0),
        data_driven: true,
    },
];

/// The paint properties a layer type accepts, or `None` for a type R0 does not implement.
#[must_use]
pub fn paint_specs(kind: &LayerKind) -> Option<&'static [PropertySpec]> {
    match kind {
        LayerKind::Background => Some(BACKGROUND_PAINT),
        LayerKind::Fill => Some(FILL_PAINT),
        LayerKind::Line => Some(LINE_PAINT),
        LayerKind::Circle => Some(CIRCLE_PAINT),
        LayerKind::Symbol => Some(SYMBOL_PAINT),
        LayerKind::Raster => Some(RASTER_PAINT),
        _ => None,
    }
}

/// The layout properties a layer type accepts, or `None` for a type R0 does not implement.
#[must_use]
pub fn layout_specs(kind: &LayerKind) -> Option<&'static [PropertySpec]> {
    match kind {
        LayerKind::Background => Some(&[]),
        LayerKind::Fill => Some(FILL_LAYOUT),
        LayerKind::Line => Some(LINE_LAYOUT),
        LayerKind::Circle => Some(CIRCLE_LAYOUT),
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
/// Two properties take their value from another property rather than from their own default,
/// and mbgl implements both in the layer rather than in the table. `fill-outline-color` falls
/// back to `fill-color` when the style does not set it; `line-floorwidth` mirrors `line-width`
/// always, having no spelling of its own. Copying the *resolved* source across, expression and
/// binding together, is what makes them right: when the source is data-driven the target is too,
/// and needs a vertex attribute rather than a uniform.
///
/// Found by the oracle rather than by reading the spec. The golden dump's data-driven fill
/// drawable carries `fill-outline-color` as an attribute at offset 12 even though the style
/// never mentions it, which is only explicable if it inherited the binding along with the
/// value. Treating it as its own constant default gave a stride of 12 where the oracle has 20.
fn apply_layer_rules(layer: &Layer, resolved: &mut BTreeMap<&'static str, ResolvedProperty>) {
    // `line-floorwidth` is unconditional where `fill-outline-color` is a fallback: mbgl's
    // `setLineWidth` assigns both properties, so the mirror holds even when the style writes a
    // width, and there is no `line-floorwidth` for a style to write in the first place.
    let rule = match layer.kind {
        LayerKind::Fill if !layer.paint.contains_key("fill-outline-color") => {
            ("fill-color", "fill-outline-color")
        }
        LayerKind::Line => ("line-width", "line-floorwidth"),
        _ => return,
    };
    let (from, to) = rule;

    let Some(source) = resolved.get(from).cloned() else {
        return;
    };
    if let Some(target) = resolved.get_mut(to) {
        // The spec entry belongs to the target; only the value and how it binds are inherited.
        target.expression = source.expression;
        target.binding = source.binding;
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
        // The expression parser needs what the *spec* says, not just what the style wrote: a
        // legacy function falls back to the property's default, `identity` checks its value
        // against the property's type, and a property typed as an array may be a bare constant
        // rather than a call.
        let context = expression_spec(spec);
        let parse = |value: &Value| {
            Expression::parse_for(value, &context).map_err(|source| PropertyError::Expression {
                property: spec.name.to_string(),
                source,
            })
        };

        let expression = match written.get(spec.name) {
            Some(PropertyValue::Expression(expression)) => parse(expression.value())?,
            Some(PropertyValue::Literal(value)) => {
                check_literal(spec, value)?;
                parse(value)?
            }
            None => parse(&default_value(spec))?,
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

/// What the expression parser needs to know about a property.
///
/// The two halves come from different places and both matter. The default is what a
/// pre-expression function falls back to; the type is what `identity` checks against and what
/// decides whether a bare array is a constant or a malformed call.
fn expression_spec(spec: &PropertySpec) -> expression::PropertySpec {
    expression::PropertySpec {
        default: Some(default_value(spec)),
        expected: Some(match spec.default {
            DefaultValue::Color(_) => expression::Type::Color,
            DefaultValue::Number(_) => expression::Type::Number,
            DefaultValue::Boolean(_) => expression::Type::Boolean,
            // An enum is a string with a value list. The list is checked elsewhere; the type is
            // what the expression parser needs.
            DefaultValue::Enum(_) => expression::Type::String,
            DefaultValue::NumberPair(..) => expression::Type::Array,
            // A property with no default has no type to enforce either. `Value` is the
            // parser's "unknown", which is the honest answer rather than a guess.
            DefaultValue::None => expression::Type::Value,
        }),
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
    // A colour-typed property now arrives already resolved, as four channels in 0..1: the
    // expression parser coerces the result, so `"red"` and a legacy function returning `"red"`
    // both reach here as RGBA. The string form is still accepted, because a colour written
    // inside an expression that is *not* colour-typed — a `match` output read by something
    // else — has not been through that coercion.
    // The common path: a colour-typed expression coerces its result, so a colour property
    // arrives here already resolved.
    if let Value::Color(color) = value {
        return Ok(*color);
    }
    if let Some(channels) = value.as_array()
        && channels.len() == 4
    {
        let mut out = [0.0f32; 4];
        for (slot, channel) in out.iter_mut().zip(channels) {
            #[allow(clippy::cast_possible_truncation)]
            let Some(number) = channel.as_number() else {
                return Err(PropertyError::Type {
                    property: String::new(),
                    expected: "a color",
                    got: channel.type_name(),
                });
            };
            *slot = number as f32;
        }
        return Ok(Color {
            r: out[0],
            g: out[1],
            b: out[2],
            a: out[3],
        });
    }

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
