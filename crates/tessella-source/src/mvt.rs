//! Mapbox Vector Tile decoding (§10 R1).
//!
//! # What a vector tile is
//!
//! A protobuf holding layers, each holding features, each holding a geometry encoded as
//! commands over a fixed-point grid. The grid is the layer's `extent` — 4096 by convention,
//! though the field exists precisely because it need not be — and coordinates are relative to
//! the tile's own corner, which is what makes a tile independent of where it is displayed.
//!
//! # Strictness, and where the spec draws it
//!
//! Unknown *protobuf* fields are skipped, because that is protobuf's rule and what makes the
//! encoding extensible. Unknown *vector tile* structure is refused: a layer with no name, or a
//! version this decoder does not implement, is a tile that cannot be drawn correctly, and
//! drawing it wrongly is worse than reporting it.
//!
//! The distinction matters more than it sounds. Several fixtures in the spec's own suite are
//! invalid as vector tiles while being well-formed protobuf, and a decoder that conflated the
//! two would reject tiles a newer writer is entitled to produce.
//!
//! # Geometry is commands, not coordinates
//!
//! `MoveTo`, `LineTo` and `ClosePath`, each with a repeat count, over zigzag deltas from the
//! previous point. A point layer is a run of `MoveTo`; a line is `MoveTo` then `LineTo`; a
//! polygon ring adds `ClosePath`. Decoding it wrongly does not produce slightly wrong shapes —
//! a mis-stepped cursor produces coordinates thousands of units away, which is at least loud.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Range;

use crate::protobuf::{Reader, WireError, WireType, zigzag};

/// The `extent` a layer has when it does not say.
pub const DEFAULT_EXTENT: u32 = 4096;

/// The only vector tile versions this decodes.
///
/// Version 1 differs in ways that matter — it does not require layer names to be unique and its
/// geometry rules are looser — so it is accepted but flagged rather than silently treated as 2.
pub const SUPPORTED_VERSIONS: [u32; 2] = [1, 2];

/// A tile that could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MvtError {
    /// The protobuf itself is malformed.
    #[error("malformed protobuf: {0}")]
    Wire(#[from] WireError),
    /// A layer carried no name, which is how features are addressed.
    #[error("a layer has no name")]
    LayerWithoutName,
    /// Two layers share a name, so a style naming it means two different things.
    #[error("duplicate layer name `{0}`")]
    DuplicateLayerName(String),
    /// A layer declared no version.
    #[error("layer `{layer}` declares no version")]
    LayerWithoutVersion {
        /// The layer's name.
        layer: String,
    },
    /// A version this build does not implement.
    #[error("layer `{layer}` is version {version}, which this build does not decode")]
    UnsupportedVersion {
        /// The layer's name.
        layer: String,
        /// The version it declared.
        version: u32,
    },
    /// A feature's tag list had an odd length, so a key has no value.
    #[error("a feature in `{layer}` has {count} tags, which is odd")]
    OddTags {
        /// The layer's name.
        layer: String,
        /// How many tag entries there were.
        count: usize,
    },
    /// A tag referenced a key or value index the layer does not have.
    #[error("a feature in `{layer}` references {kind} {index}, which the layer does not have")]
    TagOutOfRange {
        /// The layer's name.
        layer: String,
        /// `key` or `value`.
        kind: &'static str,
        /// The index referenced.
        index: u32,
    },
    /// A field the schema names carried the wrong wire type.
    ///
    /// Distinct from an unknown field, which is skipped. A field this decoder *knows*, arriving
    /// as the wrong type, is a writer that is broken rather than newer — and for an optional
    /// field with a default, skipping it silently substitutes the default, which for `extent`
    /// means every coordinate in the layer is silently at the wrong scale.
    #[error("layer `{layer}`: field {field} has the wrong wire type")]
    MistypedField {
        /// The layer's name, or empty if it is not known yet.
        layer: String,
        /// The field number.
        field: u32,
    },
    /// A `Value` carried none of its seven alternatives, or more than one.
    #[error("a value in `{layer}` sets {count} of its seven fields")]
    MalformedValue {
        /// The layer's name.
        layer: String,
        /// How many were set.
        count: usize,
    },
    /// The geometry commands did not decode.
    #[error("a feature in `{layer}` has malformed geometry: {detail}")]
    Geometry {
        /// The layer's name.
        layer: String,
        /// What went wrong.
        detail: &'static str,
    },
}

/// What a feature draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GeomType {
    /// Not stated, or a type this build does not know.
    ///
    /// Kept rather than refused: the spec reserves the right to add types, and a feature whose
    /// type is unknown is one to skip rather than a tile to reject.
    #[default]
    Unknown,
    /// One or more points.
    Point,
    /// One or more line strings.
    LineString,
    /// One or more polygons.
    Polygon,
}

/// A feature's property value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// A string.
    ///
    /// Shared rather than owned. A tile's string values live in one per-layer table and are
    /// referenced by index precisely so a value repeated across ten thousand features is stored
    /// once; copying one into every feature that mentions it throws that away, and it was
    /// measured doing so — see [`Tile::decode`].
    String(Arc<str>),
    /// A float, double, int, uint or sint — all of which a style sees as one number type.
    Number(f64),
    /// A boolean.
    Bool(bool),
}

/// A feature's rings, as one buffer with the boundaries beside it.
///
/// # Why not `Vec<Vec<[i32; 2]>>`
///
/// That is the obvious spelling and it allocates once per ring. Measured on the tile
/// maplibre-native benchmarks, a feature averages 6.6 rings of 7.2 points — so the obvious
/// spelling asks the allocator for 3937 vectors of 58 bytes to decode 593 features, which is
/// most of what decode does. One buffer and a list of ends is two allocations a feature however
/// many rings it has, and the rings come back out as slices of the same memory rather than as
/// separately-heap-allocated pieces of it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Geometry {
    /// Every point of every ring, in order.
    points: Vec<[i32; 2]>,
    /// Where each ring ends, exclusive. Ring `i` is `points[ends[i - 1]..ends[i]]`.
    ends: Vec<u32>,
}

impl Geometry {
    /// Builds from rings, for callers that already have them separately.
    #[must_use]
    pub fn from_rings(rings: impl IntoIterator<Item = Vec<[i32; 2]>>) -> Self {
        let mut geometry = Self::default();
        for ring in rings {
            geometry.push_ring(ring.iter().copied());
        }
        geometry
    }

    /// Appends one ring.
    pub fn push_ring(&mut self, points: impl IntoIterator<Item = [i32; 2]>) {
        self.points.extend(points);
        #[allow(clippy::cast_possible_truncation)]
        self.ends.push(self.points.len() as u32);
    }

    /// How many rings.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ends.len()
    }

    /// True when there are no rings.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// Every ring, as a slice of the shared buffer.
    pub fn rings(&self) -> impl Iterator<Item = &[[i32; 2]]> {
        let mut start = 0usize;
        self.ends.iter().map(move |&end| {
            let end = end as usize;
            let ring = &self.points[start..end];
            start = end;
            ring
        })
    }

    /// Total points across every ring.
    #[must_use]
    pub fn points(&self) -> usize {
        self.points.len()
    }

    /// Reserves room for `points` more points and `rings` more rings.
    fn reserve(&mut self, points: usize, rings: usize) {
        self.points.reserve(points);
        self.ends.reserve(rings);
    }
}

/// One feature.
///
/// Holds ranges rather than buffers. A tile's features are decoded together and read together,
/// so their points and properties live in one buffer per [`Layer`] and a feature says which part
/// of it is its own. Owning them per feature costs three allocations each — profiled at about a
/// sixth of the instructions a decode executes, for data that is never resized after decoding
/// and never outlives the layer.
///
/// Read one through [`Layer::feature`], which pairs it with the buffers it indexes.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Feature {
    /// The feature id, when it has one.
    pub id: Option<u64>,
    /// What it draws.
    pub geom_type: GeomType,
    /// This feature's slice of its layer's property list.
    properties: Range<u32>,
    /// This feature's slice of its layer's ring-end list.
    rings: Range<u32>,
}

/// A feature together with the layer whose buffers it indexes.
///
/// What a caller actually works with: the ranges on [`Feature`] mean nothing without the layer,
/// and pairing them here keeps that from being something every call site has to remember.
#[derive(Debug, Clone, Copy)]
pub struct FeatureRef<'a> {
    layer: &'a Layer,
    feature: &'a Feature,
}

impl<'a> FeatureRef<'a> {
    /// The feature id, when it has one.
    #[must_use]
    pub const fn id(&self) -> Option<u64> {
        self.feature.id
    }

    /// What it draws.
    #[must_use]
    pub const fn geom_type(&self) -> GeomType {
        self.feature.geom_type
    }

    /// Its properties, resolved against the layer's key and value tables.
    #[must_use]
    pub fn properties(&self) -> &'a [(Arc<str>, Value)] {
        let range = &self.feature.properties;
        &self.layer.properties[range.start as usize..range.end as usize]
    }

    /// Its rings, as slices of the layer's point buffer.
    pub fn rings(&self) -> impl Iterator<Item = &'a [[i32; 2]]> {
        let ends = &self.layer.ends;
        let range = self.feature.rings.clone();
        // A ring starts where the one before it ended, and the ends are monotonic across the
        // whole layer — so the first ring of a feature starts at its predecessor's end rather
        // than at zero.
        let mut start = if range.start == 0 {
            0usize
        } else {
            ends[range.start as usize - 1] as usize
        };
        let points = &self.layer.points;
        ends[range.start as usize..range.end as usize]
            .iter()
            .map(move |&end| {
                let end = end as usize;
                let ring = &points[start..end];
                start = end;
                ring
            })
    }

    /// How many rings.
    #[must_use]
    pub fn ring_count(&self) -> usize {
        (self.feature.rings.end - self.feature.rings.start) as usize
    }

    /// Its rings, scaled from the layer's extent onto `target`.
    ///
    /// A vector tile states its own grid, and a tile written at 8192 draws at exactly half scale
    /// against one written at 4096 unless somebody divides. So the conversion happens once,
    /// here, rather than every consumer carrying a divisor it might forget. A forgotten divisor
    /// is a layer at the wrong scale, which renders as geometry in the right shape and the wrong
    /// place.
    ///
    /// Rounded rather than truncated, for the reason the GeoJSON path rounds: truncation drifts
    /// every coordinate the same direction, which reads as a projection error rather than as
    /// rounding.
    #[must_use]
    pub fn rings_scaled(&self, target: i32) -> Geometry {
        let from_extent = self.layer.extent;
        if from_extent == 0 {
            return Geometry::default();
        }
        let scale = f64::from(target) / f64::from(from_extent);
        let mut out = Geometry::default();
        out.reserve(self.rings().map(<[[i32; 2]]>::len).sum(), self.ring_count());
        for ring in self.rings() {
            out.push_ring(ring.iter().map(|point| {
                #[allow(clippy::cast_possible_truncation)]
                [
                    (f64::from(point[0]) * scale).round() as i32,
                    (f64::from(point[1]) * scale).round() as i32,
                ]
            }));
        }
        out
    }
}

/// One layer.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Layer {
    /// Layer name, which is what a style's `source-layer` matches.
    pub name: String,
    /// The tile grid this layer's coordinates are in.
    pub extent: u32,
    /// The spec version it declared.
    pub version: u32,
    /// Its features, each holding ranges into the buffers below.
    features: Vec<Feature>,
    /// Every point of every ring of every feature, in feature order.
    points: Vec<[i32; 2]>,
    /// Where each ring ends in `points`, exclusive and monotonic across the whole layer.
    ends: Vec<u32>,
    /// Every property of every feature, in feature order.
    properties: Vec<(Arc<str>, Value)>,
}

impl Layer {
    /// How many features.
    #[must_use]
    pub fn len(&self) -> usize {
        self.features.len()
    }

    /// True when the layer has no features.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.features.is_empty()
    }

    /// One feature, paired with the buffers it indexes.
    #[must_use]
    pub fn feature(&self, index: usize) -> Option<FeatureRef<'_>> {
        self.features.get(index).map(|feature| FeatureRef {
            layer: self,
            feature,
        })
    }

    /// Every feature, in the order the tile carried them.
    pub fn features(&self) -> impl Iterator<Item = FeatureRef<'_>> {
        self.features.iter().map(move |feature| FeatureRef {
            layer: self,
            feature,
        })
    }

    /// A layer with no features yet.
    #[must_use]
    pub fn new(name: String, extent: u32, version: u32) -> Self {
        Self {
            name,
            extent,
            version,
            ..Self::default()
        }
    }

    /// Appends a feature whose geometry was decoded straight into this layer.
    ///
    /// `rings` is where its ring ends begin, from [`Self::open_rings`] before decoding started.
    fn finish_feature(
        &mut self,
        id: Option<u64>,
        geom_type: GeomType,
        properties: impl IntoIterator<Item = (Arc<str>, Value)>,
        rings: u32,
    ) {
        #[allow(clippy::cast_possible_truncation)]
        let property_start = self.properties.len() as u32;
        self.properties.extend(properties);
        #[allow(clippy::cast_possible_truncation)]
        let property_end = self.properties.len() as u32;
        #[allow(clippy::cast_possible_truncation)]
        let ring_end = self.ends.len() as u32;
        self.features.push(Feature {
            id,
            geom_type,
            properties: property_start..property_end,
            rings: rings..ring_end,
        });
    }

    /// Where the next feature's rings will start.
    #[allow(clippy::cast_possible_truncation)]
    fn open_rings(&self) -> u32 {
        self.ends.len() as u32
    }

    /// Appends a feature, copying its parts into the layer's buffers.
    ///
    /// Public because a layer's fields are not: a caller assembling one — a test, or a source
    /// that is not a `.mvt` — cannot write the ranges itself and should not have to know they
    /// exist.
    pub fn push_feature(
        &mut self,
        id: Option<u64>,
        geom_type: GeomType,
        properties: impl IntoIterator<Item = (Arc<str>, Value)>,
        geometry: &Geometry,
    ) {
        #[allow(clippy::cast_possible_truncation)]
        let property_start = self.properties.len() as u32;
        self.properties.extend(properties);
        #[allow(clippy::cast_possible_truncation)]
        let property_end = self.properties.len() as u32;

        #[allow(clippy::cast_possible_truncation)]
        let ring_start = self.ends.len() as u32;
        // The feature's ends are relative to its own buffer; the layer's are absolute, so they
        // shift by wherever this feature's points begin.
        #[allow(clippy::cast_possible_truncation)]
        let base = self.points.len() as u32;
        self.points.extend_from_slice(&geometry.points);
        self.ends.extend(geometry.ends.iter().map(|end| end + base));
        #[allow(clippy::cast_possible_truncation)]
        let ring_end = self.ends.len() as u32;

        self.features.push(Feature {
            id,
            geom_type,
            properties: property_start..property_end,
            rings: ring_start..ring_end,
        });
    }
}

/// A decoded tile.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Tile {
    /// Layers, in the order the tile carried them.
    pub layers: Vec<Layer>,
}

impl Tile {
    /// Decodes a vector tile.
    ///
    /// # Errors
    ///
    /// [`MvtError`] when the protobuf is malformed or the tile is not a valid vector tile.
    pub fn decode(data: &[u8]) -> Result<Self, MvtError> {
        let mut reader = Reader::new(data);
        let mut layers: Vec<Layer> = Vec::new();

        while let Some(field) = reader.next_field() {
            let (number, wire) = field?;
            match (number, wire) {
                // Tile.layers
                (3, WireType::Delimited) => {
                    let layer = decode_layer(reader.delimited()?)?;
                    if layers.iter().any(|existing| existing.name == layer.name) {
                        return Err(MvtError::DuplicateLayerName(layer.name));
                    }
                    layers.push(layer);
                }
                _ => reader.skip(wire)?,
            }
        }
        Ok(Self { layers })
    }

    /// A layer by name.
    #[must_use]
    pub fn layer(&self, name: &str) -> Option<&Layer> {
        self.layers.iter().find(|layer| layer.name == name)
    }
}

fn decode_layer(data: &[u8]) -> Result<Layer, MvtError> {
    let mut reader = Reader::new(data);
    let mut name: Option<String> = None;
    let mut version: Option<u32> = None;
    let mut extent: Option<u32> = None;
    let mut keys: Vec<Arc<str>> = Vec::new();
    let mut values: Vec<Value> = Vec::new();
    // Features are decoded after the tables, because a feature's tags index into them and the
    // spec does not require the tables to come first.
    let mut raw_features: Vec<&[u8]> = Vec::new();

    while let Some(field) = reader.next_field() {
        let (number, wire) = field?;
        match (number, wire) {
            (1, WireType::Delimited) => {
                name = Some(String::from_utf8_lossy(reader.delimited()?).into_owned());
            }
            (2, WireType::Delimited) => raw_features.push(reader.delimited()?),
            (3, WireType::Delimited) => {
                keys.push(Arc::from(
                    String::from_utf8_lossy(reader.delimited()?).as_ref(),
                ));
            }
            (4, WireType::Delimited) => values.push(decode_value(reader.delimited()?, "")?),
            (5, WireType::Varint) => {
                #[allow(clippy::cast_possible_truncation)]
                {
                    extent = Some(reader.varint()? as u32);
                }
            }
            // `extent` is optional with a default, so skipping a mistyped one would quietly
            // substitute 4096 and put the whole layer at the wrong scale. The required fields
            // fail on their own by ending up absent; this one has to be caught here.
            (5, _) => {
                return Err(MvtError::MistypedField {
                    layer: name.unwrap_or_default(),
                    field: 5,
                });
            }
            (15, WireType::Varint) => {
                #[allow(clippy::cast_possible_truncation)]
                {
                    version = Some(reader.varint()? as u32);
                }
            }
            _ => reader.skip(wire)?,
        }
    }

    let name = name.ok_or(MvtError::LayerWithoutName)?;
    let version = version.ok_or_else(|| MvtError::LayerWithoutVersion {
        layer: name.clone(),
    })?;
    if !SUPPORTED_VERSIONS.contains(&version) {
        return Err(MvtError::UnsupportedVersion {
            layer: name,
            version,
        });
    }

    let mut out = Layer {
        name,
        // The default is the spec's, and it is 4096 rather than the 8192 mbgl uses internally:
        // the tile says what grid it is on, and the frontend rescales.
        extent: extent.unwrap_or(DEFAULT_EXTENT),
        version,
        features: Vec::with_capacity(raw_features.len()),
        ..Layer::default()
    };
    // One reservation for the layer, from the bytes its features occupy. A point costs at least
    // two varints of at least one byte, so half the total bounds it — and growing the shared
    // buffer per feature instead copies everything already in it each time it doubles, which is
    // the cost that reappeared as `memcpy` the moment the buffers became per-layer.
    let bytes: usize = raw_features.iter().map(|raw| raw.len()).sum();
    out.points.reserve(bytes / 2);
    out.ends.reserve(raw_features.len() * 2);
    out.properties.reserve(raw_features.len() * 2);

    let mut scratch = Scratch::default();
    let name = out.name.clone();
    for raw in raw_features {
        decode_feature(raw, &name, &keys, &values, &mut scratch, &mut out)?;
    }

    Ok(out)
}

fn decode_value(data: &[u8], layer: &str) -> Result<Value, MvtError> {
    let mut reader = Reader::new(data);
    let mut found: Option<Value> = None;
    let mut count = 0;

    while let Some(field) = reader.next_field() {
        let (number, wire) = field?;
        let value = match (number, wire) {
            (1, WireType::Delimited) => Value::String(Arc::from(
                String::from_utf8_lossy(reader.delimited()?).as_ref(),
            )),
            (2, WireType::Fixed32) => Value::Number(f64::from(f32::from_bits(reader.fixed32()?))),
            (3, WireType::Fixed64) => Value::Number(f64::from_bits(reader.fixed64()?)),
            #[allow(clippy::cast_possible_wrap, clippy::cast_precision_loss)]
            (4, WireType::Varint) => Value::Number(reader.varint()? as i64 as f64),
            #[allow(clippy::cast_precision_loss)]
            (5, WireType::Varint) => Value::Number(reader.varint()? as f64),
            #[allow(clippy::cast_possible_truncation)]
            (6, WireType::Varint) => {
                let raw = reader.varint()?;
                // 64-bit zigzag, which the geometry helper does not cover.
                #[allow(clippy::cast_possible_wrap, clippy::cast_precision_loss)]
                Value::Number((((raw >> 1) as i64) ^ -((raw & 1) as i64)) as f64)
            }
            (7, WireType::Varint) => Value::Bool(reader.varint()? != 0),
            _ => {
                reader.skip(wire)?;
                continue;
            }
        };
        count += 1;
        found = Some(value);
    }

    // Exactly one of the seven, which is what a proto2 optional-union means here. Neither zero
    // nor several is a value a style could read.
    if count != 1 {
        return Err(MvtError::MalformedValue {
            layer: layer.into(),
            count,
        });
    }
    found.ok_or(MvtError::MalformedValue {
        layer: layer.into(),
        count,
    })
}

/// Buffers reused across the features of a layer.
///
/// A feature's tags and geometry commands are packed varints that have to be unpacked before
/// they can be read, and both were a fresh `Vec` per feature grown by pushing — so a tile of
/// seventeen thousand features allocated and freed thirty-four thousand vectors, and reallocated
/// each of them a few times on the way up. Neither buffer outlives the feature it decodes, which
/// is exactly the shape a scratch buffer fits: cleared and refilled rather than rebuilt.
#[derive(Default)]
struct Scratch {
    tags: Vec<u32>,
    geometry: Vec<u32>,
    properties: Vec<(Arc<str>, Value)>,
}

fn decode_feature(
    data: &[u8],
    name: &str,
    keys: &[Arc<str>],
    values: &[Value],
    scratch: &mut Scratch,
    out: &mut Layer,
) -> Result<(), MvtError> {
    let layer = name;
    let mut reader = Reader::new(data);
    let mut id: Option<u64> = None;
    let mut geom_type = GeomType::Unknown;
    // Cleared, not reallocated: `clear` keeps the capacity, so after the first few features
    // neither of these grows again.
    scratch.tags.clear();
    scratch.geometry.clear();
    let (tags, geometry) = (&mut scratch.tags, &mut scratch.geometry);
    let mut geometry_fields = 0;

    while let Some(field) = reader.next_field() {
        let (number, wire) = field?;
        match (number, wire) {
            (1, WireType::Varint) => id = Some(reader.varint()?),
            (2, _) => reader.packed_varints(wire, tags)?,
            (3, WireType::Varint) => {
                geom_type = match reader.varint()? {
                    1 => GeomType::Point,
                    2 => GeomType::LineString,
                    3 => GeomType::Polygon,
                    // Includes 0 and anything the spec adds later.
                    _ => GeomType::Unknown,
                };
            }
            (4, _) => {
                geometry_fields += 1;
                reader.packed_varints(wire, geometry)?;
            }
            _ => reader.skip(wire)?,
        }
    }

    // The geometry field is `repeated uint32 [packed]`, so a conforming writer emits it once.
    // Two of them is a feature with two geometries, which the spec forbids and which would
    // otherwise decode as one long concatenated command stream.
    if geometry_fields > 1 {
        return Err(MvtError::Geometry {
            layer: layer.into(),
            detail: "more than one geometry field",
        });
    }

    if !tags.len().is_multiple_of(2) {
        return Err(MvtError::OddTags {
            layer: layer.into(),
            count: tags.len(),
        });
    }
    // Two entries per property, and the count is known before any of them is read.
    scratch.properties.clear();
    scratch.properties.reserve(tags.len() / 2);
    for [key, value] in tags.as_chunks::<2>().0 {
        let (key, value) = (*key, *value);
        let key = keys
            .get(key as usize)
            .ok_or(MvtError::TagOutOfRange {
                layer: layer.into(),
                kind: "key",
                index: key,
            })?
            .clone();
        let value = values
            .get(value as usize)
            .ok_or(MvtError::TagOutOfRange {
                layer: layer.into(),
                kind: "value",
                index: value,
            })?
            .clone();
        scratch.properties.push((key, value));
    }

    let rings = out.open_rings();
    // Straight into the layer's buffers. Decoding into a per-feature `Geometry` and copying it
    // in afterwards allocates once per feature and then memcpys every coordinate — which is the
    // pair of costs this arrangement exists to remove, so paying them on the way in would be
    // most of the point thrown away.
    decode_geometry_into(geometry, layer, &mut out.points, &mut out.ends)?;
    out.finish_feature(id, geom_type, scratch.properties.drain(..), rings);
    Ok(())
}

/// Walks the command stream into rings.
/// Walks the command stream into rings, appending into buffers that may already hold others.
///
/// The ring ends are absolute offsets into `points`, and monotonic across everything already
/// there — so "the ring being built" is whatever follows the last recorded end, whether that end
/// belongs to this feature or the one before it.
fn decode_geometry_into(
    commands: &[u32],
    layer: &str,
    points: &mut Vec<[i32; 2]>,
    ends: &mut Vec<u32>,
) -> Result<(), MvtError> {
    let malformed = |detail: &'static str| MvtError::Geometry {
        layer: layer.into(),
        detail,
    };

    // Once, from the command stream's own length, rather than per ring. A point costs at least
    // two varints of at least one byte each, so this bounds the count — and reserving per
    // `MoveTo` instead means a feature of eighteen rings reallocates its buffers eighteen times
    // on the way up.
    // Everything after the last recorded end is the ring being built.
    let open_at = |points: &Vec<[i32; 2]>, ends: &Vec<u32>| -> usize {
        ends.last().map_or(0, |&end| end as usize).min(points.len())
    };
    let close = |points: &Vec<[i32; 2]>, ends: &mut Vec<u32>| {
        if points.len() > open_at(points, ends) {
            #[allow(clippy::cast_possible_truncation)]
            ends.push(points.len() as u32);
        }
    };
    let (mut x, mut y) = (0i32, 0i32);
    let mut index = 0;

    while index < commands.len() {
        let header = commands[index];
        index += 1;
        let id = header & 0x7;
        let count = (header >> 3) as usize;

        match id {
            // MoveTo starts a new ring or emits a point.
            1 | 2 => {
                if id == 1 {
                    close(points, ends);
                }
                if id == 2 && points.len() == open_at(points, ends) {
                    return Err(malformed("LineTo before any MoveTo"));
                }
                for _ in 0..count {
                    let (dx, dy) = (
                        *commands.get(index).ok_or_else(|| malformed("truncated"))?,
                        *commands
                            .get(index + 1)
                            .ok_or_else(|| malformed("truncated"))?,
                    );
                    index += 2;
                    // Deltas accumulate, so a wrapping add here would place a point on the far
                    // side of the world rather than reporting a broken tile.
                    x = x
                        .checked_add(zigzag(dx))
                        .ok_or_else(|| malformed("coordinate overflow"))?;
                    y = y
                        .checked_add(zigzag(dy))
                        .ok_or_else(|| malformed("coordinate overflow"))?;
                    points.push([x, y]);
                    // Each MoveTo in a run starts its own geometry, which is how a multipoint
                    // travels: one command with a count, not one command each.
                    if id == 1 && count > 1 {
                        close(points, ends);
                    }
                }
            }
            // ClosePath takes no parameters and closes the ring by repeating its first point.
            7 => {
                if count != 1 {
                    return Err(malformed("ClosePath with a count other than one"));
                }
                let open = &points[open_at(points, ends)..];
                let first = *open
                    .first()
                    .ok_or_else(|| malformed("ClosePath with no ring"))?;
                let last = open.last().copied();
                // Only when it is not already closed. The spec says a ring should not repeat
                // its first point, since ClosePath implies it — but real tiles do return to the
                // start explicitly, and appending unconditionally then leaves a zero-length edge
                // at the seam of every ring.
                //
                // That is not cosmetic: a degenerate edge is what makes ear-clipping spin.
                if last != Some(first) {
                    points.push(first);
                }
                close(points, ends);
            }
            _ => return Err(malformed("unknown geometry command")),
        }
    }

    close(points, ends);
    Ok(())
}

use tessella_style::Value as StyleValue;
use tessella_style::expression::Feature as StyleFeature;

impl StyleFeature for FeatureRef<'_> {
    fn property(&self, key: &str) -> Option<StyleValue> {
        self.properties()
            .iter()
            .find(|(name, _)| &**name == key)
            .map(|(_, value)| match value {
                Value::String(text) => StyleValue::String(text.as_ref().into()),
                Value::Number(number) => StyleValue::Number(*number),
                Value::Bool(flag) => StyleValue::Bool(*flag),
            })
    }

    fn geometry_type(&self) -> &str {
        // The names the spec's `geometry-type` expression produces. A tile's `Unknown` maps to
        // the same string a GeoJSON feature of no recognised type would give, so a filter reads
        // one rule for both sources.
        match self.geom_type() {
            GeomType::Point => "Point",
            GeomType::LineString => "LineString",
            GeomType::Polygon => "Polygon",
            GeomType::Unknown => "Unknown",
        }
    }

    fn id(&self) -> Option<StyleValue> {
        #[allow(clippy::cast_precision_loss)]
        self.id().map(|id| StyleValue::Number(id as f64))
    }

    fn properties(&self) -> StyleValue {
        StyleValue::Object(
            self.properties()
                .iter()
                .map(|(key, value)| {
                    let value = match value {
                        Value::String(text) => StyleValue::String(text.as_ref().into()),
                        Value::Number(number) => StyleValue::Number(*number),
                        Value::Bool(flag) => StyleValue::Bool(*flag),
                    };
                    (String::from(key.as_ref()), value)
                })
                .collect(),
        )
    }
}
