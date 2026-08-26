//! The style document: sources, layers, and their properties.
//!
//! Parse only. Nothing here evaluates an expression, resolves a paint property against a zoom,
//! or validates that a layer's properties belong to its type. Those are the compile step
//! (DR-11), and keeping them apart matters: §12.5 wants a binary compiled-style cache keyed by
//! style etag, which only means anything if "parsed" and "compiled" are separable stages.
//!
//! # Properties stay untyped here
//!
//! A layer's `paint` and `layout` are maps of name to [`PropertyValue`] rather than a struct
//! per layer type with a field per property. That is deliberate. The typed view is what the
//! compile step produces, because it is the step that knows which properties a layer type
//! accepts, which are data-driven-capable, and what each one's default is. Building the typed
//! view during parse would mean hundreds of fields whose only job is to be re-sorted into
//! expression endpoints immediately afterwards, and it would make an unrecognized property a
//! parse failure rather than something the compile step can report precisely.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::value::Value;

/// A parsed style document.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Style {
    /// Spec version. Always 8 for anything this frontend accepts.
    pub version: u32,
    /// Human-readable name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Sprite sheet URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sprite: Option<String>,
    /// Glyph range URL template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glyphs: Option<String>,
    /// Default center, as `[longitude, latitude]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub center: Option<[f64; 2]>,
    /// Default zoom.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zoom: Option<f64>,
    /// Default bearing in degrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearing: Option<f64>,
    /// Default pitch in degrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pitch: Option<f64>,
    /// The style light. Travels in the camera block (§2.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub light: Option<Value>,
    /// Default transition timing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transition: Option<Transition>,
    /// Sources by id.
    #[serde(default)]
    pub sources: BTreeMap<String, Source>,
    /// Layers, in draw order. Order is the document's, and is load-bearing.
    #[serde(default)]
    pub layers: Vec<Layer>,
    /// Anything the spec has and this parser does not yet name.
    ///
    /// Kept rather than dropped so a round trip is lossless and so an unrecognized top-level
    /// key is a thing the compile step can report, rather than something that silently
    /// vanished between parse and use.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// Transition timing, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct Transition {
    /// How long a change takes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<u64>,
    /// How long to wait before starting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay: Option<u64>,
}

/// A source definition.
///
/// The `type` discriminant is the spec's, and an unknown one is kept as [`Source::Other`]
/// rather than rejected: a style carrying a source type this build does not implement is a
/// style whose other layers should still draw.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Source {
    /// Vector tiles.
    Vector(TileSource),
    /// Raster tiles.
    Raster(TileSource),
    /// Raster DEM tiles.
    RasterDem(TileSource),
    /// GeoJSON, either inline or by URL.
    Geojson(GeojsonSource),
    /// Anything else the spec defines and this build does not implement.
    #[serde(untagged)]
    Other(Value),
}

/// A tiled source: vector, raster, or raster-dem.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
pub struct TileSource {
    /// TileJSON URL, when the source is described indirectly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Tile URL templates, when it is described inline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tiles: Option<Vec<String>>,
    /// Minimum zoom the source provides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minzoom: Option<f64>,
    /// Maximum zoom the source provides. Beyond it, tiles are overscaled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maxzoom: Option<f64>,
    /// Tile side in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tile_size: Option<u32>,
    /// Bounding box, as `[west, south, east, north]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds: Option<[f64; 4]>,
    /// Attribution text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<String>,
    /// Unrecognized keys, kept for a lossless round trip.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// A GeoJSON source.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct GeojsonSource {
    /// Either a URL string or inline GeoJSON. The spec overloads one key for both, and which
    /// it is cannot be known until it is inspected — a string is a URL, an object is data.
    pub data: Value,
    /// Zoom past which features are no longer clustered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maxzoom: Option<f64>,
    /// Whether to cluster points.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<bool>,
    /// Cluster radius in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster_radius: Option<f64>,
    /// Zoom past which clustering stops.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster_max_zoom: Option<f64>,
    /// Buffer around tile edges, in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer: Option<f64>,
    /// Douglas-Peucker simplification tolerance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tolerance: Option<f64>,
    /// Unrecognized keys, kept for a lossless round trip.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl GeojsonSource {
    /// The URL, when `data` is one rather than inline GeoJSON.
    #[must_use]
    pub fn url(&self) -> Option<&str> {
        self.data.as_str()
    }

    /// True when the data is inline rather than fetched. The hermetic probe style is this
    /// case, which is what lets §10's R0 run with no network at all.
    #[must_use]
    pub fn is_inline(&self) -> bool {
        !matches!(self.data, Value::String(_) | Value::Null)
    }
}

/// What kind of thing a layer draws.
///
/// Kept as an enum with an `Other` arm rather than a string so that matching on it is
/// exhaustive where it matters, without a style using a layer type this build does not
/// implement failing to parse. §1 puts heatmap, hillshade and the rest behind an explicit
/// line; this is where that line is drawn without pretending they do not exist.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LayerKind {
    /// A full-viewport background.
    Background,
    /// Filled polygons.
    Fill,
    /// Stroked lines.
    Line,
    /// Circles.
    Circle,
    /// Text and icons.
    Symbol,
    /// Raster imagery.
    Raster,
    /// Extruded polygons.
    FillExtrusion,
    /// Heatmap density.
    Heatmap,
    /// Hillshading.
    Hillshade,
    /// A host-drawn layer.
    Custom,
    /// A type this build does not implement.
    #[serde(untagged)]
    Other(String),
}

impl LayerKind {
    /// True for the layer types R0 covers (§10).
    #[must_use]
    pub fn is_r0(&self) -> bool {
        matches!(self, Self::Background | Self::Fill)
    }

    /// True for the layer types this build can turn into geometry.
    ///
    /// Kept separate from [`Self::is_r0`], which is a statement about the release's scope and
    /// does not move as later releases add types. This one is what the tile builder gates on,
    /// so the two disagree for exactly as long as a type is implemented ahead of, or behind,
    /// the release that owns it.
    #[must_use]
    pub fn is_built(&self) -> bool {
        matches!(
            self,
            Self::Background | Self::Fill | Self::Line | Self::Circle | Self::Symbol | Self::Raster
        )
    }
}

/// A layer definition.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Layer {
    /// Layer id, unique within the style.
    pub id: String,
    /// What this layer draws.
    #[serde(rename = "type")]
    pub kind: LayerKind,
    /// Source id. Absent for background, which draws from nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Layer within a vector source.
    #[serde(
        rename = "source-layer",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub source_layer: Option<String>,
    /// Zoom below which the layer is not drawn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minzoom: Option<f64>,
    /// Zoom at and above which the layer is not drawn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maxzoom: Option<f64>,
    /// Feature filter.
    ///
    /// Left as a raw value because the spec has two syntaxes for it and they are not
    /// distinguishable by shape alone: the modern one is an expression, and the legacy one
    /// (`["==", "$type", "Polygon"]`, which the probe style uses) looks exactly like one but
    /// binds `$type` and `$id` specially. Converting the legacy form is a compile-step job,
    /// and doing it here would mean guessing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<Value>,
    /// Paint properties.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub paint: BTreeMap<String, PropertyValue>,
    /// Layout properties.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub layout: BTreeMap<String, PropertyValue>,
    /// Unrecognized keys, kept for a lossless round trip.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// A property's value: either a literal or something to be evaluated.
///
/// The distinction is made here rather than deferred because it is purely syntactic — a
/// non-empty array whose first element is a string is a call, anything else is data — and
/// because DR-11's classification consumes it. What this does *not* do is decide whether an
/// expression is constant, camera-only, or data-driven; that needs the operator table and
/// belongs to the compile step.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum PropertyValue {
    /// Evaluated. Held as a raw value until the compile step parses the operator.
    Expression(ExpressionValue),
    /// Used as-is.
    Literal(Value),
}

/// An unparsed expression.
#[derive(Debug, Clone, PartialEq)]
pub struct ExpressionValue(Value);

impl ExpressionValue {
    /// The raw value.
    #[must_use]
    pub fn value(&self) -> &Value {
        &self.0
    }

    /// The operator name — the expression's first element.
    #[must_use]
    pub fn operator(&self) -> Option<&str> {
        self.0.as_array()?.first()?.as_str()
    }

    /// The operator's arguments.
    #[must_use]
    pub fn arguments(&self) -> &[Value] {
        self.0.as_array().map_or(&[], |items| &items[1..])
    }
}

impl<'de> Deserialize<'de> for ExpressionValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        if value.looks_like_expression() {
            Ok(Self(value))
        } else {
            // Not an error the caller sees: `PropertyValue` is untagged, so serde falls
            // through to the literal arm.
            Err(serde::de::Error::custom("not an expression"))
        }
    }
}

impl Serialize for ExpressionValue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl PropertyValue {
    /// The literal value, if this is one.
    #[must_use]
    pub fn as_literal(&self) -> Option<&Value> {
        match self {
            Self::Literal(value) => Some(value),
            Self::Expression(_) => None,
        }
    }

    /// The expression, if this is one.
    #[must_use]
    pub fn as_expression(&self) -> Option<&ExpressionValue> {
        match self {
            Self::Expression(expression) => Some(expression),
            Self::Literal(_) => None,
        }
    }
}

impl Style {
    /// Parses a style document.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Json`] when the document is not valid JSON or does not match the shape above,
    /// and [`crate::Error::UnsupportedVersion`] when it declares a spec version this frontend does
    /// not implement.
    pub fn parse(json: &str) -> Result<Self, crate::Error> {
        let style: Self = serde_json::from_str(json)?;
        if style.version != 8 {
            return Err(crate::Error::UnsupportedVersion(style.version));
        }
        Ok(style)
    }

    /// Looks up a layer by id.
    #[must_use]
    pub fn layer(&self, id: &str) -> Option<&Layer> {
        self.layers.iter().find(|layer| layer.id == id)
    }

    /// Looks up a source by id.
    #[must_use]
    pub fn source(&self, id: &str) -> Option<&Source> {
        self.sources.get(id)
    }
}
