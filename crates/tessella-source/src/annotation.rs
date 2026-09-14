//! Annotations, and the tiles they synthesize.
//!
//! An annotation is not a style layer. There is no `"type": "annotation"` and no stylesheet can
//! produce one: they are added through the map's own API, and the style a frame renders is the
//! caller's style plus a source and some layers that were never written down. mbgl does this in
//! `AnnotationManager`, and this is that manager's data half — the store, and the tile it cuts
//! out of the store on demand.
//!
//! # Why the tile is an MVT tile
//!
//! Because that is what mbgl's is. `AnnotationTile` is a `GeometryTile`: it presents named
//! layers of features exactly as a decoded vector tile does, and the bucket builders downstream
//! cannot tell the difference and must not be able to. Synthesizing an [`mvt::Tile`] here gets
//! the same property for the same reason — the annotation path stops at the tile boundary, and
//! everything past it is the vector-tile path that is already at parity.
//!
//! The alternative was to feed the GeoJSON builder, which takes a flat feature list. It does not
//! fit: an annotation tile carries *named* layers — one for all the points, one per shape — and
//! each style layer reads only its own. A flat list has nowhere to put that.
//!
//! # The numbers are not the GeoJSON source's numbers
//!
//! A GeoJSON source's `buffer` and `tolerance` are written in screen units at the 512-pixel tile
//! size and multiplied by [`crate::tiling::SCALE`] on the way in. Annotations are not: mbgl hands
//! geojson-vt `buffer = 255` and `tolerance = 4` with `extent = EXTENT` directly, already in tile
//! units. A buffer of 255 tile units is about 16 screen pixels, where a GeoJSON source's default
//! 128 screen units is 2048 tile units — so reading these as screen units and scaling them would
//! give a buffer eight times too wide rather than sixteen times too narrow, which is the same
//! class of mistake in the other direction and just as invisible.
//!
//! # What is deliberately not here
//!
//! **Simplification.** mbgl's shapes go through geojson-vt, which runs Douglas-Peucker at the
//! tolerance above. Nothing in this tree simplifies — see [`crate::clip`], which establishes that
//! the clip is a faithful port and that simplification is not implicated in the geometry the
//! oracle produces for rectangles, where every corner is significant. A shape annotation dense
//! enough for a tolerance of 4 tile units to drop a vertex would diverge, and that is the first
//! place to look if one ever does.
//!
//! **`fixupPolygons`.** mbgl applies it to geojson-vt's polygon output to repair
//! geojson-vt#44, where a multi-polygon's rings come back flattened into one list with no record
//! of which exterior owns which hole. The rings here are cut from the caller's own polygon and
//! never lose that structure, so there is nothing to repair.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;

use tessella_style::Value;
use tessella_tile::projection;

use crate::clip::{clip_line_to_box, clip_ring_to_box};
use crate::geojson::{PolygonRings, Position};
use crate::kdbush::KdBush;
use crate::mvt;

/// The id an annotation is addressed by after it is added.
pub type AnnotationId = u64;

/// The source the synthesized layers read, and the prefix every synthesized name carries.
pub const SOURCE_ID: &str = "org.maplibre.annotations";

/// The one symbol layer, which draws every point annotation.
pub const POINT_LAYER_ID: &str = "org.maplibre.annotations.points";

/// Prefix of a shape annotation's own layer, completed with the annotation's id.
pub const SHAPE_LAYER_PREFIX: &str = "org.maplibre.annotations.shape.";

/// The property a point feature carries its icon in, which the point layer's `icon-image`
/// expression reads.
pub const SPRITE_PROPERTY: &str = "sprite";

/// The icon a point annotation that named none asks for.
///
/// It is not supplied here. An annotation with no icon resolves to an image the caller has to
/// have added, and a caller that added none draws nothing — which is mbgl's behavior too.
pub const DEFAULT_MARKER: &str = "default_marker";

/// Past this zoom the pyramid overscales rather than cutting new tiles.
///
/// mbgl's comment: "Zoom level 16 is typically sufficient for annotations", citing
/// mapbox-gl-native#10197. It is the source's maxzoom, not a property of any one annotation.
pub const MAXZOOM: u8 = 16;

/// Margin around a shape annotation's tile, in tile units.
pub const SHAPE_BUFFER: i32 = 255;

/// Douglas-Peucker tolerance mbgl tiles shape annotations at, in tile units.
///
/// Recorded rather than used: nothing here simplifies. See the module note.
pub const SHAPE_TOLERANCE: f64 = 4.0;

/// Tile-local extent, which annotation tiles state explicitly rather than taking the MVT default.
pub const EXTENT: u32 = 8192;

/// How far outside a tile's bounds a point annotation still counts as inside it.
///
/// mbgl widens the query box by this much in degrees, against
/// mapbox-gl-native#12472: a point on a tile boundary is inside both tiles at `f64` precision
/// only by luck, and a symbol that lands in neither tile is a symbol that does not draw.
/// Landing in both is safe — placement already decides which tile owns a symbol.
const BOUNDS_EPSILON: f64 = 0.000_000_001;

/// A point annotation.
#[derive(Debug, Clone, PartialEq)]
pub struct SymbolAnnotation {
    /// Where it sits, in longitude and latitude.
    pub geometry: Position,
    /// The image id, without the source prefix. Empty asks for [`DEFAULT_MARKER`].
    pub icon: String,
}

/// A shape annotation's geometry.
///
/// mbgl's `ShapeAnnotationGeometry` is a four-way variant over LineString, MultiLineString,
/// Polygon and MultiPolygon. Singular and multi fold together the same way [`crate::Geometry`]
/// folds them, which leaves two cases — and both classes accept both, because mbgl's variant
/// does not restrict a line annotation to lines.
#[derive(Debug, Clone, PartialEq)]
pub enum ShapeGeometry {
    /// One or more lines.
    Lines(Vec<Vec<Position>>),
    /// One or more polygons, each an exterior ring followed by its holes.
    Polygons(Vec<PolygonRings>),
}

impl ShapeGeometry {
    /// Closes every ring of every polygon, which is what a shape annotation's constructor does.
    ///
    /// Lines are left alone: mbgl's `CloseShapeAnnotation` returns both line cases unchanged.
    #[must_use]
    pub fn closed(mut self) -> Self {
        if let Self::Polygons(polygons) = &mut self {
            for polygon in polygons.iter_mut() {
                for ring in polygon.iter_mut() {
                    match (ring.first().copied(), ring.last().copied()) {
                        (Some(first), Some(last)) if first != last => ring.push(first),
                        _ => {}
                    }
                }
            }
        }
        self
    }

    /// The feature type a synthesized tile gives this geometry.
    const fn geom_type(&self) -> mvt::GeomType {
        match self {
            Self::Lines(_) => mvt::GeomType::LineString,
            Self::Polygons(_) => mvt::GeomType::Polygon,
        }
    }
}

/// Which class a shape annotation belongs to, and so which layer kind it synthesizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeKind {
    /// A line annotation: `line-opacity`, `line-width`, `line-color`.
    Line,
    /// A fill annotation: `fill-opacity`, `fill-color`, `fill-outline-color`.
    Fill,
}

/// A shape annotation's paint, as style values rather than as numbers.
///
/// mbgl's annotation classes hold `PropertyValue<float>` and `PropertyValue<Color>`, which is to
/// say a constant *or* an expression. Holding the style's own [`Value`] keeps that: whatever the
/// caller passed is what the synthesized layer's paint property is set to, and the property
/// parser downstream is the one that already decides what a paint value may be. `None` leaves the
/// property unset, which is how the annotation class's own default applies.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShapePaint {
    /// `line-opacity` or `fill-opacity`. mbgl defaults both to 1.
    pub opacity: Option<Value>,
    /// `line-width`, on a line annotation. mbgl defaults it to 1.
    pub width: Option<Value>,
    /// `line-color` or `fill-color`. mbgl defaults both to black.
    pub color: Option<Value>,
    /// `fill-outline-color`, on a fill annotation. mbgl leaves it unset.
    pub outline_color: Option<Value>,
}

/// A line or fill annotation.
#[derive(Debug, Clone, PartialEq)]
pub struct ShapeAnnotation {
    /// Which class it is.
    pub kind: ShapeKind,
    /// Its geometry, in longitude and latitude.
    pub geometry: ShapeGeometry,
    /// Its paint.
    pub paint: ShapePaint,
}

impl ShapeAnnotation {
    /// A line annotation, with its rings closed the way the constructor closes them.
    #[must_use]
    pub fn line(geometry: ShapeGeometry, paint: ShapePaint) -> Self {
        Self {
            kind: ShapeKind::Line,
            geometry: geometry.closed(),
            paint,
        }
    }

    /// A fill annotation, with its rings closed the way the constructor closes them.
    #[must_use]
    pub fn fill(geometry: ShapeGeometry, paint: ShapePaint) -> Self {
        Self {
            kind: ShapeKind::Fill,
            geometry: geometry.closed(),
            paint,
        }
    }

    /// The layer this annotation synthesizes, which is also its source-layer.
    #[must_use]
    pub fn layer_id(id: AnnotationId) -> String {
        let mut name = String::from(SHAPE_LAYER_PREFIX);
        name.push_str(&id.to_string());
        name
    }
}

/// Either kind of annotation, as an `add` takes it.
#[derive(Debug, Clone, PartialEq)]
pub enum Annotation {
    /// A point.
    Symbol(SymbolAnnotation),
    /// A line or a fill.
    Shape(ShapeAnnotation),
}

/// The point index, rebuilt when the points it indexes change.
///
/// # Why an index at all, and why this one
///
/// A tile asks which points lie inside it, and a frame's cover asks that a dozen or more times.
/// Scanned linearly that is `covers * annotations` bounds tests per frame, which is fine at ten
/// annotations and is not at ten thousand — and ten thousand is the case the index exists for.
///
/// mbgl keeps a boost R-tree, which inserts and removes in place. This is a static k-d tree that
/// is thrown away when the set changes and rebuilt on the next query. For the access pattern that
/// is the better trade and not a compromise: annotations are mutated in bursts and queried every
/// frame, so a rebuild is paid once per burst rather than once per annotation, and the queries in
/// between are answered by a tree with a better constant than an R-tree's.
struct PointIndex {
    tree: KdBush,
    /// The annotation ids, in the order the tree indexes them.
    ids: Vec<AnnotationId>,
}

/// Every annotation a map is carrying, and the tiles it cuts.
///
/// Cloning is not offered: the index behind it is a cache, and a copy of a cache is a second
/// thing to invalidate.
#[derive(Debug, Default)]
pub struct Annotations {
    symbols: BTreeMap<AnnotationId, SymbolAnnotation>,
    shapes: BTreeMap<AnnotationId, ShapeAnnotation>,
    next_id: AnnotationId,
    /// `None` when the points have changed since the last query.
    index: RefCell<Option<PointIndex>>,
}

impl core::fmt::Debug for PointIndex {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PointIndex")
            .field("points", &self.ids.len())
            .finish()
    }
}

impl Annotations {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether anything has been added.
    ///
    /// mbgl returns no tile data at all while this holds, which is what keeps a map with no
    /// annotations from paying for an annotation source.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.symbols.is_empty() && self.shapes.is_empty()
    }

    /// Adds one, and returns the id it is addressed by.
    pub fn add(&mut self, annotation: Annotation) -> AnnotationId {
        let id = self.next_id;
        self.next_id += 1;
        match annotation {
            Annotation::Symbol(symbol) => {
                self.symbols.insert(id, symbol);
                self.index.replace(None);
            }
            Annotation::Shape(shape) => {
                self.shapes.insert(id, shape);
            }
        }
        id
    }

    /// Replaces one in place, keeping its id.
    ///
    /// Returns whether an annotation of that id existed. An update that changes a point
    /// annotation into a shape one is an update: mbgl's own `update` removes and re-adds.
    pub fn update(&mut self, id: AnnotationId, annotation: Annotation) -> bool {
        if !self.symbols.contains_key(&id) && !self.shapes.contains_key(&id) {
            return false;
        }
        self.remove(id);
        match annotation {
            Annotation::Symbol(symbol) => {
                self.symbols.insert(id, symbol);
                self.index.replace(None);
            }
            Annotation::Shape(shape) => {
                self.shapes.insert(id, shape);
            }
        }
        true
    }

    /// Removes one. Returns whether it was there.
    pub fn remove(&mut self, id: AnnotationId) -> bool {
        if self.symbols.remove(&id).is_some() {
            self.index.replace(None);
            return true;
        }
        self.shapes.remove(&id).is_some()
    }

    /// Every shape annotation, in id order, which is the order their layers are synthesized in.
    pub fn shapes(&self) -> impl Iterator<Item = (AnnotationId, &ShapeAnnotation)> {
        self.shapes.iter().map(|(id, shape)| (*id, shape))
    }

    /// The tile at `z/x/y`, or `None` when there is nothing to put in one.
    ///
    /// The point layer is added whenever anything exists at all, empty or not — mbgl adds it
    /// unconditionally, and an empty named layer is not the same as an absent one to a style
    /// layer that reads it. A shape's layer is added only when the shape has geometry in this
    /// tile.
    #[must_use]
    pub fn tile(&self, z: u8, x: u32, y: u32) -> Option<mvt::Tile> {
        if self.is_empty() {
            return None;
        }

        let mut tile = mvt::Tile::default();
        tile.layers.push(self.point_layer(z, x, y));
        for (id, shape) in &self.shapes {
            if let Some(layer) = shape_layer(*id, shape, z, x, y) {
                tile.layers.push(layer);
            }
        }
        Some(tile)
    }

    /// The point layer: every symbol annotation whose position is inside this tile.
    fn point_layer(&self, z: u8, x: u32, y: u32) -> mvt::Layer {
        let mut layer = mvt::Layer::new(POINT_LAYER_ID.into(), EXTENT, 2);
        let sprite: Arc<str> = SPRITE_PROPERTY.into();

        self.with_index(|index| {
            let (west, south, east, north) = tile_bounds(z, x, y);
            index
                .tree
                .range(west, south, east, north, &mut |slot: u32| {
                    let id = index.ids[slot as usize];
                    let Some(symbol) = self.symbols.get(&id) else {
                        return;
                    };
                    let icon: Arc<str> = if symbol.icon.is_empty() {
                        DEFAULT_MARKER.into()
                    } else {
                        symbol.icon.as_str().into()
                    };
                    let point = to_tile_units(symbol.geometry, z, x, y);
                    layer.push_feature(
                        Some(id),
                        mvt::GeomType::Point,
                        [(Arc::clone(&sprite), mvt::Value::String(icon))],
                        &mvt::Geometry::from_rings([alloc::vec![point]]),
                    );
                });
        });

        layer
    }

    /// Runs `visit` against a current index, rebuilding it first if the points have moved.
    fn with_index(&self, visit: impl FnOnce(&PointIndex)) {
        let mut slot = self.index.borrow_mut();
        let index = slot.get_or_insert_with(|| {
            let ids: Vec<AnnotationId> = self.symbols.keys().copied().collect();
            let points: Vec<(f64, f64)> = self
                .symbols
                .values()
                .map(|symbol| (symbol.geometry[0], symbol.geometry[1]))
                .collect();
            PointIndex {
                tree: KdBush::new(&points),
                ids,
            }
        });
        visit(index);
    }
}

/// One shape's layer in one tile, or `None` when none of it lands here.
fn shape_layer(
    id: AnnotationId,
    shape: &ShapeAnnotation,
    z: u8,
    x: u32,
    y: u32,
) -> Option<mvt::Layer> {
    let lo = f64::from(-SHAPE_BUFFER);
    let hi = f64::from(i32::try_from(EXTENT).unwrap_or(i32::MAX) + SHAPE_BUFFER);
    let project = |position: &Position| projection::tile_local(position[0], position[1], z, x, y);

    let rings: Vec<Vec<[i32; 2]>> = match &shape.geometry {
        ShapeGeometry::Lines(lines) => lines
            .iter()
            .flat_map(|line| {
                let projected: Vec<Position> = line.iter().map(project).collect();
                clip_line_to_box(&projected, lo, hi)
            })
            .map(|piece| crate::clip::round_to_tile_units(&piece))
            .filter(|piece| !piece.is_empty())
            .collect(),
        ShapeGeometry::Polygons(polygons) => polygons
            .iter()
            .flatten()
            .filter_map(|ring| {
                let projected: Vec<Position> = ring.iter().map(project).collect();
                let clipped = clip_ring_to_box(&projected, lo, hi);
                if clipped.is_empty() {
                    None
                } else {
                    Some(crate::clip::round_to_tile_units(&clipped))
                }
            })
            .collect(),
    };

    if rings.is_empty() {
        return None;
    }

    let mut layer = mvt::Layer::new(ShapeAnnotation::layer_id(id), EXTENT, 2);
    layer.push_feature(
        Some(id),
        shape.geometry.geom_type(),
        [],
        &mvt::Geometry::from_rings(rings),
    );
    Some(layer)
}

/// A position in this tile's units, truncated and clamped the way mbgl's does it.
///
/// `toGeometryCoordinate` casts to `int64_t` — which truncates toward zero rather than rounding —
/// and clamps into `int16_t`. The clamp is reachable only for a point far outside the tile, which
/// the bounds query does not return; it is kept because the truncation is not, and a reader who
/// sees one without the other will assume the wrong one was the choice.
#[allow(clippy::cast_possible_truncation)]
fn to_tile_units(position: Position, z: u8, x: u32, y: u32) -> [i32; 2] {
    let local = projection::tile_local(position[0], position[1], z, x, y);
    [
        (local[0] as i64).clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i32,
        (local[1] as i64).clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i32,
    ]
}

/// A tile's bounds in degrees, widened by [`BOUNDS_EPSILON`] on every side.
///
/// Returned west, south, east, north — the order [`KdBush::range`] takes its box in.
fn tile_bounds(z: u8, x: u32, y: u32) -> (f64, f64, f64, f64) {
    let scale = f64::from(1u32 << z);
    let (west, north) = projection::unproject([f64::from(x), f64::from(y)], scale);
    let (east, south) = projection::unproject([f64::from(x) + 1.0, f64::from(y) + 1.0], scale);
    (
        west - BOUNDS_EPSILON,
        south - BOUNDS_EPSILON,
        east + BOUNDS_EPSILON,
        north + BOUNDS_EPSILON,
    )
}

/// Builds a style value that is a call: `["op", ...args]`.
fn call(op: &str, args: Vec<Value>) -> Value {
    let mut items = Vec::with_capacity(args.len() + 1);
    items.push(Value::String(op.into()));
    items.extend(args);
    Value::Array(items)
}

/// The `icon-image` expression the point layer resolves its sprite through.
///
/// `["image", ["concat", "org.maplibre.annotations.", ["to-string", ["get", "sprite"]]]]`, which
/// is mbgl's `image(concat(vec(literal(SourceID + "."), toString(get("sprite")))))` written out.
/// The prefix is here rather than in the feature's property because the images are prefixed on
/// the way in, so that an annotation image can never collide with one from the style's sheet.
fn icon_image_expression() -> Value {
    let mut prefix = String::from(SOURCE_ID);
    prefix.push('.');
    call(
        "image",
        alloc::vec![call(
            "concat",
            alloc::vec![
                Value::String(prefix),
                call(
                    "to-string",
                    alloc::vec![call(
                        "get",
                        alloc::vec![Value::String(SPRITE_PROPERTY.into())]
                    )]
                ),
            ],
        )],
    )
}

/// Sets `key` from `value`, or leaves it unset so the spec's own default applies.
///
/// Unset and set-to-the-default are the same pixel here by construction: mbgl's annotation
/// classes default opacity to 1, width to 1 and color to black, and the spec defaults
/// `line-opacity`, `line-width`, `fill-opacity` and both colors to exactly those. Leaving the
/// property out is what keeps that agreement checkable rather than restated.
fn set_paint(
    paint: &mut alloc::collections::BTreeMap<String, tessella_style::PropertyValue>,
    key: &str,
    value: &Option<Value>,
) {
    if let Some(value) = value {
        paint.insert(
            key.into(),
            tessella_style::PropertyValue::from_value(value.clone()),
        );
    }
}

impl Annotations {
    /// Puts the source and the layers into `style`, replacing any this put there before.
    ///
    /// This is mbgl's `AnnotationManager::updateStyle`, which runs on every style load and after
    /// every mutation. Idempotent for the same reason: the caller's style is reloaded and
    /// replaced underneath the annotations, and what was synthesized into the old one has to
    /// arrive in the new one without the annotations being re-added.
    ///
    /// # Layer order
    ///
    /// The point layer is appended once and every shape layer is inserted *before* it, so the
    /// style ends `[the caller's layers, shapes in id order, points]`. Shapes under points is
    /// mbgl's order and not an accident: a marker is a label and belongs on top of the geometry
    /// it marks.
    ///
    /// # Nothing is synthesized for an empty store
    ///
    /// mbgl adds the source and the point layer whether or not any annotation exists, because
    /// `updateStyle` runs from `onStyleLoaded` unconditionally. This does not, and the frame
    /// cannot tell: `getTileData` returns nothing for an empty store, so mbgl's version of those
    /// tiles carries no features and its point layer draws nothing. What the divergence avoids is
    /// covering a source that has nothing in it, which is a tile pyramid's worth of work for a
    /// layer that is guaranteed to be empty.
    pub fn synthesize(&self, style: &mut tessella_style::Style) {
        style
            .layers
            .retain(|layer| !is_synthesized_layer(&layer.id));
        if self.is_empty() {
            style.sources.remove(SOURCE_ID);
            return;
        }

        style
            .sources
            .insert(SOURCE_ID.into(), tessella_style::Source::Annotation);

        for (id, shape) in &self.shapes {
            style.layers.push(shape_style_layer(*id, shape));
        }
        style.layers.push(point_style_layer());
    }
}

/// Whether `id` names a layer [`Annotations::synthesize`] put there.
#[must_use]
pub fn is_synthesized_layer(id: &str) -> bool {
    id == POINT_LAYER_ID || id.starts_with(SHAPE_LAYER_PREFIX)
}

/// The one symbol layer every point annotation draws through.
fn point_style_layer() -> tessella_style::Layer {
    let mut layout = alloc::collections::BTreeMap::new();
    layout.insert(
        "icon-image".into(),
        tessella_style::PropertyValue::from_value(icon_image_expression()),
    );
    // A marker is placed where the caller put it. Collision would move or drop it, and an
    // annotation that is not where it was added is worse than one that overlaps another.
    layout.insert(
        "icon-allow-overlap".into(),
        tessella_style::PropertyValue::Literal(Value::Bool(true)),
    );
    layout.insert(
        "icon-ignore-placement".into(),
        tessella_style::PropertyValue::Literal(Value::Bool(true)),
    );

    tessella_style::Layer {
        id: POINT_LAYER_ID.into(),
        kind: tessella_style::LayerKind::Symbol,
        source: Some(SOURCE_ID.into()),
        source_layer: Some(POINT_LAYER_ID.into()),
        minzoom: None,
        maxzoom: None,
        filter: None,
        paint: alloc::collections::BTreeMap::new(),
        layout,
        extra: alloc::collections::BTreeMap::new(),
    }
}

/// One shape annotation's layer, which is a line or a fill and reads only its own source-layer.
fn shape_style_layer(id: AnnotationId, shape: &ShapeAnnotation) -> tessella_style::Layer {
    let name = ShapeAnnotation::layer_id(id);
    let mut paint = alloc::collections::BTreeMap::new();
    let mut layout = alloc::collections::BTreeMap::new();

    let kind = match shape.kind {
        ShapeKind::Line => {
            set_paint(&mut paint, "line-opacity", &shape.paint.opacity);
            set_paint(&mut paint, "line-width", &shape.paint.width);
            set_paint(&mut paint, "line-color", &shape.paint.color);
            // mbgl sets it on the layer rather than leaving the spec's `miter`: an annotation is
            // a shape the caller drew, not a road, and a mitered corner on a hand-drawn polyline
            // spikes.
            layout.insert(
                "line-join".into(),
                tessella_style::PropertyValue::Literal(Value::String("round".into())),
            );
            tessella_style::LayerKind::Line
        }
        ShapeKind::Fill => {
            set_paint(&mut paint, "fill-opacity", &shape.paint.opacity);
            set_paint(&mut paint, "fill-color", &shape.paint.color);
            set_paint(&mut paint, "fill-outline-color", &shape.paint.outline_color);
            tessella_style::LayerKind::Fill
        }
    };

    tessella_style::Layer {
        id: name.clone(),
        kind,
        source: Some(SOURCE_ID.into()),
        source_layer: Some(name),
        minzoom: None,
        maxzoom: None,
        filter: None,
        paint,
        layout,
        extra: alloc::collections::BTreeMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn symbol(lon: f64, lat: f64, icon: &str) -> Annotation {
        Annotation::Symbol(SymbolAnnotation {
            geometry: [lon, lat],
            icon: icon.into(),
        })
    }

    #[test]
    fn an_empty_store_cuts_no_tile() {
        assert!(Annotations::new().tile(0, 0, 0).is_none());
    }

    #[test]
    fn ids_count_up_across_both_kinds() {
        let mut annotations = Annotations::new();
        assert_eq!(annotations.add(symbol(0.0, 0.0, "")), 0);
        assert_eq!(
            annotations.add(Annotation::Shape(ShapeAnnotation::line(
                ShapeGeometry::Lines(alloc::vec![alloc::vec![[0.0, 0.0], [1.0, 1.0]]]),
                ShapePaint::default(),
            ))),
            1
        );
        assert_eq!(annotations.add(symbol(1.0, 1.0, "")), 2);
    }

    /// The layer is there whether or not any point landed in the tile, because an absent named
    /// layer and an empty one are not the same thing to the layer that reads it.
    #[test]
    fn the_point_layer_exists_even_when_no_point_is_in_the_tile() {
        let mut annotations = Annotations::new();
        annotations.add(symbol(100.0, 40.0, "marker"));
        let tile = annotations.tile(4, 0, 0).expect("a tile");
        assert_eq!(tile.layers.len(), 1);
        assert_eq!(tile.layers[0].name, POINT_LAYER_ID);
        assert_eq!(tile.layers[0].len(), 0);
    }

    #[test]
    fn a_point_lands_in_the_tile_that_contains_it() {
        let mut annotations = Annotations::new();
        let id = annotations.add(symbol(13.405, 52.52, "marker"));

        // z14 tile containing Berlin.
        let units = projection::tile_units(13.405, 52.52, 14);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (x, y) = (units[0] as u32, units[1] as u32);

        let tile = annotations.tile(14, x, y).expect("a tile");
        let layer = &tile.layers[0];
        assert_eq!(layer.len(), 1);
        let feature = layer.feature(0).expect("the point");
        assert_eq!(feature.id(), Some(id));
        assert_eq!(feature.geom_type(), mvt::GeomType::Point);
        let ring = feature.rings().next().expect("one ring");
        let expected = to_tile_units([13.405, 52.52], 14, x, y);
        assert_eq!(ring, [expected]);
    }

    #[test]
    fn a_point_with_no_icon_asks_for_the_default_marker() {
        let mut annotations = Annotations::new();
        annotations.add(symbol(0.0, 0.0, ""));
        let tile = annotations.tile(0, 0, 0).expect("a tile");
        let feature = tile.layers[0].feature(0).expect("the point");
        assert_eq!(
            feature.properties(),
            [(
                Arc::from(SPRITE_PROPERTY),
                mvt::Value::String(Arc::from(DEFAULT_MARKER))
            )]
        );
    }

    /// Truncation toward zero, not rounding. A coordinate at `x.9` is `x`.
    #[test]
    fn a_points_tile_coordinate_truncates() {
        // Longitude chosen so the z0 tile-unit x lands just under a whole number of extent units.
        let lon = -180.0 + 360.0 * (1234.9999 / f64::from(EXTENT));
        let local = projection::tile_local(lon, 0.0, 0, 0, 0);
        assert!(local[0] > 1234.0 && local[0] < 1235.0);
        assert_eq!(to_tile_units([lon, 0.0], 0, 0, 0)[0], 1234);
    }

    #[test]
    fn a_shape_gets_its_own_layer_named_after_its_id() {
        let mut annotations = Annotations::new();
        annotations.add(symbol(0.0, 0.0, ""));
        let id = annotations.add(Annotation::Shape(ShapeAnnotation::fill(
            ShapeGeometry::Polygons(alloc::vec![alloc::vec![alloc::vec![
                [-1.0, -1.0],
                [1.0, -1.0],
                [1.0, 1.0],
                [-1.0, 1.0],
            ]]]),
            ShapePaint::default(),
        )));

        let tile = annotations.tile(0, 0, 0).expect("a tile");
        assert_eq!(tile.layers.len(), 2);
        assert_eq!(tile.layers[1].name, ShapeAnnotation::layer_id(id));
        assert_eq!(tile.layers[1].name, "org.maplibre.annotations.shape.1");
        let feature = tile.layers[1].feature(0).expect("the ring");
        assert_eq!(feature.geom_type(), mvt::GeomType::Polygon);
        assert_eq!(feature.id(), Some(id));
        assert!(feature.properties().is_empty());
    }

    /// A fill annotation's rings are closed on construction; a line annotation's are not.
    #[test]
    fn construction_closes_polygon_rings_and_leaves_lines_alone() {
        let open = alloc::vec![alloc::vec![alloc::vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0]]]];
        let fill =
            ShapeAnnotation::fill(ShapeGeometry::Polygons(open.clone()), ShapePaint::default());
        match fill.geometry {
            ShapeGeometry::Polygons(polygons) => {
                assert_eq!(polygons[0][0].len(), 4);
                assert_eq!(polygons[0][0][3], [0.0, 0.0]);
            }
            ShapeGeometry::Lines(_) => panic!("a fill kept its polygons"),
        }

        let line = ShapeAnnotation::line(
            ShapeGeometry::Lines(alloc::vec![alloc::vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0]]]),
            ShapePaint::default(),
        );
        match line.geometry {
            ShapeGeometry::Lines(lines) => assert_eq!(lines[0].len(), 3),
            ShapeGeometry::Polygons(_) => panic!("a line kept its lines"),
        }
    }

    /// A shape that misses the tile entirely contributes no layer, rather than an empty one.
    #[test]
    fn a_shape_outside_the_tile_is_absent_not_empty() {
        let mut annotations = Annotations::new();
        annotations.add(Annotation::Shape(ShapeAnnotation::fill(
            ShapeGeometry::Polygons(alloc::vec![alloc::vec![alloc::vec![
                [170.0, -40.0],
                [171.0, -40.0],
                [171.0, -39.0],
            ]]]),
            ShapePaint::default(),
        )));
        let tile = annotations.tile(4, 0, 0).expect("a tile");
        assert_eq!(tile.layers.len(), 1);
    }

    /// The buffer is 255 tile units, not 255 screen units scaled to 4080.
    #[test]
    fn a_shape_is_clipped_to_the_tile_plus_255() {
        let mut annotations = Annotations::new();
        // A band spanning the whole world at the equator: at z1 it crosses tile 0 entirely.
        annotations.add(Annotation::Shape(ShapeAnnotation::fill(
            ShapeGeometry::Polygons(alloc::vec![alloc::vec![alloc::vec![
                [-179.0, -10.0],
                [179.0, -10.0],
                [179.0, 10.0],
                [-179.0, 10.0],
            ]]]),
            ShapePaint::default(),
        )));
        let tile = annotations.tile(1, 0, 0).expect("a tile");
        let feature = tile.layers[1].feature(0).expect("the ring");
        let limit = i32::try_from(EXTENT).expect("extent fits") + SHAPE_BUFFER;
        for ring in feature.rings() {
            for point in ring {
                assert!(point[0] >= -SHAPE_BUFFER && point[0] <= limit, "{point:?}");
                assert!(point[1] >= -SHAPE_BUFFER && point[1] <= limit, "{point:?}");
            }
        }
        // And it really does reach the buffer rather than stopping at the edge.
        assert!(
            feature
                .rings()
                .flatten()
                .any(|point| point[0] == limit || point[0] == -SHAPE_BUFFER)
        );
    }

    #[test]
    fn removing_a_point_drops_it_from_the_next_tile() {
        let mut annotations = Annotations::new();
        let id = annotations.add(symbol(0.0, 0.0, "marker"));
        assert_eq!(
            annotations.tile(0, 0, 0).expect("a tile").layers[0].len(),
            1
        );
        assert!(annotations.remove(id));
        assert!(annotations.tile(0, 0, 0).is_none());
    }

    #[test]
    fn updating_a_point_moves_it_and_keeps_its_id() {
        let mut annotations = Annotations::new();
        let id = annotations.add(symbol(0.0, 0.0, "marker"));
        assert!(annotations.update(id, symbol(100.0, 40.0, "other")));
        let tile = annotations.tile(0, 0, 0).expect("a tile");
        let feature = tile.layers[0].feature(0).expect("the point");
        assert_eq!(feature.id(), Some(id));
        assert_eq!(
            feature.properties(),
            [(
                Arc::from(SPRITE_PROPERTY),
                mvt::Value::String(Arc::from("other"))
            )]
        );
        assert!(!annotations.update(99, symbol(0.0, 0.0, "")));
    }

    fn empty_style() -> tessella_style::Style {
        tessella_style::Style::parse(r#"{"version":8,"sources":{},"layers":[]}"#).expect("a style")
    }

    fn shape(kind: ShapeKind, paint: ShapePaint) -> Annotation {
        let geometry = ShapeGeometry::Lines(alloc::vec![alloc::vec![[0.0, 0.0], [1.0, 1.0]]]);
        Annotation::Shape(match kind {
            ShapeKind::Line => ShapeAnnotation::line(geometry, paint),
            ShapeKind::Fill => ShapeAnnotation::fill(geometry, paint),
        })
    }

    #[test]
    fn an_empty_store_synthesizes_nothing() {
        let mut style = empty_style();
        Annotations::new().synthesize(&mut style);
        assert!(style.layers.is_empty());
        assert!(style.source(SOURCE_ID).is_none());
    }

    #[test]
    fn shapes_sit_under_the_point_layer_in_id_order() {
        let mut annotations = Annotations::new();
        annotations.add(symbol(0.0, 0.0, "marker"));
        annotations.add(shape(ShapeKind::Line, ShapePaint::default()));
        annotations.add(shape(ShapeKind::Fill, ShapePaint::default()));

        let mut style = empty_style();
        annotations.synthesize(&mut style);

        let ids: Vec<&str> = style.layers.iter().map(|layer| layer.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "org.maplibre.annotations.shape.1",
                "org.maplibre.annotations.shape.2",
                POINT_LAYER_ID,
            ]
        );
        assert_eq!(style.layers[0].kind, tessella_style::LayerKind::Line);
        assert_eq!(style.layers[1].kind, tessella_style::LayerKind::Fill);
        assert_eq!(style.layers[2].kind, tessella_style::LayerKind::Symbol);
        assert_eq!(
            style.source(SOURCE_ID),
            Some(&tessella_style::Source::Annotation)
        );
    }

    /// The caller's own layers are untouched, and the synthesized ones go after them.
    #[test]
    fn synthesis_appends_and_is_idempotent() {
        let mut annotations = Annotations::new();
        annotations.add(symbol(0.0, 0.0, "marker"));

        let mut style = tessella_style::Style::parse(
            r#"{"version":8,"sources":{},"layers":[{"id":"bg","type":"background"}]}"#,
        )
        .expect("a style");

        annotations.synthesize(&mut style);
        annotations.synthesize(&mut style);
        annotations.synthesize(&mut style);

        let ids: Vec<&str> = style.layers.iter().map(|layer| layer.id.as_str()).collect();
        assert_eq!(ids, ["bg", POINT_LAYER_ID]);
    }

    /// Removing the last annotation takes the source and the layers back out with it.
    #[test]
    fn synthesis_undoes_itself_when_the_last_annotation_goes() {
        let mut annotations = Annotations::new();
        let id = annotations.add(symbol(0.0, 0.0, "marker"));
        let mut style = empty_style();
        annotations.synthesize(&mut style);
        assert_eq!(style.layers.len(), 1);

        annotations.remove(id);
        annotations.synthesize(&mut style);
        assert!(style.layers.is_empty());
        assert!(style.source(SOURCE_ID).is_none());
    }

    /// The prefix is the manager's, so an annotation image can never collide with a style one.
    #[test]
    fn the_point_layer_resolves_its_icon_through_the_prefix() {
        let mut annotations = Annotations::new();
        annotations.add(symbol(0.0, 0.0, "marker"));
        let mut style = empty_style();
        annotations.synthesize(&mut style);

        let layer = &style.layers[0];
        let icon = layer.layout.get("icon-image").expect("an icon-image");
        let expression = icon.as_expression().expect("an expression").value();
        assert_eq!(
            expression,
            &call(
                "image",
                alloc::vec![call(
                    "concat",
                    alloc::vec![
                        Value::String("org.maplibre.annotations.".into()),
                        call(
                            "to-string",
                            alloc::vec![call("get", alloc::vec![Value::String("sprite".into())])]
                        ),
                    ],
                )],
            )
        );
        assert_eq!(
            layer.layout.get("icon-allow-overlap"),
            Some(&tessella_style::PropertyValue::Literal(Value::Bool(true)))
        );
        assert_eq!(
            layer.layout.get("icon-ignore-placement"),
            Some(&tessella_style::PropertyValue::Literal(Value::Bool(true)))
        );
    }

    /// Absent paint stays absent, so the spec's default applies -- which is the annotation
    /// class's default, and the two agreeing is the point.
    #[test]
    fn unset_paint_is_left_out_and_set_paint_is_written_through() {
        let mut annotations = Annotations::new();
        annotations.add(shape(ShapeKind::Line, ShapePaint::default()));
        annotations.add(shape(
            ShapeKind::Fill,
            ShapePaint {
                opacity: Some(Value::Number(0.5)),
                color: Some(Value::String("#ff0000".into())),
                outline_color: Some(call(
                    "interpolate",
                    alloc::vec![
                        call("linear", Vec::new()),
                        call("zoom", Vec::new()),
                        Value::Number(0.0),
                        Value::String("red".into()),
                        Value::Number(10.0),
                        Value::String("blue".into()),
                    ],
                )),
                ..ShapePaint::default()
            },
        ));

        let mut style = empty_style();
        annotations.synthesize(&mut style);

        assert!(style.layers[0].paint.is_empty());
        assert_eq!(
            style.layers[0].layout.get("line-join"),
            Some(&tessella_style::PropertyValue::Literal(Value::String(
                "round".into()
            )))
        );

        let fill = &style.layers[1].paint;
        assert_eq!(
            fill.get("fill-opacity"),
            Some(&tessella_style::PropertyValue::Literal(Value::Number(0.5)))
        );
        assert_eq!(
            fill.get("fill-color"),
            Some(&tessella_style::PropertyValue::Literal(Value::String(
                "#ff0000".into()
            )))
        );
        // An expression survives as an expression, which is what holding style values rather
        // than floats buys.
        assert!(
            fill.get("fill-outline-color")
                .expect("an outline")
                .as_expression()
                .is_some()
        );
        assert!(!fill.contains_key("fill-width"));
    }

    /// Every synthesized layer compiles. A layer the style drops is a layer that never draws,
    /// and nothing else here would say so.
    #[test]
    fn the_synthesized_layers_compile() {
        let mut annotations = Annotations::new();
        annotations.add(symbol(0.0, 0.0, "marker"));
        annotations.add(shape(
            ShapeKind::Line,
            ShapePaint {
                width: Some(Value::Number(4.0)),
                color: Some(Value::String("#ff9c00".into())),
                ..ShapePaint::default()
            },
        ));
        annotations.add(shape(ShapeKind::Fill, ShapePaint::default()));

        let mut style = empty_style();
        annotations.synthesize(&mut style);
        let before = style.layers.len();
        let rejected = style.reject_uncompilable();
        assert_eq!(rejected, Vec::new());
        assert_eq!(style.layers.len(), before);
    }

    /// The widening is what keeps a point exactly on a boundary in a tile at all.
    #[test]
    fn a_point_on_a_tile_boundary_is_in_both_tiles() {
        let mut annotations = Annotations::new();
        // The z1 meridian boundary, which is longitude zero.
        annotations.add(symbol(0.0, 10.0, "marker"));
        assert_eq!(annotations.tile(1, 0, 0).expect("west").layers[0].len(), 1);
        assert_eq!(annotations.tile(1, 1, 0).expect("east").layers[0].len(), 1);
    }
}
