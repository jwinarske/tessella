//! GeoJSON features, read from a style's inline data.
//!
//! # Singular and multi are the same type
//!
//! GeoJSON distinguishes `Point` from `MultiPoint`, `LineString` from `MultiLineString`, and
//! `Polygon` from `MultiPolygon`. The style spec does not: a filter asking
//! `["==", "$type", "Polygon"]` matches a MultiPolygon too, because mbgl's feature type has
//! exactly three values and both collapse onto `Polygon`.
//!
//! So the singular forms are normalized into the multi forms on the way in. A `Point` becomes a
//! one-point list, a `Polygon` becomes a one-polygon list. Downstream code then has one shape
//! to handle per type instead of two, and — more to the point — cannot accidentally treat a
//! MultiPolygon as something a `Polygon` filter should miss.
//!
//! # Why this walks the style value rather than using the `geojson` crate
//!
//! Inline data arrives already parsed, as part of the style document. Handing it to a GeoJSON
//! parser would mean serializing it back to text and parsing it again to reach the same tree.
//! The crate earns its place on the URL path, where bytes arrive separately and have to be
//! parsed from scratch; it does not earn it here.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use tessella_style::Value;
use tessella_style::expression::Feature;

/// A position. Elevation is dropped: nothing in the pipeline reads it, and carrying it would
/// widen every vertex for a value that never reaches the GPU.
pub type Position = [f64; 2];

/// A closed ring of positions.
pub type Ring = Vec<Position>;

/// A polygon: an exterior ring followed by any interior rings.
pub type PolygonRings = Vec<Ring>;

/// A feature's geometry, with singular forms folded into their multi equivalents.
#[derive(Debug, Clone, PartialEq)]
pub enum Geometry {
    /// One or more points.
    Point(Vec<Position>),
    /// One or more lines.
    LineString(Vec<Vec<Position>>),
    /// One or more polygons, each a list of rings.
    Polygon(Vec<PolygonRings>),
}

impl Geometry {
    /// The type name the style spec uses, which is what `$type` and `["geometry-type"]` compare
    /// against.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Point(_) => "Point",
            Self::LineString(_) => "LineString",
            Self::Polygon(_) => "Polygon",
        }
    }

    /// True when the geometry carries no positions at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Point(points) => points.is_empty(),
            Self::LineString(lines) => lines.iter().all(Vec::is_empty),
            Self::Polygon(polygons) => polygons.iter().all(|rings| rings.iter().all(Vec::is_empty)),
        }
    }
}

/// A GeoJSON feature.
#[derive(Debug, Clone, PartialEq)]
pub struct GeoJsonFeature {
    /// The feature id, if it has one. Read by `$id` filters and `["id"]`.
    pub id: Option<Value>,
    /// The feature's properties.
    pub properties: BTreeMap<String, Value>,
    /// Its geometry.
    pub geometry: Geometry,
}

impl Feature for GeoJsonFeature {
    fn property(&self, key: &str) -> Option<Value> {
        self.properties.get(key).cloned()
    }

    fn geometry_type(&self) -> &str {
        self.geometry.type_name()
    }

    fn id(&self) -> Option<Value> {
        self.id.clone()
    }

    fn properties(&self) -> Value {
        Value::Object(self.properties.clone())
    }
}

/// GeoJSON that could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GeoJsonError {
    /// A value that should have been an object was not.
    #[error("expected a GeoJSON object, got {0}")]
    NotAnObject(&'static str),
    /// The `type` member is missing or is not a string.
    #[error("a GeoJSON object needs a string `type`")]
    MissingType,
    /// A geometry type this build does not implement.
    ///
    /// `GeometryCollection` is the one that matters: mbgl does not carry it into tiles either,
    /// and silently dropping it would lose features without saying so.
    #[error("GeoJSON type `{0}` is not implemented")]
    UnsupportedType(String),
    /// Coordinates that are not shaped like the geometry type says.
    #[error("`{type_name}` coordinates are malformed: {detail}")]
    Coordinates {
        /// The geometry type being read.
        type_name: String,
        /// What was wrong.
        detail: String,
    },
}

/// Reads a GeoJSON value into features.
///
/// Accepts a `FeatureCollection`, a bare `Feature`, or a bare geometry, which are the three
/// things a style's `data` member is allowed to be.
///
/// # Errors
///
/// [`GeoJsonError`] when the value is not GeoJSON this build can read.
pub fn read(value: &Value) -> Result<Vec<GeoJsonFeature>, GeoJsonError> {
    let type_name = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or(GeoJsonError::MissingType)?;

    match type_name {
        "FeatureCollection" => {
            let features = value
                .get("features")
                .and_then(Value::as_array)
                .ok_or_else(|| GeoJsonError::Coordinates {
                    type_name: "FeatureCollection".to_string(),
                    detail: "`features` must be an array".to_string(),
                })?;
            features.iter().map(read_feature).collect()
        }
        "Feature" => Ok(vec![read_feature(value)?]),
        // A bare geometry is a feature with no id and no properties.
        _ => Ok(vec![GeoJsonFeature {
            id: None,
            properties: BTreeMap::new(),
            geometry: read_geometry(value)?,
        }]),
    }
}

fn read_feature(value: &Value) -> Result<GeoJsonFeature, GeoJsonError> {
    let object = value
        .as_object()
        .ok_or(GeoJsonError::NotAnObject(value.type_name()))?;

    // A null geometry is legal GeoJSON and means a feature with no location. Nothing can be
    // drawn from it, so it is rejected here rather than carried as an empty shape that would
    // silently contribute no vertices later.
    let geometry = object
        .get("geometry")
        .filter(|value| **value != Value::Null)
        .ok_or_else(|| GeoJsonError::Coordinates {
            type_name: "Feature".to_string(),
            detail: "a feature needs a geometry".to_string(),
        })?;

    let properties = match object.get("properties") {
        Some(Value::Object(entries)) => entries.clone(),
        // `properties` is required by the spec but nullable, and styles write it both ways.
        _ => BTreeMap::new(),
    };

    Ok(GeoJsonFeature {
        id: object.get("id").filter(|v| **v != Value::Null).cloned(),
        properties,
        geometry: read_geometry(geometry)?,
    })
}

fn read_geometry(value: &Value) -> Result<Geometry, GeoJsonError> {
    let type_name = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or(GeoJsonError::MissingType)?;
    let coordinates = value.get("coordinates").unwrap_or(&Value::Null);

    match type_name {
        "Point" => Ok(Geometry::Point(vec![position(type_name, coordinates)?])),
        "MultiPoint" => Ok(Geometry::Point(positions(type_name, coordinates)?)),
        "LineString" => Ok(Geometry::LineString(vec![positions(
            type_name,
            coordinates,
        )?])),
        "MultiLineString" => Ok(Geometry::LineString(list(
            type_name,
            coordinates,
            |item| positions(type_name, item),
        )?)),
        "Polygon" => Ok(Geometry::Polygon(vec![rings(type_name, coordinates)?])),
        "MultiPolygon" => Ok(Geometry::Polygon(list(type_name, coordinates, |item| {
            rings(type_name, item)
        })?)),
        other => Err(GeoJsonError::UnsupportedType(other.to_string())),
    }
}

fn malformed(type_name: &str, detail: &str) -> GeoJsonError {
    GeoJsonError::Coordinates {
        type_name: type_name.to_string(),
        detail: detail.to_string(),
    }
}

fn list<T>(
    type_name: &str,
    value: &Value,
    mut read: impl FnMut(&Value) -> Result<T, GeoJsonError>,
) -> Result<Vec<T>, GeoJsonError> {
    value
        .as_array()
        .ok_or_else(|| malformed(type_name, "coordinates must be an array"))?
        .iter()
        .map(&mut read)
        .collect()
}

/// One polygon's rings, wound the way a vector tile winds them.
///
/// # Why the direction is not free
///
/// A ring's *order* is geometry to an extrusion in a way it never is to a fill. The wall builder
/// walks the outline and takes each edge's perpendicular as that wall's outward facing, and the
/// quad it winds from the edge is what back-face culling reads. Reverse the ring and both
/// invert: the walls are lit from behind, and culling takes one triangle of every quad, which
/// draws a building as a band across its top with a diagonal sliver down each side.
///
/// Vector tiles carry one winding and so never showed this. A GeoJSON polygon is written by
/// hand, and RFC 7946 §3.1.6 asks for the *opposite* one -- exterior rings counter-clockwise --
/// so the spec-conformant way to write a building was the way that drew it broken. One white
/// cube read 19742 gross pixels against the oracle, and 0 with its ring reversed.
///
/// mbgl normalizes in the same place, before any bucket sees the ring: traced through
/// `FillExtrusionBucket::addFeature`, it reports the same shoelace, `+17784648`, for a polygon
/// written either way. Doing it here rather than in the extrusion builder is what keeps a vector
/// tile's rings untouched -- and with them the edge distances a `fill-extrusion-pattern` wraps
/// against, which reversing a ring would re-phase.
///
/// Every ring turns together, so a hole stays a hole: `classify_rings` tells an interior ring
/// from an exterior one by the sign it does *not* share, and flipping only the exterior would
/// leave the two indistinguishable.
fn rings(type_name: &str, value: &Value) -> Result<PolygonRings, GeoJsonError> {
    let mut rings: PolygonRings = list(type_name, value, |ring| positions(type_name, ring))?;
    // Positive is counter-clockwise in longitude and latitude, which is RFC 7946's exterior and
    // the one to turn. Web Mercator flips the vertical axis, so it is the ring that reads
    // *clockwise* on a map that arrives wound the way a tile is.
    if rings
        .first()
        .is_some_and(|exterior| shoelace(exterior) > 0.0)
    {
        for ring in &mut rings {
            ring.reverse();
        }
    }
    Ok(rings)
}

/// Twice a ring's signed area, in longitude and latitude.
///
/// Only the sign is read, so the factor of two and the units are beside the point. A ring of
/// fewer than three positions encloses nothing and gets zero, which leaves it alone.
fn shoelace(ring: &[Position]) -> f64 {
    let len = ring.len();
    if len < 3 {
        return 0.0;
    }
    let mut sum = 0.0;
    for index in 0..len {
        let here = ring[index];
        let next = ring[(index + 1) % len];
        sum += here[0] * next[1] - next[0] * here[1];
    }
    sum
}

fn positions(type_name: &str, value: &Value) -> Result<Vec<Position>, GeoJsonError> {
    list(type_name, value, |point| position(type_name, point))
}

fn position(type_name: &str, value: &Value) -> Result<Position, GeoJsonError> {
    let items = value
        .as_array()
        .ok_or_else(|| malformed(type_name, "a position must be an array"))?;
    if items.len() < 2 {
        return Err(malformed(
            type_name,
            "a position needs at least a longitude and a latitude",
        ));
    }
    let longitude = items[0]
        .as_number()
        .ok_or_else(|| malformed(type_name, "longitude must be a number"))?;
    let latitude = items[1]
        .as_number()
        .ok_or_else(|| malformed(type_name, "latitude must be a number"))?;
    // A third element is elevation, which the spec allows and nothing here reads.
    Ok([longitude, latitude])
}
