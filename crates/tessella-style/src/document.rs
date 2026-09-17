// SPDX-License-Identifier: BSD-2-Clause
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
    /// Definitions of the configuration options `["config", …]` reads.
    ///
    /// Mapbox Style Spec v3; maplibre-native has no counterpart. See [`crate::config`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub schema: BTreeMap<String, crate::config::ConfigOption>,
    /// Styles this one imports, for the sake of the values they were given.
    ///
    /// Nothing fetches them: merging an imported style's layers is a feature of its own. They
    /// are here so `["config", name, import-id]` can name one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<crate::config::Import>,
    /// The terrain the map is draped over, when the style asks for one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terrain: Option<Terrain>,
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

/// The style's terrain: which DEM to read, and how far to stretch it.
///
/// Two properties and that is the whole of the spec -- `source` is required and `exaggeration`
/// defaults to one with a minimum of zero. There is no maplibre-native counterpart to check this
/// against: the C++ tree has no `style/terrain.hpp`, no `setTerrain` and no parser member for it
/// at the pinned revision or upstream, and a style carrying one renders identically to a style
/// without. So this is the style spec and MapLibre GL JS, which is the reference plan.md §1 names
/// for the two features that have no oracle.
///
/// # Why a zero exaggeration is not the same as no terrain
///
/// It flattens the surface and keeps everything else: the mesh, the draping, and the elevation
/// queries that return zero. A style that means "no terrain" omits the member. Keeping the two
/// distinct is what lets a map animate an exaggeration to zero without the layer set changing
/// under it.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Terrain {
    /// The id of the `raster-dem` source the elevation is read from.
    pub source: String,
    /// How far the elevation is stretched. One is the ground's own height.
    ///
    /// Absent is one, which is what the spec's default means and why this is not an `Option`: a
    /// caller asking for the exaggeration wants a number, and "the style did not say" and "the
    /// style said one" are the same terrain.
    #[serde(default = "one")]
    pub exaggeration: f64,
}

/// The spec's default exaggeration.
fn one() -> f64 {
    1.0
}

impl Terrain {
    /// The exaggeration, clamped to the range the spec gives it.
    ///
    /// Negative is not "upside-down terrain", it is a value the spec's `minimum: 0` excludes, and
    /// a style that writes one has said something it has no meaning for. Clamped rather than
    /// refused, because the rest of the style is still a map.
    #[must_use]
    pub fn exaggeration(&self) -> f64 {
        if self.exaggeration.is_finite() {
            self.exaggeration.max(0.0)
        } else {
            1.0
        }
    }
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
    /// The annotations the caller added through the map's own API.
    ///
    /// Not a source a stylesheet declares. It is synthesized, carries no fields, and its tiles
    /// are cut from the annotation store rather than fetched or parsed; mbgl keeps it in the
    /// same enum as the rest for the same reason, as `SourceType::Annotations`.
    ///
    /// mbgl's parser rejects `"type": "annotation"` outright where this accepts it, which is the
    /// one divergence and is not one a frame can see: what it produces is an annotation source
    /// with nothing in it, and a source with nothing in it draws nothing either way.
    Annotation,
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
    ///
    /// Spelled `tileSize` in the document. The spec's source fields are camelCase where they are
    /// more than one word, unlike a layer's kebab-case properties, and the rename has to be
    /// stated per field because most of them are single words that need none. Without it the key
    /// falls into `extra` and this reads `None` for every style ever written — which is not a
    /// parse error but a raster basemap covered at the wrong zoom.
    #[serde(default, rename = "tileSize", skip_serializing_if = "Option::is_none")]
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
    /// Cluster radius in pixels. Spelled `clusterRadius` in the document.
    #[serde(
        default,
        rename = "clusterRadius",
        skip_serializing_if = "Option::is_none"
    )]
    pub cluster_radius: Option<f64>,
    /// Zoom past which clustering stops. Spelled `clusterMaxZoom` in the document.
    #[serde(
        default,
        rename = "clusterMaxZoom",
        skip_serializing_if = "Option::is_none"
    )]
    pub cluster_max_zoom: Option<f64>,
    /// Buffer around tile edges, in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer: Option<f64>,
    /// Douglas-Peucker simplification tolerance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tolerance: Option<f64>,
    /// Whether each line records how far along the whole of itself a tile's piece of it runs.
    /// Spelled `lineMetrics` in the document, and what `line-gradient` needs: without it every
    /// piece of a line cut by a tile boundary would start its gradient over.
    #[serde(
        default,
        rename = "lineMetrics",
        skip_serializing_if = "Option::is_none"
    )]
    pub line_metrics: Option<bool>,
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
    /// Elevation mapped to color through a ramp.
    ColorRelief,
    /// Where the device is, and which way it points.
    LocationIndicator,
    /// The ground itself, raised from a DEM.
    ///
    /// Not a type a style document writes. The spec puts terrain at the top level rather than in
    /// the layer list, and [`Style::synthesize_terrain`] turns that member into a layer of this
    /// kind over the DEM the terrain names -- the same move `Annotations::synthesize` makes for a
    /// source nothing declares. Everything downstream then treats the ground as what it is: a
    /// layer with an index, a bucket per tile, a place in painter order, and a uniform block.
    Terrain,
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
            Self::Background
                | Self::Fill
                | Self::FillExtrusion
                | Self::Line
                | Self::Circle
                | Self::Symbol
                | Self::Raster
                | Self::Heatmap
                | Self::Hillshade
                | Self::ColorRelief
                | Self::LocationIndicator
                | Self::Terrain
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
/// The distinction is made here rather than deferred because it is purely syntactic — an array
/// headed by a registered operator is a call, a pre-expression function object is evaluated the
/// same way, and anything else is data — and because DR-11's classification consumes it. What this does *not* do is decide whether an
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

/// Whether a property value is evaluated rather than used as written.
///
/// One rule for the three places a value is classified -- the deserializer, [`PropertyValue::from_value`]
/// and config resolution -- because two of them disagreeing is a property that parses as one
/// thing and is rebuilt as another.
fn is_evaluated(value: &Value) -> bool {
    value.looks_like_expression() || value.looks_like_function()
}

/// An unparsed expression, or a pre-expression function the expression parser converts.
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
        if is_evaluated(&value) {
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
    /// Classifies a raw value the way the deserializer does.
    ///
    /// The rule is the deserializer's, not a second one: a call or a pre-expression function is
    /// evaluated and everything else is data. Needed by anything that builds a layer rather than
    /// parsing one — a synthesized annotation layer is the case — because `ExpressionValue`
    /// holds its value privately and there is otherwise no way to say "whatever this turns out
    /// to be".
    #[must_use]
    pub fn from_value(value: Value) -> Self {
        if is_evaluated(&value) {
            Self::Expression(ExpressionValue(value))
        } else {
            Self::Literal(value)
        }
    }

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

    /// Drops the layers that will not compile, and says which and why.
    ///
    /// # Why a whole style is not refused over one layer
    ///
    /// mbgl's parser converts each layer on its own: `Parser::parseLayer` calls
    /// `convert<std::unique_ptr<Layer>>`, and on failure logs a warning and *returns* — the
    /// layer never enters `layers`, and every other layer in the document still does. So a
    /// style that names one thing mbgl does not have renders without that layer rather than not
    /// at all.
    ///
    /// This is not a nicety on real styles, it is the difference between a map and a blank
    /// screen. A vendor style routinely uses expressions outside the MapLibre spec —
    /// `["distance-from-center"]` and `["pitch"]` are Mapbox GL JS v3 additions that mbgl has no
    /// compound expression for — and each one appears in a filter on a label layer. Refusing the
    /// document over them drops the other hundred layers with it.
    ///
    /// # Why the reasons are returned rather than logged
    ///
    /// mbgl's `Log::Warning` goes wherever the embedder pointed the log, which in practice is
    /// nowhere. A dropped layer is a real difference between what the style asked for and what
    /// is drawn, and the caller is the only one that can decide whether it matters — so it is a
    /// value, and this returns it.
    ///
    /// Compiling here also means the tile builders cannot meet these failures: everything left
    /// in `layers` has had its filter, paint and layout compiled once, on the way in.
    ///
    /// # Config first
    ///
    /// [`Self::resolve_config`] runs before any of it, because a `["config", …]` call is not
    /// something the compile step can make sense of — the value lives in the document's own
    /// `schema`, not in the expression. Compiling first would drop every layer that reads one.
    pub fn reject_uncompilable(&mut self) -> Vec<RejectedLayer> {
        self.resolve_config();
        let mut rejected = Vec::new();
        self.layers.retain(|layer| match compile_check(layer) {
            Ok(()) => true,
            Err(reason) => {
                rejected.push(RejectedLayer {
                    id: layer.id.clone(),
                    reason,
                });
                false
            }
        });
        rejected
    }

    /// Substitutes every `["config", …]` call with the value it resolves to.
    ///
    /// A config value is fixed for a style load — no zoom, no feature, no camera in it — so it
    /// belongs in the document rather than in the evaluator. After this no `config` call
    /// remains, and an expression that used one is constant in that part and folds like any
    /// other constant (DR-11).
    ///
    /// Called by [`Self::reject_uncompilable`] before anything is compiled, because that is the
    /// point of it: a style whose labels read `["config", "language"]` has twelve layers that
    /// compile only once the call has become a value.
    ///
    /// Idempotent — a second pass finds no calls to replace.
    pub fn resolve_config(&mut self) {
        let values = crate::config::ConfigValues::new(&self.schema, &self.imports);
        for layer in &mut self.layers {
            if let Some(filter) = &layer.filter {
                layer.filter = Some(crate::config::substitute(filter, &values));
            }
            for value in layer.layout.values_mut().chain(layer.paint.values_mut()) {
                let PropertyValue::Expression(expression) = value else {
                    continue;
                };
                let resolved = crate::config::substitute(expression.value(), &values);
                // A property that *was* a config call is now a plain value, and has to stop
                // being an expression or the compile step reads a literal as a call. One that
                // merely contained a call is still an expression, with a literal inside it.
                *value = if is_evaluated(&resolved) {
                    PropertyValue::Expression(ExpressionValue(resolved))
                } else {
                    PropertyValue::Literal(resolved)
                };
            }
        }
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

    /// The terrain's DEM, when the style has a terrain that names one.
    ///
    /// `Some((id, source))` only when all three hold: the style has a `terrain`, its `source`
    /// names a source the style declares, and that source is a `raster-dem`. A terrain naming a
    /// vector source or a source that is not there is inert -- the map draws flat -- because
    /// there is no elevation to read and inventing one is worse than drawing the map the style
    /// otherwise describes.
    ///
    /// # Why one accessor answers two questions
    ///
    /// "Is this terrain usable" and "what do I fetch" have the same answer, and they are asked
    /// from opposite ends of the build: the resolve plan asks the second before any layer is
    /// compiled, and the frame asks the first every tick. Two accessors would be two places for
    /// the definition of a usable terrain to drift apart.
    #[must_use]
    pub fn terrain_dem(&self) -> Option<(&str, &TileSource)> {
        let terrain = self.terrain.as_ref()?;
        let (id, source) = self.sources.get_key_value(terrain.source.as_str())?;
        match source {
            Source::RasterDem(tiles) => Some((id.as_str(), tiles)),
            _ => None,
        }
    }

    /// Whether a layer draws a *picture* of this source rather than standing on it.
    ///
    /// A hillshade and a color relief shade the DEM per fragment, so they want it at the zoom
    /// that puts a texel on a screen pixel. The terrain does not: it samples once per mesh
    /// vertex, and the level that rule asks for is finer than the geometry can carry. The two
    /// therefore cover at different zooms, and a style with no such layer should not fetch the
    /// finer cover at all -- which for a terrain-only style is most of the tiles it asks for.
    #[must_use]
    pub fn shades(&self, source: &str) -> bool {
        self.layers.iter().any(|layer| {
            matches!(layer.kind, LayerKind::Hillshade | LayerKind::ColorRelief)
                && layer.source.as_deref() == Some(source)
        })
    }
}

/// The id [`Style::synthesize_terrain`] gives the layer it makes.
///
/// Dotted, like the annotation source's, so it cannot collide with anything a document writes:
/// the spec's layer ids are author-chosen strings and nobody writes one of these by accident.
pub const TERRAIN_LAYER_ID: &str = "org.maplibre.terrain";

impl Style {
    /// Turns the style's `terrain` member into a layer over the DEM it names.
    ///
    /// Idempotent: the layer it made last time is removed first, so a style whose terrain changed
    /// -- or went away -- does not keep the old one. Called wherever a style is (re)compiled, as
    /// `Annotations::synthesize` is.
    ///
    /// # Why a layer and not a special case
    ///
    /// The ground is drawn geometry with a texture, a place in painter order and a uniform block,
    /// which is what a layer is. Carrying it as anything else means every one of those -- the
    /// index, the bucket lookup, the draw order, the UBO keying -- needs a branch for the one
    /// thing that is not a layer. The annotation source settled the same question the same way.
    ///
    /// # Where it goes, and why not first
    ///
    /// Under every layer that draws on the ground, and *over* a leading background. A background
    /// is not ground -- it is the void the map is painted on, mbgl's clear color -- so the terrain
    /// sits on it the way every other layer sits on the terrain.
    ///
    /// Inserting at zero instead was wrong twice. The background then draws *after* the ground
    /// and paints over it, which is a flat map with a terrain hidden behind it. And it stops the
    /// background being the style's first layer, which is the test
    /// `tile::background_covers_viewport` makes: a solid first-layer background is one viewport
    /// quad, and demoted to second it silently becomes a quad per cover tile instead. A style
    /// would have changed how its background draws by gaining a terrain.
    pub fn synthesize_terrain(&mut self) {
        use alloc::string::ToString as _;

        self.layers.retain(|layer| layer.id != TERRAIN_LAYER_ID);
        let Some((source, _)) = self.terrain_dem() else {
            return;
        };
        let source = source.to_string();
        // Past the backgrounds the style opens with, and before anything else.
        let at = self
            .layers
            .iter()
            .position(|layer| layer.kind != LayerKind::Background)
            .unwrap_or(self.layers.len());
        self.layers.insert(
            at,
            Layer {
                id: TERRAIN_LAYER_ID.to_string(),
                kind: LayerKind::Terrain,
                source: Some(source),
                source_layer: None,
                minzoom: None,
                maxzoom: None,
                filter: None,
                layout: BTreeMap::new(),
                paint: BTreeMap::new(),
                extra: BTreeMap::new(),
            },
        );
    }
}

/// A layer that could not be compiled, and the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedLayer {
    /// The layer's id, as the style wrote it.
    pub id: String,
    /// Why it will not compile, in the words of whichever step refused it.
    pub reason: String,
}

/// Everything the tile builders will ask of a layer, asked once here instead.
fn compile_check(layer: &Layer) -> Result<(), alloc::string::String> {
    use alloc::string::ToString;

    if let Some(filter) = &layer.filter {
        let parsed = crate::filter::Filter::parse(filter).map_err(|error| error.to_string())?;
        // `pitch` and `distance-from-center` "may only be used in the `filter` expression for a
        // `symbol` layer" — the style spec says so of both, in the same words. This is the
        // second half of that: a filter, yes, but only a symbol's.
        //
        // The dependency does the detecting. `Dependency::CAMERA` is introduced by exactly
        // these two operators and by nothing else, so a filter that needs the camera is a
        // filter that used one — no second walk of the tree, and a third camera operator would
        // be covered the day it is added.
        if parsed.expression().dependency().needs_camera() && layer.kind != LayerKind::Symbol {
            return Err(alloc::format!(
                "`pitch` and `distance-from-center` are only allowed in a symbol layer's \
                 filter, and this is a {} layer",
                alloc::format!("{:?}", layer.kind).to_lowercase()
            ));
        }
    }
    crate::property::resolve_paint(layer).map_err(|error| error.to_string())?;
    crate::property::resolve_layout(layer).map_err(|error| error.to_string())?;

    // Then every remaining expression, whether or not a spec table names the property.
    //
    // The tables cover the kinds this build draws, and `layout_specs` answers `None` for a
    // symbol layer — so a symbol's `text-field` was compiled by nobody. A layer whose text
    // cannot be shaped then stayed in the style and drew *nothing*, which is the worst of the
    // three outcomes: refusing the document is loud, dropping the layer is honest, and silently
    // rendering an empty layer reads as a style that chose not to label anything.
    //
    // It is not hypothetical. A vendor style's `text-field` is
    // `["coalesce", ["get", ["concat", "name_", ["config", …]]], ["get", "name"]]`, and `config`
    // is a Mapbox Standard style-config expression with no MapLibre counterpart — so twelve
    // label layers compiled clean and shaped no labels.
    // Against the property's own spec where there is one, and untyped only where there is not.
    //
    // This used to parse everything untyped, and that rejected whole layers for a shape half of
    // every real style uses: `["interpolate", ["linear"], ["zoom"], 0, "red", 10, "blue"]`.
    // Color stops are written as *strings*, and a string is not interpolatable — the coercion
    // to color is what the expected type buys, which is exactly what `resolve_paint` passes and
    // this loop was throwing away. A `step` survived because step interpolates nothing, so the
    // failure looked like something about `interpolate` rather than something about types.
    //
    // The loop still runs over everything: its job is the camera check below, and the properties
    // a spec table does not name are still the ones nobody else compiles.
    let paint = crate::property::paint_specs(&layer.kind).unwrap_or(&[]);
    let layout = crate::property::layout_specs(&layer.kind).unwrap_or(&[]);
    let spec_for = |name: &str| {
        paint
            .iter()
            .chain(layout.iter())
            .find(|spec| spec.name == name)
            .map(crate::property::expression_spec)
    };

    for (name, value) in layer.layout.iter().chain(layer.paint.iter()) {
        if let PropertyValue::Expression(expression) = value {
            let parsed = match spec_for(name) {
                Some(spec) => crate::Expression::parse_for(expression.value(), &spec),
                None => crate::Expression::parse(expression.value()),
            }
            .map_err(|error| error.to_string())?;
            // And the first half: a filter, not a paint or layout property. Allowing one here
            // would be worse than refusing it, because it would appear to work — the value is
            // real, and §12.1's per-interval cache would then hold a camera-dependent number
            // for the length of a zoom interval and hand back a stale pitch for every frame in
            // it. The spec forbids the case that has nowhere correct to be evaluated.
            if parsed.dependency().needs_camera() {
                return Err(alloc::string::String::from(
                    "`pitch` and `distance-from-center` are only allowed in a filter, \
                     not in a paint or layout property",
                ));
            }
        }
    }
    Ok(())
}
