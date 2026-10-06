//! What each feature left in a bucket, so a host can ask what is drawn at a point.
//!
//! # Why a record rather than a re-read
//!
//! mbgl answers `queryRenderedFeatures` by keeping the tile's `GeometryTileData` alive and reading
//! the feature back out of it on demand. Nothing here can: the build job takes a `&mvt::Tile`,
//! returns `Vec<LayerBucket>`, and drops both the decoded tile and the response body on the way
//! out. So what a query will need has to be written down while the tile is being laid out, which
//! is what this module is.
//!
//! What gets written down is deliberately not the geometry. A feature's vertices are already in
//! its bucket and are already *extruded* — a line's quads carry its width, a circle's corners
//! carry its radius — so a record that names the vertex range it filled points at geometry that is
//! closer to what was actually drawn than mbgl's own query is, which re-grows the source geometry
//! by the paint at query time. The record is therefore an identity and a range, and the shape
//! stays small.
//!
//! # Why the properties have two shapes
//!
//! An MVT feature's tags are not its own. Its keys are `Arc<str>` and its string values are
//! [`mvt::Value::String(Arc<str>)`], both pointing into one per-layer table, precisely so that a
//! value repeated across ten thousand features is stored once — [`mvt::Tile::decode`] says it was
//! measured copying them and does not. A GeoJSON feature's properties are a
//! `BTreeMap<String, Value>` it owns outright, and they nest, which an `mvt::Value` cannot
//! represent at all.
//!
//! Flattening both into the richer type would copy every MVT key and string once per feature and
//! throw that table away; flattening both into the cheaper one would silently drop a GeoJSON
//! feature's nested properties. So [`Tags`] carries whichever shape the source actually had, and
//! the accessors are what the two have in common.
//!
//! [`mvt::Value::String(Arc<str>)`]: tessella_source::mvt::Value::String
//!
//! # What it weighs
//!
//! Measured on `protomaps-berlin-14-8802-5373.mvt` -- 89,859 bytes of tile, every source layer it
//! carries named by a fill layer so that every feature is recorded once: 905 records over 2,971
//! property pairs, which is about 80 KiB of record and 119 KiB of tag, or roughly twice the tile's
//! own encoded size. That is the cost of a tile being queryable, and it is paid whether or not a
//! host ever asks, as mbgl pays for keeping its `GeometryTileData`.
//!
//! What it costs to write is small beside that. `expression_cost`'s z10 streets tile, 593 features
//! over fourteen layers, with the recording in place against the same build with
//! `Recording::add` returning immediately: 377.32 us against 370.48 us of data-driven build and
//! 333.95 us against 326.92 us of constant, so about 2% either way, for 127 allocations and 38 KiB.
//! The control is the point -- the absolute numbers move with the machine, the difference between
//! two runs of one binary does not.
//!
//! The 119 KiB is the part that can still go. A record could hold its layer in an
//! `Arc<mvt::Layer>` and its own property `Range<u32>` into that layer's tables instead of a
//! `Vec` of pairs, which is what the decoder already does internally and would leave the tag cost
//! at zero. It is not done here because `mvt::Feature::properties` is private and the builders
//! take a `&Tile` they do not own, so the change is a signature change through `boot` rather than
//! an addition.
//!
//! # The overlap with feature state
//!
//! `PaintBinder`'s own index (#353) records the same three things for a state-reading layer, so
//! such a layer now carries both. They are not merged yet on purpose: `restate` writes bytes that
//! a test compares to a fresh build byte for byte, and re-pointing it at a different record is a
//! change to that path rather than to this one. The binder's index is also narrower by design --
//! it drops a non-numeric id, because state is *keyed* by id, where a query only reports one.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Range;

use tessella_source::mvt;
use tessella_style::Value;

/// One feature's properties, in whichever shape its source has them.
///
/// See the module note on why this is not one type.
#[derive(Debug, Clone, PartialEq)]
pub enum Tags {
    /// An MVT feature's tags, sharing its layer's key and value tables.
    Mvt(Vec<(Arc<str>, mvt::Value)>),
    /// A GeoJSON feature's own properties, which it owns and which may nest.
    Json(BTreeMap<String, Value>),
}

impl Tags {
    /// One property by name.
    ///
    /// An MVT value widens into a [`Value`] on the way out, which costs a string copy for the one
    /// key asked about rather than for every key of every feature.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<Value> {
        match self {
            Self::Mvt(tags) => tags
                .iter()
                .find(|(name, _)| &**name == key)
                .map(|(_, value)| widen(value)),
            Self::Json(map) => map.get(key).cloned(),
        }
    }

    /// Every property, in key order for a GeoJSON feature and in tag order for an MVT one.
    ///
    /// Tag order is the order the tile wrote them, which is what mbgl hands back as well.
    pub fn iter(&self) -> impl Iterator<Item = (&str, Value)> + '_ {
        // Two arms of one iterator rather than a boxed `dyn`: this runs once per returned feature
        // per query, and the crate is `no_std` for its own code.
        let mvt = match self {
            Self::Mvt(tags) => Some(tags.iter().map(|(name, value)| (&**name, widen(value)))),
            Self::Json(_) => None,
        };
        let json = match self {
            Self::Json(map) => Some(
                map.iter()
                    .map(|(name, value)| (name.as_str(), value.clone())),
            ),
            Self::Mvt(_) => None,
        };
        mvt.into_iter().flatten().chain(json.into_iter().flatten())
    }

    /// How many properties there are.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Mvt(tags) => tags.len(),
            Self::Json(map) => map.len(),
        }
    }

    /// Whether the feature carried any.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// An MVT value as the style's own.
fn widen(value: &mvt::Value) -> Value {
    match value {
        mvt::Value::String(text) => Value::String(String::from(&**text)),
        mvt::Value::Number(number) => Value::Number(*number),
        mvt::Value::Bool(flag) => Value::Bool(*flag),
    }
}

/// One feature, as the thing a rendered-feature query returns.
#[derive(Debug, Clone, PartialEq)]
pub struct Queryable {
    /// The feature's id as its source gave it, which is a number for MVT and may be a string for
    /// GeoJSON.
    ///
    /// Not narrowed to a `u64` the way feature *state* narrows it: state is keyed by id and so can
    /// only be set for a feature that has a numeric one, but a query reports what is there and a
    /// host that sees a string id can still act on it.
    pub id: Option<Value>,
    /// `Point`, `LineString`, `Polygon` or `Unknown`, as `["geometry-type"]` spells them.
    pub geometry_type: &'static str,
    /// Its properties.
    pub properties: Tags,
    /// The vertices it filled, as a range into its bucket's own vertex buffer.
    ///
    /// `u32` rather than `usize`: a bucket's vertices are counted in thousands, and this record
    /// exists once per feature per layer per tile, where eight bytes saved is the whole reason the
    /// range is stored instead of the geometry.
    pub vertices: Range<u32>,
}

/// What a query needs off a feature, in whichever shape its source has it.
///
/// A third trait beside `expression::Feature` because that one's `properties` returns an owned
/// [`Value`], which is the copy this module exists to avoid for an MVT tile.
pub trait Described {
    /// Its id, as its source gave it.
    fn query_id(&self) -> Option<Value>;
    /// Its geometry type, as `["geometry-type"]` spells it.
    fn query_geometry_type(&self) -> &'static str;
    /// Its properties, in its source's own shape.
    fn query_tags(&self) -> Tags;
}

impl Described for tessella_source::geojson::GeoJsonFeature {
    fn query_id(&self) -> Option<Value> {
        self.id.clone()
    }

    fn query_geometry_type(&self) -> &'static str {
        geometry_type_of(tessella_style::expression::Feature::geometry_type(self))
    }

    fn query_tags(&self) -> Tags {
        Tags::Json(self.properties.clone())
    }
}

impl Described for mvt::FeatureRef<'_> {
    fn query_id(&self) -> Option<Value> {
        #[allow(clippy::cast_precision_loss)]
        self.id().map(|id| Value::Number(id as f64))
    }

    fn query_geometry_type(&self) -> &'static str {
        geometry_type_of(tessella_style::expression::Feature::geometry_type(self))
    }

    fn query_tags(&self) -> Tags {
        Tags::Mvt(self.properties().to_vec())
    }
}

/// The borrowed geometry-type name as a static one.
///
/// The four the spec has. Anything else is `Unknown`, which is what mbgl reports for an MVT
/// feature whose `GeomType` is `UNKNOWN`.
fn geometry_type_of(name: &str) -> &'static str {
    match name {
        "Point" => "Point",
        "LineString" => "LineString",
        "Polygon" => "Polygon",
        _ => "Unknown",
    }
}

/// One bucket's records, as they are accumulated during layout.
///
/// A plain `Vec` would do; this exists for [`Self::add`], which is the one rule every call site
/// has to get right and which is easy to get wrong in eight places.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Recording(Vec<Queryable>);

impl Recording {
    /// Records `feature` as owning the vertices `start..end`.
    ///
    /// A feature that produced no vertices records nothing. That is not tidiness: a query walks
    /// these ranges to decide what was hit, and an empty range belonging to a feature that was
    /// filtered out, clipped away or degenerate would be a feature reported as drawn when nothing
    /// of it reached the buffer.
    pub fn add(&mut self, feature: &dyn Described, start: usize, end: usize) {
        if end <= start {
            return;
        }
        // Saturating rather than `try_into`: a bucket past four billion vertices cannot be drawn
        // and is not a reason to fail the tile. The clamp is unreachable and recorded so that it
        // reads as unreachable rather than as an unhandled case.
        let (start, end) = (
            u32::try_from(start).unwrap_or(u32::MAX),
            u32::try_from(end).unwrap_or(u32::MAX),
        );
        self.0.push(Queryable {
            id: feature.query_id(),
            geometry_type: feature.query_geometry_type(),
            properties: feature.query_tags(),
            vertices: start..end,
        });
    }

    /// What was recorded.
    #[must_use]
    pub fn into_vec(self) -> Vec<Queryable> {
        self.0
    }
}
