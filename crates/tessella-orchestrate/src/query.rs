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
//! The MVT arm does not copy its slice either. A layer's property table is *already* one flat
//! buffer of every property of every feature, which its `Feature` indexes with a `Range<u32>`, so a
//! record that copied its own slice out would be copying a table that exists. It holds the table
//! and the range instead.
//!
//! [`mvt::Value::String(Arc<str>)`]: tessella_source::mvt::Value::String
//!
//! # What it weighs
//!
//! Measured on `protomaps-berlin-14-8802-5373.mvt`, 89,859 bytes of tile, two ways -- because the
//! answer depends on the style and the first measurement here used the shape that flatters a copy.
//!
//! | style | records | pairs | tables | a copy per feature | this | difference |
//! | --- | --- | --- | --- | --- | --- | --- |
//! | one layer per source layer | 905 | 2,971 | 7 | 140,560 B | 204,920 B | **+64 KiB** |
//! | twelve `line` layers over `roads` | 2,880 | 15,816 | **1** | 701,760 B | 306,160 B | **-386 KiB** |
//!
//! The second row is the shape a real style has: a source layer is named by many style layers --
//! a dozen road layers over `roads` is ordinary -- and each becomes its own bucket. A copy per
//! feature pays for the table once per bucket, so its cost grows with the number of layers; sharing
//! it does not, which is the whole of the 2.3x there.
//!
//! The first row is the price of that, and it is real: one table per source layer keeps the pairs of
//! features the style *filtered out* as well, where a copy keeps only what was kept. Two things
//! would take it back, neither done here: a `Queryable` is 88 bytes, of which 16 are recoverable by
//! narrowing `id` from a [`Value`] and `geometry_type` from a `&'static str`; and the builder
//! already knows how many style layers name each source layer, so it could copy for the ones named
//! once and share for the rest.
//!
//! What it costs to write is small either way. `expression_cost`'s z10 streets tile, 593 features
//! over fourteen layers, with the recording in place against the same binary with `Recording::add`
//! returning immediately: 377.32 us against 370.48 us of data-driven build and 333.95 us against
//! 326.92 us of constant, so about 2%. The control is the point -- the absolute numbers move with
//! the machine, the difference between two runs of one binary does not.
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
    /// An MVT feature's tags, as a window on its layer's own property table.
    ///
    /// The table is the layer's, shared: one per source layer however many style layers name it,
    /// and sixteen bytes in the record rather than a `Vec` of pairs per feature.
    Mvt {
        /// Every property of every feature of the layer, in feature order.
        table: Arc<Vec<(Arc<str>, mvt::Value)>>,
        /// This feature's slice of it.
        range: Range<u32>,
    },
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
            Self::Mvt { .. } => self
                .tags()
                .iter()
                .find(|(name, _)| &**name == key)
                .map(|(_, value)| widen(value)),
            Self::Json(map) => map.get(key).cloned(),
        }
    }

    /// An MVT feature's own slice of its layer's table, or nothing for a GeoJSON one.
    ///
    /// Clamped rather than indexed: the range came from the layer the table came from, so it is in
    /// bounds, and a panic in a query is worse than an empty answer if it ever is not.
    #[must_use]
    fn tags(&self) -> &[(Arc<str>, mvt::Value)] {
        match self {
            Self::Mvt { table, range } => {
                let end = (range.end as usize).min(table.len());
                let start = (range.start as usize).min(end);
                &table[start..end]
            }
            Self::Json(_) => &[],
        }
    }

    /// Every property, in key order for a GeoJSON feature and in tag order for an MVT one.
    ///
    /// Tag order is the order the tile wrote them, which is what mbgl hands back as well.
    pub fn iter(&self) -> impl Iterator<Item = (&str, Value)> + '_ {
        // Two arms of one iterator rather than a boxed `dyn`: this runs once per returned feature
        // per query, and the crate is `no_std` for its own code.
        let mvt = self
            .tags()
            .iter()
            .map(|(name, value)| (&**name, widen(value)));
        let json = match self {
            Self::Json(map) => Some(
                map.iter()
                    .map(|(name, value)| (name.as_str(), value.clone())),
            ),
            Self::Mvt { .. } => None,
        };
        mvt.chain(json.into_iter().flatten())
    }

    /// How many properties there are.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Mvt { .. } => self.tags().len(),
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
        Tags::Mvt {
            table: Arc::clone(self.property_table()),
            range: self.property_range(),
        }
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

/// One feature a rendered-feature query found.
///
/// A [`Queryable`] says what a feature is; this says where it was drawn from as well, which is what
/// a host acts on -- the same feature can be in two layers over one source, and "the POI layer's
/// bakery" is a different answer from "the label layer's bakery".
///
/// The vertex range is deliberately absent. It is an index into one bucket's buffer, which is a fact
/// about this frame's tiles and means nothing to a host: by the time it reads the answer the tile may
/// have been rebuilt at another zoom.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// The style layer that drew it.
    pub layer_id: String,
    /// The source that layer draws from, absent only for a background -- which has no features and
    /// is never a hit.
    pub source: Option<String>,
    /// The layer within a vector source, absent for a GeoJSON one.
    pub source_layer: Option<String>,
    /// The feature's id as its source gave it, which may be a string and may be absent.
    pub id: Option<Value>,
    /// `Point`, `LineString`, `Polygon` or `Unknown`.
    pub geometry_type: &'static str,
    /// Its properties.
    pub properties: Tags,
}
