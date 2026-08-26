//! Building one tile's buckets from a style and a set of features.
//!
//! This is where the pieces meet: the style's layers, their filters, the projection, the clip,
//! and the fill tessellator. Each has been checked against the oracle in isolation; this runs
//! them in sequence and checks the result against what the oracle draws for the same tile.
//!
//! # A fill layer is two drawables, not one
//!
//! The oracle emits a fill layer as a *pair* per tile: the triangles at sublayer 1 and the
//! outline at sublayer 2. That is not an optimization detail — `fill-outline-color` is a
//! separate paint property with its own default, and the outline is drawn as lines over the
//! same vertices. A builder producing one drawable per fill layer would be half a layer short
//! and would look correct until something set an outline color.
//!
//! Outlines are counted here but not yet built: they share the fill's vertices and need only
//! their own index list, which is line-list rather than triangle-list — note that this is *not*
//! the extruded line the line layer builds, so the line generator does not supply it.
//!
//! # A line layer is one drawable, and byte-exact
//!
//! A line has no outline sublayer, so it is one drawable per tile at sublayer 0 where a fill is
//! two at sublayers 1 and 2. Its buffers match the oracle byte for byte, which the fill's do
//! not: mbgl runs every GeoJSON *polygon* through wagyu before bucketing and wagyu rotates the
//! rings, while a LineString reaches the bucket in source order. So the line path is the one
//! place the whole chain — projection, clip, rounding, join selection, extrusion, bit-packing —
//! is checked against the oracle's own buffer hashes rather than up to a permutation.
//!
//! # Layer index is the style's order
//!
//! The layer index the stream carries is the layer's position in the style document, not a
//! count of layers that produced geometry. A layer that draws nothing still occupies its index,
//! because the index is what painter order is expressed in and what a consumer keys uniforms
//! by. Skipping unimplemented layers while renumbering the rest would silently restack the map.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tessella_layout::circle::CircleBucket;
use tessella_layout::fill::{self, FillBucket, Position, Ring};
use tessella_layout::line::{LineBucket, LineCap, LineJoin, LineOptions};
use tessella_layout::paint::{BinderError, PaintBinder};
use tessella_layout::raster::RasterBucket;
use tessella_layout::symbol_layout::SymbolLayout;
use tessella_source::clip::{
    clip_line_to_box, clip_points_to_box, clip_ring_to_box, round_to_tile_units,
};
use tessella_source::geojson::{GeoJsonFeature, Geometry};
use tessella_source::tiling::{EXTENT, TilingOptions};
use tessella_style::property::{ResolvedProperty, paint_specs, resolve_paint};
use tessella_style::{Filter, LayerKind, Style};
use tessella_tile::projection;
use tessella_tile::store::{Lookup, TileKey, TileStore};

/// The tile being built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TileId {
    /// Zoom.
    pub z: u8,
    /// Column.
    pub x: u32,
    /// Row.
    pub y: u32,
    /// The zoom this tile is being *used* at, which is the zoom its buckets are built for.
    ///
    /// Equal to `z` for a tile drawn at its own zoom, and greater when one stands in above its
    /// source's maxzoom or covers for a tile still loading. mbgl calls this `overscaledZ` and
    /// passes it, not `z`, as the bucket zoom — so it is what a zoom-varying paint property's
    /// endpoints are evaluated at, and therefore part of a bucket's identity rather than a
    /// display detail.
    pub overscaled_z: u8,
}

/// What a layer contributed to a tile.
#[derive(Debug, Clone, PartialEq)]
pub enum Content {
    /// A full-viewport background. Draws from no source, so it has no features and its
    /// geometry is a quad the consumer can synthesize.
    Background,
    /// Triangles, and a count of the outline drawable that accompanies them.
    Fill(FillBucket),
    /// An extruded polyline.
    Line(LineBucket),
    /// A quad per point, with the disc drawn inside it by the shader.
    Circle(CircleBucket),
    /// A raster layer's quad.
    ///
    /// Geometry only. A raster tile *is* an image, so what a layer contributes per tile is the
    /// rectangle it is stretched over — the picture arrives as a texture and the colour
    /// adjustment is the layer's rather than the tile's.
    Raster(RasterBucket),
    /// A symbol layer's labels, resolved but not yet shaped.
    ///
    /// The only content that is not geometry. Shaping needs glyph metrics, and the glyphs are a
    /// network resource whose URL is not known until the text has been resolved — so the tile
    /// builder produces the text and the dependencies, and `SymbolLayout::lay_out` produces
    /// vertices once the ranges have arrived. mbgl splits it in the same place.
    Symbol(SymbolLayout),
}

/// One layer's contribution to one tile.
#[derive(Debug, Clone, PartialEq)]
pub struct LayerBucket {
    /// Position in the style document, which is painter order.
    pub layer_index: usize,
    /// Layer id.
    pub layer_id: String,
    /// What it drew.
    pub content: Content,
    /// Resolved paint properties, carrying each one's binding.
    pub paint: alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    /// The interleaved data-driven paint buffer, one entry per vertex.
    ///
    /// Empty-strided when every property is a uniform, which is the common case and is why it
    /// is a field of the bucket rather than a variant of [`Content`]: whether a layer has one
    /// is a property of its paint, not of its geometry.
    pub binder: PaintBinder,
}

impl LayerBucket {
    /// How many drawables this becomes on the stream.
    ///
    /// A fill is two — triangles and outline — and a background is one.
    #[must_use]
    pub fn drawable_count(&self) -> usize {
        if !self.content.has_data() {
            return 0;
        }
        match self.content {
            Content::Background => 1,
            Content::Fill(_) => 2,
            // A line layer is one drawable per tile: unlike a fill it has no outline
            // sublayer, because the extrusion already is the stroke.
            Content::Line(_) => 1,
            // As is a circle. Its stroke is a shader term, not a second draw.
            Content::Circle(_) => 1,
            // And a raster tile, whose quads share one drawable however many the mask made.
            Content::Raster(_) => 1,
            // And a symbol layer, whose labels share one buffer per tile — the golden's
            // twelve-glyph drawable is two labels, not two drawables.
            Content::Symbol(_) => 1,
        }
    }
}

/// Something went wrong building a tile.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TileError {
    /// A layer's filter did not compile.
    #[error("layer `{layer}`: {source}")]
    Filter {
        /// Layer id.
        layer: String,
        /// What went wrong.
        source: tessella_style::FilterError,
    },
    /// A feature's data-driven paint value did not bind.
    #[error("layer `{layer}`: {source}")]
    Binder {
        /// Layer id.
        layer: String,
        /// What went wrong.
        source: BinderError,
    },
    /// A layer's paint properties did not resolve.
    #[error("layer `{layer}`: {source}")]
    Property {
        /// Layer id.
        layer: String,
        /// What went wrong.
        source: tessella_style::PropertyError,
    },
}

/// Builds every implemented layer's contribution to one tile.
///
/// Layers of a kind this build does not implement are skipped, keeping their index. Layers that
/// pass no features still appear, because a layer with an empty bucket is different from a
/// layer that is not there — the first draws nothing this frame, the second is not in the style.
///
/// # Errors
///
/// [`TileError`] when a layer's filter or paint properties do not compile.
pub fn build_tile(
    style: &Style,
    source: &str,
    tile: TileId,
    features: &[GeoJsonFeature],
    options: TilingOptions,
) -> Result<Vec<LayerBucket>, TileError> {
    let (lo, hi) = options.clip_range();
    let (lo, hi) = (f64::from(lo), f64::from(hi));
    let mut buckets = Vec::new();

    for (layer_index, layer) in style.layers.iter().enumerate() {
        if !layer.kind.is_built() || !draws_from(layer, source) {
            continue;
        }

        let paint = resolve_paint(layer).map_err(|source| TileError::Property {
            layer: layer.id.clone(),
            source,
        })?;

        let mut binder = PaintBinder::new(
            paint_specs(&layer.kind).unwrap_or(&[]),
            &paint,
            f64::from(tile.bucket_zoom()),
        );

        let content = match layer.kind {
            // A raster layer draws its tile whether or not anything was decoded for it: the
            // geometry is the rectangle the image goes on, and the image arrives separately as a
            // texture. That is the difference from every other layer here, all of which build
            // from features and produce nothing when there are none.
            //
            // One quad, because the tile mask is not built — see the plan. A mask only differs
            // from the whole tile where a parent is partly covered by its children, which is a
            // state a settled frame at a fixed camera never reaches.
            LayerKind::Raster => Content::Raster(RasterBucket::whole_tile()),
            LayerKind::Symbol => {
                let filter = match &layer.filter {
                    Some(value) => Filter::parse(value).map_err(|source| TileError::Filter {
                        layer: layer.id.clone(),
                        source,
                    })?,
                    None => Filter::always(),
                };

                let zoom = f64::from(tile.bucket_zoom());
                let mut layout = SymbolLayout::new(layer, zoom, tile.overscale_factor());
                let project =
                    |p: &[f64; 2]| projection::tile_local(p[0], p[1], tile.z, tile.x, tile.y);

                for feature in features {
                    if !filter.matches(feature, None) {
                        continue;
                    }

                    // A label is clipped by whether its *anchor* is on the tile, not by cutting
                    // its geometry: half a road name is not a label, and a point label has no
                    // geometry to cut. So the rings go in whole and placement decides.
                    #[allow(clippy::cast_possible_truncation)]
                    let rings: Vec<Vec<(f32, f32)>> = match &feature.geometry {
                        Geometry::Point(points) => points
                            .iter()
                            .map(|point| {
                                let projected = project(point);
                                alloc::vec![(projected[0] as f32, projected[1] as f32)]
                            })
                            .collect(),
                        Geometry::LineString(lines) => lines
                            .iter()
                            .map(|line| {
                                line.iter()
                                    .map(|point| {
                                        let projected = project(point);
                                        (projected[0] as f32, projected[1] as f32)
                                    })
                                    .collect()
                            })
                            .collect(),
                        // A polygon labels at its rings, which is what mbgl does when a symbol
                        // layer reads an area source: the outline is what a line-placed label
                        // follows, and the first vertex is what a point-placed one anchors to.
                        Geometry::Polygon(polygons) => polygons
                            .iter()
                            .flatten()
                            .map(|ring| {
                                ring.iter()
                                    .map(|point| {
                                        let projected = project(point);
                                        (projected[0] as f32, projected[1] as f32)
                                    })
                                    .collect()
                            })
                            .collect(),
                    };

                    layout.push(layer, zoom, feature, &rings);
                }

                // A road is rarely one feature; joining its segments before anything is placed
                // is what makes it long enough to name.
                layout.merge_lines();
                Content::Symbol(layout)
            }
            LayerKind::Background => Content::Background,
            LayerKind::Fill => {
                let filter = match &layer.filter {
                    Some(value) => Filter::parse(value).map_err(|source| TileError::Filter {
                        layer: layer.id.clone(),
                        source,
                    })?,
                    None => Filter::always(),
                };

                // Rings are kept per feature, because that is the boundary `classify_rings`
                // needs: handed a flat list it will attach one feature's hole to another
                // feature's exterior, having nothing in the list to say where one ended.
                let mut per_feature: Vec<Vec<Ring>> = Vec::new();
                let mut kept: Vec<&GeoJsonFeature> = Vec::new();
                for feature in features {
                    if !filter.matches(feature, None) {
                        continue;
                    }
                    // Every geometry type, not just polygons. mbgl's `FillBucket::addFeature`
                    // makes no type check — see the note in `build_mvt_tile` — so a point or a
                    // line in a fill layer becomes a degenerate ring, and `classify_rings`
                    // keeps a lone one because it short-circuits before the area filter.
                    let parts: Vec<&[[f64; 2]]> = match &feature.geometry {
                        Geometry::Polygon(polygons) => polygons
                            .iter()
                            .flat_map(|polygon| polygon.iter().map(Vec::as_slice))
                            .collect(),
                        Geometry::LineString(lines) => lines.iter().map(Vec::as_slice).collect(),
                        Geometry::Point(points) => alloc::vec![points.as_slice()],
                    };
                    let points_only = matches!(feature.geometry, Geometry::Point(_));
                    let mut rings: Vec<Ring> = Vec::new();
                    for ring in parts {
                        let projected: Vec<[f64; 2]> = ring
                            .iter()
                            .map(|p| projection::tile_local(p[0], p[1], tile.z, tile.x, tile.y))
                            .collect();
                        // A point set has no edges to intersect the box with, so the ring clip
                        // would drop it entirely rather than keep the ones inside.
                        let clipped = if points_only {
                            clip_points_to_box(&projected, lo, hi)
                        } else {
                            clip_ring_to_box(&projected, lo, hi)
                        };
                        if clipped.is_empty() {
                            continue;
                        }
                        rings.push(to_tile_ring(&clipped));
                    }
                    if !rings.is_empty() {
                        per_feature.push(rings);
                        kept.push(feature);
                    }
                }
                let borrowed: Vec<&[Ring]> = per_feature.iter().map(Vec::as_slice).collect();
                let (bucket, ends) = fill::build_features_tracked(&borrowed);
                for (feature, end) in kept.iter().zip(&ends) {
                    binder
                        .push(*end, &paint, *feature)
                        .map_err(|source| TileError::Binder {
                            layer: layer.id.clone(),
                            source,
                        })?;
                }
                Content::Fill(bucket)
            }
            LayerKind::Line => {
                let filter = match &layer.filter {
                    Some(value) => Filter::parse(value).map_err(|source| TileError::Filter {
                        layer: layer.id.clone(),
                        source,
                    })?,
                    None => Filter::always(),
                };

                let options = line_options(layer);
                let mut bucket = LineBucket::default();
                let project =
                    |p: &[f64; 2]| projection::tile_local(p[0], p[1], tile.z, tile.x, tile.y);
                for feature in features {
                    if !filter.matches(feature, None) {
                        continue;
                    }
                    match &feature.geometry {
                        Geometry::LineString(lines) => {
                            for line in lines {
                                let projected: Vec<[f64; 2]> = line.iter().map(project).collect();
                                // Each piece the clip returns is a separate polyline with its
                                // own caps, not a continuation: a line that leaves the buffered
                                // box and comes back must not be joined across the gap.
                                for piece in clip_line_to_box(&projected, lo, hi) {
                                    bucket.add_geometry(&to_tile_ring(&piece), &options);
                                }
                            }
                        }
                        // A line layer over polygons draws their outlines. mbgl takes the
                        // feature's own type rather than the layer's, so this is not an odd
                        // case to tolerate — it is how a style strokes a fill without a second
                        // source. The rings clip as rings, not as lines: a ring that leaves the
                        // box re-enters along the box edge, and clipping it open would draw the
                        // detour as a visible chord.
                        Geometry::Polygon(polygons) => {
                            let options = LineOptions {
                                closed: true,
                                ..options
                            };
                            for polygon in polygons {
                                for ring in polygon {
                                    let projected: Vec<[f64; 2]> =
                                        ring.iter().map(project).collect();
                                    let clipped = clip_ring_to_box(&projected, lo, hi);
                                    if !clipped.is_empty() {
                                        bucket.add_geometry(&to_tile_ring(&clipped), &options);
                                    }
                                }
                            }
                        }
                        // A point has no length to extrude.
                        Geometry::Point(_) => continue,
                    }
                    // After the feature's geometry, not before: the count is what says which
                    // vertices are this feature's, and a clip may have produced none.
                    binder
                        .push(bucket.vertices.len(), &paint, feature)
                        .map_err(|source| TileError::Binder {
                            layer: layer.id.clone(),
                            source,
                        })?;
                }
                Content::Line(bucket)
            }
            LayerKind::Circle => {
                let filter = match &layer.filter {
                    Some(value) => Filter::parse(value).map_err(|source| TileError::Filter {
                        layer: layer.id.clone(),
                        source,
                    })?,
                    None => Filter::always(),
                };

                let mut bucket = CircleBucket::default();
                for feature in features {
                    if !filter.matches(feature, None) {
                        continue;
                    }
                    let Geometry::Point(points) = &feature.geometry else {
                        continue;
                    };
                    // Projected but *not* clipped: `add_geometry` drops points outside the tile
                    // proper itself, and the buffered box a clip would use is wider than that.
                    let projected: Vec<Position> = points
                        .iter()
                        .map(|p| {
                            let local = projection::tile_local(p[0], p[1], tile.z, tile.x, tile.y);
                            #[allow(clippy::cast_possible_truncation)]
                            [local[0].round() as i16, local[1].round() as i16]
                        })
                        .collect();
                    bucket.add_geometry(&projected);
                    binder
                        .push(bucket.vertices.len(), &paint, feature)
                        .map_err(|source| TileError::Binder {
                            layer: layer.id.clone(),
                            source,
                        })?;
                }
                Content::Circle(bucket)
            }
            // `is_built` gates this, so anything else is unreachable rather than merely unhandled.
            _ => continue,
        };

        buckets.push(LayerBucket {
            layer_index,
            layer_id: layer.id.clone(),
            content,
            paint,
            binder,
        });
    }

    Ok(buckets)
}

/// Whether a layer draws from this source.
///
/// # Why this is not obvious, and what it cost
///
/// A vector layer names its data twice: `source` picks the source, `source-layer` picks a layer
/// within that source's tile. Matching only on the second is enough for a style with one
/// source and silently wrong for a style with two — every schema calls a layer `water`, so a
/// layer of source B would be built from source A's tile and drawn with data it never asked
/// for. Nothing here had two sources, so nothing failed; a real style has two the moment it
/// overlays a local extract on a world basemap.
///
/// A layer with no source at all — a background — belongs to none of them and is built by
/// [`build_sourceless`] instead, once per tile rather than once per source.
fn draws_from(layer: &tessella_style::Layer, source: &str) -> bool {
    layer.source.as_deref() == Some(source)
}

/// Builds the layers that draw from no source at all.
///
/// A background is one: it fills the viewport rather than reading a tile, so it is per *tile*
/// but not per *source*, and building it inside a source's pass would produce one copy per
/// source of a thing the oracle emits once.
///
/// # Errors
///
/// [`TileError`] when a layer's paint properties do not compile.
pub fn build_sourceless(style: &Style, tile: TileId) -> Result<Vec<LayerBucket>, TileError> {
    let _ = tile;
    let mut buckets = Vec::new();
    for (layer_index, layer) in style.layers.iter().enumerate() {
        if layer.source.is_some() || !layer.kind.is_built() {
            continue;
        }
        let paint = resolve_paint(layer).map_err(|source| TileError::Property {
            layer: layer.id.clone(),
            source,
        })?;
        let binder = PaintBinder::new(
            paint_specs(&layer.kind).unwrap_or(&[]),
            &paint,
            f64::from(tile.bucket_zoom()),
        );
        let content = match layer.kind {
            LayerKind::Background => Content::Background,
            // Every other built kind reads a source, so `layer.source.is_some()` excluded it.
            _ => continue,
        };
        buckets.push(LayerBucket {
            layer_index,
            layer_id: layer.id.clone(),
            content,
            paint,
            binder,
        });
    }
    Ok(buckets)
}

/// Builds a tile's buckets from a decoded vector tile.
///
/// # Why this is not `build_tile` with a different feature type
///
/// The two sources differ in what their coordinates *are*, not merely in how they are spelled.
/// GeoJSON carries longitude and latitude, so it must be projected into the tile and then
/// clipped to the buffered box. A vector tile arrives already tile-local, already clipped by
/// whoever cut it, on a grid it states for itself — so projecting it would be meaningless and
/// clipping it again would only round off the buffer the tiler deliberately included.
///
/// What they share is everything after that: the same filter, the same classification, the same
/// tessellator. So the paths converge at `fill::build` rather than being unified before it.
///
/// # Errors
///
/// [`TileError`] when a layer's filter or paint properties do not compile.
pub fn build_mvt_tile(
    style: &Style,
    source: &str,
    tile: TileId,
    decoded: &tessella_source::mvt::Tile,
) -> Result<Vec<LayerBucket>, TileError> {
    let mut buckets = Vec::new();

    for (layer_index, layer) in style.layers.iter().enumerate() {
        if !layer.kind.is_built() || !draws_from(layer, source) {
            continue;
        }

        let paint = resolve_paint(layer).map_err(|source| TileError::Property {
            layer: layer.id.clone(),
            source,
        })?;

        let mut binder = PaintBinder::new(
            paint_specs(&layer.kind).unwrap_or(&[]),
            &paint,
            f64::from(tile.bucket_zoom()),
        );

        let content = match layer.kind {
            LayerKind::Background => Content::Background,
            LayerKind::Fill => {
                let filter = match &layer.filter {
                    Some(value) => Filter::parse(value).map_err(|source| TileError::Filter {
                        layer: layer.id.clone(),
                        source,
                    })?,
                    None => Filter::always(),
                };

                // A vector layer is addressed by `source-layer`, not by the style layer's own
                // id. A style naming one the tile does not carry draws nothing, which is
                // ordinary: one style serves many tiles and not every tile has every layer.
                let named = layer
                    .source_layer
                    .as_deref()
                    .and_then(|name| decoded.layer(name));

                let mut per_feature: Vec<Vec<Ring>> = Vec::new();
                let mut kept: Vec<tessella_source::mvt::FeatureRef<'_>> = Vec::new();
                if let Some(named) = named {
                    for feature in named.features() {
                        if !filter.matches(&feature, None) {
                            continue;
                        }
                        // No geometry-type check, deliberately. `FillBucket::addFeature` has
                        // none either: it hands whatever the feature carries to
                        // `classifyRings`, and a point or a line becomes a degenerate ring
                        // whose vertices are still written. Filtering here reads as tidiness
                        // and is a divergence — one that the real-style oracle diff found, as
                        // a single missing vertex in a `water` layer whose one point feature
                        // mbgl draws and this did not.
                        let mut rings: Vec<Ring> = Vec::new();
                        let scaled = feature.rings_scaled(EXTENT);
                        for ring in scaled.rings() {
                            #[allow(clippy::cast_possible_truncation)]
                            let ring: Ring = ring
                                .iter()
                                .map(|point| [point[0] as i16, point[1] as i16])
                                .collect();
                            if !ring.is_empty() {
                                rings.push(ring);
                            }
                        }
                        if !rings.is_empty() {
                            per_feature.push(rings);
                            kept.push(feature);
                        }
                    }
                }
                let borrowed: Vec<&[Ring]> = per_feature.iter().map(Vec::as_slice).collect();
                let (bucket, ends) = fill::build_features_tracked(&borrowed);
                for (feature, end) in kept.iter().zip(&ends) {
                    binder
                        .push(*end, &paint, feature)
                        .map_err(|source| TileError::Binder {
                            layer: layer.id.clone(),
                            source,
                        })?;
                }
                Content::Fill(bucket)
            }
            LayerKind::Line => {
                let filter = match &layer.filter {
                    Some(value) => Filter::parse(value).map_err(|source| TileError::Filter {
                        layer: layer.id.clone(),
                        source,
                    })?,
                    None => Filter::always(),
                };

                let named = layer
                    .source_layer
                    .as_deref()
                    .and_then(|name| decoded.layer(name));

                let options = line_options(layer);
                let mut bucket = LineBucket::default();
                if let Some(named) = named {
                    for feature in named.features() {
                        if !filter.matches(&feature, None) {
                            continue;
                        }
                        // Polygons are drawn by a line layer as their own outlines, which is
                        // what `closed` in the generator means; points have no length to
                        // extrude and are dropped the way mbgl drops them.
                        let closed = match feature.geom_type() {
                            tessella_source::mvt::GeomType::LineString => false,
                            tessella_source::mvt::GeomType::Polygon => true,
                            _ => continue,
                        };
                        let options = LineOptions { closed, ..options };
                        // Already tile-local and already clipped by whoever cut the tile, so
                        // the geometry goes straight to the generator; see this function's
                        // note on why that differs from the GeoJSON path.
                        let scaled = feature.rings_scaled(EXTENT);
                        for part in scaled.rings() {
                            #[allow(clippy::cast_possible_truncation)]
                            let part: Ring = part
                                .iter()
                                .map(|point| [point[0] as i16, point[1] as i16])
                                .collect();
                            bucket.add_geometry(&part, &options);
                        }
                        binder
                            .push(bucket.vertices.len(), &paint, &feature)
                            .map_err(|source| TileError::Binder {
                                layer: layer.id.clone(),
                                source,
                            })?;
                    }
                }
                Content::Line(bucket)
            }
            // A raster layer draws its tile whether or not anything was decoded for it: the
            // geometry is the rectangle the image goes on, and the image arrives separately as a
            // texture. That is the difference from every other layer here, all of which build
            // from features and produce nothing when there are none.
            //
            // One quad, because the tile mask is not built — see the plan. A mask only differs
            // from the whole tile where a parent is partly covered by its children, which is a
            // state a settled frame at a fixed camera never reaches.
            LayerKind::Raster => Content::Raster(RasterBucket::whole_tile()),
            LayerKind::Symbol => {
                let filter = match &layer.filter {
                    Some(value) => Filter::parse(value).map_err(|source| TileError::Filter {
                        layer: layer.id.clone(),
                        source,
                    })?,
                    None => Filter::always(),
                };

                let named = layer
                    .source_layer
                    .as_deref()
                    .and_then(|name| decoded.layer(name));

                let zoom = f64::from(tile.bucket_zoom());
                let mut layout = SymbolLayout::new(layer, zoom, tile.overscale_factor());
                if let Some(named) = named {
                    for feature in named.features() {
                        if !filter.matches(&feature, None) {
                            continue;
                        }

                        // No geometry-type check: `symbol-placement` decides what to do with
                        // whatever the feature carries, and a symbol layer over polygons is how
                        // a style labels park and water areas.
                        let scaled = feature.rings_scaled(EXTENT);
                        #[allow(clippy::cast_possible_truncation)]
                        let rings: Vec<Vec<(f32, f32)>> = scaled
                            .rings()
                            .map(|ring| {
                                ring.iter()
                                    .map(|point| (point[0] as f32, point[1] as f32))
                                    .collect()
                            })
                            .collect();

                        layout.push(layer, zoom, &feature, &rings);
                    }
                }
                layout.merge_lines();
                Content::Symbol(layout)
            }
            LayerKind::Circle => {
                let filter = match &layer.filter {
                    Some(value) => Filter::parse(value).map_err(|source| TileError::Filter {
                        layer: layer.id.clone(),
                        source,
                    })?,
                    None => Filter::always(),
                };

                let named = layer
                    .source_layer
                    .as_deref()
                    .and_then(|name| decoded.layer(name));

                let mut bucket = CircleBucket::default();
                if let Some(named) = named {
                    for feature in named.features() {
                        if !filter.matches(&feature, None) {
                            continue;
                        }
                        // A circle layer draws points, and mbgl's `CircleBucket::addFeature`
                        // takes the feature's geometry whatever its type — a line's vertices
                        // each get a disc. So the type is not checked here, the way it is not
                        // checked for a fill.
                        let scaled = feature.rings_scaled(EXTENT);
                        #[allow(clippy::cast_possible_truncation)]
                        let points: Vec<Position> = scaled
                            .rings()
                            .flatten()
                            .map(|point| [point[0] as i16, point[1] as i16])
                            .collect();
                        bucket.add_geometry(&points);
                        binder
                            .push(bucket.vertices.len(), &paint, &feature)
                            .map_err(|source| TileError::Binder {
                                layer: layer.id.clone(),
                                source,
                            })?;
                    }
                }
                Content::Circle(bucket)
            }
            // Every built type has an arm above. Spelled out rather than left to a wildcard:
            // a wildcard here is what let a layer type be enabled in `is_built` and silently
            // draw nothing from a vector tile, which is the quietest kind of gap.
            LayerKind::FillExtrusion
            | LayerKind::Heatmap
            | LayerKind::Hillshade
            | LayerKind::Custom
            | LayerKind::Other(_) => continue,
        };

        buckets.push(LayerBucket {
            layer_index,
            layer_id: layer.id.clone(),
            content,
            paint,
            binder,
        });
    }

    Ok(buckets)
}

/// Rounds a clipped ring into the i16 coordinates the vertex buffer carries.
///
/// The clip box is `-2048..10240`, which fits i16 with room to spare, so the narrowing cannot
/// lose a coordinate that survived clipping. A coordinate that did not survive is not here.
fn to_tile_ring(clipped: &[[f64; 2]]) -> Ring {
    round_to_tile_units(clipped)
        .into_iter()
        .map(|p| {
            #[allow(clippy::cast_possible_truncation)]
            [p[0] as i16, p[1] as i16]
        })
        .collect()
}

/// Reads a line layer's layout properties.
///
/// Only the constant forms are read. `line-cap` and `line-join` are permitted to be
/// zoom-dependent expressions, and `line-join` may additionally be data-driven; a layer using
/// either falls back to the spec default here rather than silently evaluating at the wrong
/// zoom, because the join type changes how many vertices a corner emits and getting it from
/// the wrong zoom would be a structural error, not a cosmetic one.
fn line_options(layer: &tessella_style::Layer) -> LineOptions {
    let literal = |name: &str| match layer.layout.get(name) {
        Some(tessella_style::PropertyValue::Literal(v)) => Some(v),
        _ => None,
    };
    let number = |name: &str, default: f32| {
        literal(name)
            .and_then(tessella_style::Value::as_number)
            .map_or(default, |v| v as f32)
    };
    let cap = match literal("line-cap").and_then(tessella_style::Value::as_str) {
        Some("round") => LineCap::Round,
        Some("square") => LineCap::Square,
        _ => LineCap::Butt,
    };
    LineOptions {
        join: match literal("line-join").and_then(tessella_style::Value::as_str) {
            Some("bevel") => LineJoin::Bevel,
            Some("round") => LineJoin::Round,
            _ => LineJoin::Miter,
        },
        begin_cap: cap,
        end_cap: cap,
        miter_limit: number("line-miter-limit", 2.0),
        round_limit: number("line-round-limit", 1.05),
        overscaling: 1,
        closed: false,
        clip_distances: None,
    }
}

/// Total drawables a tile's buckets become.
#[must_use]
pub fn drawable_count(buckets: &[LayerBucket]) -> usize {
    buckets.iter().map(LayerBucket::drawable_count).sum()
}

/// Looks up a layer's bucket by id.
#[must_use]
pub fn bucket_for<'a>(buckets: &'a [LayerBucket], layer_id: &str) -> Option<&'a LayerBucket> {
    buckets.iter().find(|bucket| bucket.layer_id == layer_id)
}

impl Content {
    /// The raster quad, if this is one.
    #[must_use]
    pub fn as_raster(&self) -> Option<&RasterBucket> {
        match self {
            Self::Raster(bucket) => Some(bucket),
            Self::Background
            | Self::Fill(_)
            | Self::Line(_)
            | Self::Circle(_)
            | Self::Symbol(_) => None,
        }
    }

    /// The symbol layout, if this is one.
    #[must_use]
    pub fn as_symbol(&self) -> Option<&SymbolLayout> {
        match self {
            Self::Symbol(layout) => Some(layout),
            Self::Background
            | Self::Fill(_)
            | Self::Line(_)
            | Self::Circle(_)
            | Self::Raster(_) => None,
        }
    }

    /// The fill bucket, if this is one.
    #[must_use]
    pub fn as_fill(&self) -> Option<&FillBucket> {
        match self {
            Self::Fill(bucket) => Some(bucket),
            Self::Background
            | Self::Line(_)
            | Self::Circle(_)
            | Self::Symbol(_)
            | Self::Raster(_) => None,
        }
    }

    /// The line bucket, if this is one.
    #[must_use]
    pub fn as_line(&self) -> Option<&LineBucket> {
        match self {
            Self::Line(bucket) => Some(bucket),
            Self::Background
            | Self::Fill(_)
            | Self::Circle(_)
            | Self::Symbol(_)
            | Self::Raster(_) => None,
        }
    }

    /// The circle bucket, if this is one.
    #[must_use]
    pub fn as_circle(&self) -> Option<&CircleBucket> {
        match self {
            Self::Circle(bucket) => Some(bucket),
            Self::Background
            | Self::Fill(_)
            | Self::Line(_)
            | Self::Symbol(_)
            | Self::Raster(_) => None,
        }
    }

    /// Whether this contributed any geometry.
    ///
    /// mbgl's `Bucket::hasData`, which is `!segments.empty()` for every bucket type: a layer
    /// whose features all fell outside a tile is still a layer of that tile, and still occupies
    /// its index, but it produces no drawable.
    ///
    /// This only became observable with the circle layer. Every fill and line of the hermetic
    /// style has geometry in all six tiles, so a bucket that drew nothing had never arisen —
    /// while the style's single point lies inside exactly one tile and outside five. Emitting a
    /// drawable for each of those five would put six circles on the stream where the oracle has
    /// one, all but one of them empty.
    ///
    /// A background always has data: it is a viewport quad rather than anything read from a
    /// source.
    #[must_use]
    pub fn has_data(&self) -> bool {
        match self {
            Self::Background => true,
            Self::Fill(bucket) => !bucket.segments.is_empty(),
            Self::Line(bucket) => !bucket.segments.is_empty(),
            Self::Circle(bucket) => !bucket.segments.is_empty(),
            // Labels, not vertices: a symbol layer has data when it resolved text, whether or
            // not the glyphs to shape it with have arrived.
            Self::Symbol(layout) => !layout.is_empty(),
            Self::Raster(bucket) => !bucket.is_empty(),
        }
    }
}

impl TileId {
    /// A tile drawn at its own zoom.
    #[must_use]
    pub const fn new(z: u8, x: u32, y: u32) -> Self {
        Self {
            z,
            x,
            y,
            overscaled_z: z,
        }
    }

    /// A tile standing in above its own zoom.
    ///
    /// # Panics
    ///
    /// When `overscaled_z` is below `z`, which is not overscaling but a different tile.
    #[must_use]
    pub const fn overscaled(z: u8, x: u32, y: u32, overscaled_z: u8) -> Self {
        assert!(
            overscaled_z >= z,
            "overscaled_z is below the tile's own zoom"
        );
        Self {
            z,
            x,
            y,
            overscaled_z,
        }
    }

    /// The zoom this tile's buckets are built for.
    #[must_use]
    pub const fn bucket_zoom(&self) -> u8 {
        self.overscaled_z
    }

    /// How many times over its own size this tile is being drawn.
    ///
    /// mbgl's `overscaleFactor`: one for a tile at its own zoom, two for a parent standing in
    /// one level down, and so on. Line placement needs it so a child's anchors stay aligned with
    /// its parent's — without it every label along a road jumps at a zoom crossing.
    #[must_use]
    pub fn overscale_factor(&self) -> f32 {
        #[allow(clippy::cast_precision_loss)]
        {
            (1u32 << (self.overscaled_z - self.z)) as f32
        }
    }
}

impl core::fmt::Display for TileId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}/{}/{}", self.z, self.x, self.y)?;
        if self.overscaled_z != self.z {
            write!(f, "@{}", self.overscaled_z)?;
        }
        Ok(())
    }
}

impl TileError {
    /// The layer the error came from.
    #[must_use]
    pub fn layer(&self) -> &str {
        match self {
            Self::Filter { layer, .. }
            | Self::Property { layer, .. }
            | Self::Binder { layer, .. } => layer,
        }
    }
}

impl From<&str> for TileId {
    /// Parses `z/x/y`, panicking on anything else. For tests and tooling.
    fn from(text: &str) -> Self {
        let mut parts = text.split('/');
        let mut next = || {
            parts
                .next()
                .and_then(|p| p.parse::<u32>().ok())
                .expect("a z/x/y tile address")
        };
        #[allow(clippy::cast_possible_truncation)]
        let z = next() as u8;
        Self::new(z, next(), next())
    }
}

impl core::str::FromStr for TileId {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = text.split('/').collect();
        if parts.len() != 3 {
            return Err("expected z/x/y".to_string());
        }
        let parse = |value: &str| value.parse::<u32>().map_err(|_| "not a number".to_string());
        #[allow(clippy::cast_possible_truncation)]
        let z = parse(parts[0])? as u8;
        Ok(Self::new(z, parse(parts[1])?, parse(parts[2])?))
    }
}

/// A tile's built buckets, as the store holds them.
pub type CachedTile = alloc::sync::Arc<Vec<LayerBucket>>;

/// Builds tiles through the process-scoped store, so overlapping views share the work.
///
/// This is §5's claim made operational. Without it, N views over one cover do N bucket builds —
/// which is the mbgl model and what the §9.3 flatness counters exist to forbid. With it, the
/// first view to want a tile builds it and the rest get the same `Arc`.
///
/// Sharing is only correct because a bucket is a function of `(source, tile, style revision)`
/// and camera-free (§5.1). Anything per-view — cover decisions, placement, screen-space
/// uniforms — must not be cached here, and is not: this holds buckets, and buckets alone.
#[derive(Debug)]
pub struct TileBuilder {
    store: TileStore<Vec<LayerBucket>>,
    style_rev: u64,
    builds: u64,
}

impl TileBuilder {
    /// A builder over a store of `capacity` tiles.
    #[must_use]
    pub fn new(capacity: usize, style_rev: u64) -> Self {
        Self {
            store: TileStore::new(capacity),
            style_rev,
            builds: 0,
        }
    }

    /// The key a tile occupies in the store.
    ///
    /// Carries the tile's *used* zoom as well as its own. A bucket is only shareable between
    /// views that would build it identically, and a zoom-varying paint property is stored as
    /// its value at `overscaled_z` and `overscaled_z + 1` — so the same canonical tile standing
    /// in at two different zooms is two different buckets. Keying on `(z, x, y)` alone hands
    /// one view the other's endpoints: wrong colours and widths, and invisible at integer zoom,
    /// which is where a person would look first.
    #[must_use]
    pub fn key(&self, source: &str, tile: TileId) -> TileKey {
        TileKey::overscaled(
            source,
            tile.z,
            tile.x,
            tile.y,
            tile.overscaled_z,
            self.style_rev,
        )
    }

    /// Builds a tile, or returns the one already built.
    ///
    /// # Errors
    ///
    /// [`TileError`] when a layer's filter or paint properties do not compile. A tile that
    /// fails is not cached, so the next view attempting it sees the same error rather than a
    /// stale success.
    pub fn build(
        &mut self,
        style: &Style,
        source: &str,
        tile: TileId,
        features: &[GeoJsonFeature],
        options: TilingOptions,
    ) -> Result<(CachedTile, Lookup), TileError> {
        let key = self.key(source, tile);

        if let Some(cached) = self.store.get(&key) {
            return Ok((cached, Lookup::Hit));
        }

        // Built outside `get_or_build` so a failure propagates instead of being cached. A
        // closure returning `Result` would have to store the error or panic, and neither is
        // right: the next view should retry, not inherit a poisoned entry.
        let built = build_tile(style, source, tile, features, options)?;
        self.builds += 1;
        let (cached, lookup) = self.store.get_or_build(&key, || built);
        Ok((cached, lookup))
    }

    /// Marks a tile as held by one more view.
    pub fn retain(&mut self, source: &str, tile: TileId) {
        let key = self.key(source, tile);
        self.store.retain(&key);
    }

    /// Releases one view's hold.
    pub fn release(&mut self, source: &str, tile: TileId) {
        let key = self.key(source, tile);
        self.store.release(&key);
    }

    /// How many tiles were actually built, as opposed to fetched from the store.
    ///
    /// This is the number §9.3 asserts flat in view count.
    #[must_use]
    pub fn builds(&self) -> u64 {
        self.builds
    }

    /// Tiles currently held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.store.len()
    }

    /// True when nothing is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.store.is_empty()
    }
}
