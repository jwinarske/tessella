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

    /// Where the ring currently being built starts.
    fn open_at(&self) -> usize {
        self.ends.last().map_or(0, |&end| end as usize)
    }

    /// The ring being built, which is everything after the last closed one.
    ///
    /// Decoding appends straight into the shared buffer and closes a ring by recording where it
    /// ended. The alternative — accumulate a ring in its own vector and copy it in — writes
    /// every coordinate of the tile twice, and a tile is tens of thousands of them.
    fn open(&self) -> &[[i32; 2]] {
        &self.points[self.open_at()..]
    }

    /// Appends a point to the ring being built.
    fn push_point(&mut self, point: [i32; 2]) {
        self.points.push(point);
    }

    /// Ends the ring being built, if it has anything in it.
    fn close_ring(&mut self) {
        if self.points.len() > self.open_at() {
            #[allow(clippy::cast_possible_truncation)]
            self.ends.push(self.points.len() as u32);
        }
    }
}

/// One feature.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Feature {
    /// The feature id, when it has one.
    pub id: Option<u64>,
    /// What it draws.
    pub geom_type: GeomType,
    /// Properties, resolved against the layer's key and value tables.
    ///
    /// The key is shared with the table it came from: a layer names its keys once and every
    /// feature refers to the same ones, so an owned `String` per feature per tag is a copy of
    /// something that already exists.
    pub properties: Vec<(Arc<str>, Value)>,
    /// Geometry in tile-local units, as rings or line strings.
    pub geometry: Geometry,
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
    /// Its features.
    pub features: Vec<Feature>,
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

    let mut features = Vec::with_capacity(raw_features.len());
    let mut scratch = Scratch::default();
    for raw in raw_features {
        features.push(decode_feature(raw, &name, &keys, &values, &mut scratch)?);
    }

    Ok(Layer {
        name,
        // The default is the spec's, and it is 4096 rather than the 8192 mbgl uses internally:
        // the tile says what grid it is on, and the frontend rescales.
        extent: extent.unwrap_or(DEFAULT_EXTENT),
        version,
        features,
    })
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
}

fn decode_feature(
    data: &[u8],
    layer: &str,
    keys: &[Arc<str>],
    values: &[Value],
    scratch: &mut Scratch,
) -> Result<Feature, MvtError> {
    let mut reader = Reader::new(data);
    let mut feature = Feature::default();
    // Cleared, not reallocated: `clear` keeps the capacity, so after the first few features
    // neither of these grows again.
    scratch.tags.clear();
    scratch.geometry.clear();
    let (tags, geometry) = (&mut scratch.tags, &mut scratch.geometry);
    let mut geometry_fields = 0;

    while let Some(field) = reader.next_field() {
        let (number, wire) = field?;
        match (number, wire) {
            (1, WireType::Varint) => feature.id = Some(reader.varint()?),
            (2, _) => reader.packed_varints(wire, tags)?,
            (3, WireType::Varint) => {
                feature.geom_type = match reader.varint()? {
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
    feature.properties.reserve_exact(tags.len() / 2);
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
        feature.properties.push((key, value));
    }

    feature.geometry = decode_geometry(geometry, layer)?;
    Ok(feature)
}

/// Walks the command stream into rings.
fn decode_geometry(commands: &[u32], layer: &str) -> Result<Geometry, MvtError> {
    let malformed = |detail: &'static str| MvtError::Geometry {
        layer: layer.into(),
        detail,
    };

    let mut out = Geometry::default();
    // Once, from the command stream's own length, rather than per ring. A point costs at least
    // two varints of at least one byte each, so this bounds the count — and reserving per
    // `MoveTo` instead means a feature of eighteen rings reallocates its buffers eighteen times
    // on the way up.
    out.reserve(commands.len() / 2, 4);
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
                    out.close_ring();
                }
                if id == 2 && out.open().is_empty() {
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
                    out.push_point([x, y]);
                    // Each MoveTo in a run starts its own geometry, which is how a multipoint
                    // travels: one command with a count, not one command each.
                    if id == 1 && count > 1 {
                        out.close_ring();
                    }
                }
            }
            // ClosePath takes no parameters and closes the ring by repeating its first point.
            7 => {
                if count != 1 {
                    return Err(malformed("ClosePath with a count other than one"));
                }
                let open = out.open();
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
                    out.push_point(first);
                }
                out.close_ring();
            }
            _ => return Err(malformed("unknown geometry command")),
        }
    }

    out.close_ring();
    Ok(out)
}

use tessella_style::Value as StyleValue;
use tessella_style::expression::Feature as StyleFeature;

impl Feature {
    /// The feature's rings, rescaled from the layer's grid onto `target`.
    ///
    /// # Why rescaling rather than reading the extent everywhere
    ///
    /// A tile states its own grid, and 4096 is only a convention — the field exists because it
    /// need not be. The rest of this frontend works on one grid (§ tiling's `EXTENT`), so the
    /// conversion happens once, here, rather than every consumer carrying a divisor it might
    /// forget. A forgotten divisor is a layer at the wrong scale, which renders as geometry in
    /// the right shape and the wrong place.
    ///
    /// Rounded rather than truncated, for the reason the GeoJSON path rounds: truncation drifts
    /// every coordinate the same direction, which reads as a projection error rather than as
    /// rounding.
    #[must_use]
    pub fn rings_scaled(&self, from_extent: u32, target: i32) -> Geometry {
        if from_extent == 0 {
            return Geometry::default();
        }
        let scale = f64::from(target) / f64::from(from_extent);
        let mut out = Geometry::default();
        out.reserve(self.geometry.points(), self.geometry.len());
        for ring in self.geometry.rings() {
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

impl StyleFeature for Feature {
    fn property(&self, key: &str) -> Option<StyleValue> {
        self.properties
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
        match self.geom_type {
            GeomType::Point => "Point",
            GeomType::LineString => "LineString",
            GeomType::Polygon => "Polygon",
            GeomType::Unknown => "Unknown",
        }
    }

    fn id(&self) -> Option<StyleValue> {
        #[allow(clippy::cast_precision_loss)]
        self.id.map(|id| StyleValue::Number(id as f64))
    }

    fn properties(&self) -> StyleValue {
        StyleValue::Object(
            self.properties
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
