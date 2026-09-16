// SPDX-License-Identifier: BSD-2-Clause
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

use crate::emit::PatternVertices;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tessella_capture_abi::ProjectionMode;
use tessella_layout::circle::CircleBucket;
use tessella_layout::fill::{self, FillBucket, Position, Ring};
use tessella_layout::fill_extrusion::{self, FillExtrusionBucket};
use tessella_layout::heatmap::HeatmapBucket;
use tessella_layout::line::{LineBucket, LineCap, LineJoin, LineOptions};
use tessella_layout::paint::{BinderError, PaintBinder};
use tessella_layout::raster::RasterBucket;
use tessella_layout::subdivide;
use tessella_layout::symbol_layout::SymbolLayout;
use tessella_source::clip::{
    clip_line_to_box, clip_points_to_box, clip_ring_to_box, round_to_tile_units,
};
use tessella_source::geojson::{GeoJsonFeature, Geometry};
use tessella_source::tiling::{EXTENT, TilingOptions};
use tessella_style::property::{ResolvedProperty, paint_specs, resolve_paint};
use tessella_style::{Filter, LayerKind, Style};
use tessella_tile::cover::ViewTransform;
use tessella_tile::projection;
use tessella_tile::store::{Lookup, Surface, TileKey, TileStore};

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
    /// A quad per point, with a Gaussian kernel drawn inside it by the shader.
    ///
    /// The same bucket a circle builds — see `tessella_layout::heatmap` for why that is one
    /// module and not two — under its own variant, because what happens *to* it differs. A
    /// circle's quads are drawn into the frame; these are drawn into the layer's own offscreen
    /// view and the frame samples the result through a color ramp (DR-25).
    Heatmap(HeatmapBucket),
    /// A raster layer's quad and the picture it is stretched over.
    ///
    /// The image rides with the geometry because it *is* the tile: a raster source carries no
    /// features, so there is nothing else the tile could be, and separating them would leave a
    /// quad on the wire sampling a texture nothing had uploaded.
    ///
    /// Shared rather than owned. Two raster layers over one source — a satellite basemap and a
    /// hillshade drawn from the same imagery, or the same source at two opacities — are two
    /// buckets and one picture, and a raster tile is a quarter of a megabyte (§11.5).
    Raster(RasterContent),
    /// A color relief's elevation on the tile's own quad.
    ///
    /// The same quad again, over the *raw* DEM rather than the slope field: a relief reads the
    /// height at a pixel and looks it up in a ramp, where a hillshade reads how the height is
    /// changing. So a style with both draws two layers from one DEM tile and uploads two
    /// pictures of it, which is what mbgl does.
    ColorRelief(ColorReliefContent),
    /// A hillshade's slope field on the tile's own quad.
    ///
    /// The same quad a raster layer draws, over a different picture: not the tile's imagery but
    /// the slope field `Dem::prepare` cut from its elevation. A hillshade layer is a raster layer
    /// whose texture nobody served -- which is why the geometry is a `RasterBucket` and only the
    /// shader and the uniforms differ.
    Hillshade(HillshadeContent),
    /// Extruded polygons: an outline and a roof, with the walls raised by the shader.
    Fill3d(FillExtrusionBucket),
    /// The ground itself: a DEM tile, drawn on the shared terrain mesh.
    ///
    /// The only content that carries no geometry. Every tile of a terrain draws the *same* mesh --
    /// `tessella_layout::terrain::mesh`, a tile's own coordinates and nothing about which tile --
    /// because the height comes from the texture per vertex. So the surface is one geometry and N
    /// uses of it (§5.3), and what differs between two tiles is the DEM named here and the block
    /// that places it.
    Terrain(TerrainContent),
    /// A location indicator's accuracy circle, in world pixels around the puck.
    ///
    /// The one content that belongs to the camera rather than to a tile. Its vertices are offsets
    /// in world pixels at the *current* scale -- mbgl projects the ring at `state.getScale()`, so
    /// a zoom rebuilds the geometry where every other family leaves it alone and changes the
    /// matrix. It is built at a fixed anchor, the way a viewport background is, and rebuilt every
    /// frame.
    LocationIndicator(tessella_layout::location_indicator::LocationIndicatorBucket),
    /// A symbol layer's labels, resolved but not yet shaped.
    ///
    /// The only content that is not geometry. Shaping needs glyph metrics, and the glyphs are a
    /// network resource whose URL is not known until the text has been resolved — so the tile
    /// builder produces the text and the dependencies, and `SymbolLayout::lay_out` produces
    /// vertices once the ranges have arrived. mbgl splits it in the same place.
    Symbol(SymbolLayout),
}

/// A raster layer's contribution to one tile: where the picture goes, and the picture.
#[derive(Debug, Clone, PartialEq)]
pub struct ColorReliefContent {
    /// The quad, or one per entry of the tile's mask.
    pub bucket: RasterBucket,
    /// The DEM as it arrived, bordered, which the shader unpacks per pixel.
    ///
    /// Shared for the reason a slope field is: two relief layers over one source read one tile.
    pub dem: alloc::sync::Arc<tessella_source::dem::Dem>,
}

/// One terrain tile: the elevation it is raised by, and how far.
#[derive(Debug, Clone, PartialEq)]
pub struct TerrainContent {
    /// The DEM as it arrived, bordered, which the vertex stage samples per vertex.
    ///
    /// Shared, for the reason a slope field is: a style may draw a hillshade, a color relief and
    /// the ground itself from one DEM tile, and that is one decode.
    pub dem: alloc::sync::Arc<tessella_source::dem::Dem>,
    /// The style's `terrain.exaggeration`, already clamped to the range the spec gives it.
    pub exaggeration: f32,
    /// How far a skirt vertex hangs below the surface, in meters.
    ///
    /// A function of the tile's own zoom -- `tessella_layout::terrain::skirt_length` -- so it is
    /// per tile and not per frame, which is what keeps the bucket camera-free (§5.1).
    pub skirt: f32,
    /// How finely this tile's ground has to be split, in cells a side.
    ///
    /// From `Relief::cells_within` over the tile's own DEM: the mesh's grid where the ground is
    /// steep, and halved for every level the relief lets it coarsen. Smooth ground reaches one
    /// cell, which is a ground drawn as two triangles and layers on it split not at all.
    ///
    /// This is the number that makes terrain affordable. Split at the mesh's own grid regardless
    /// -- 128 cells a side, 16,384 per tile -- every fill covering a tile becomes tens of
    /// thousands of triangles, and a z14 cover of that took the renderer from settling in 224
    /// ticks to 1,200 and stopped it settling to the same picture twice.
    pub cells: u32,
}

/// The quad a hillshade's slope field is drawn on, and the field.
#[derive(Debug, Clone, PartialEq)]
pub struct HillshadeContent {
    /// The quad, or one per entry of the tile's mask.
    pub bucket: RasterBucket,
    /// The slope field, `dim` by `dim` RGBA, as the prepare pass encodes it.
    ///
    /// Shared, for the reason a raster tile's picture is: two hillshade layers over one DEM
    /// source -- a shaded relief and a steeper one for a contour overlay -- are two buckets and
    /// one slope field.
    pub prepared: alloc::sync::Arc<tessella_source::image::Image>,
    /// The tile's latitude range, north then south, which the shader needs to undo Mercator's
    /// stretch before it reads the slope as a real one.
    pub lat_range: [f32; 2],
}

/// The quad a raster tile's picture is drawn on, and the picture.
#[derive(Debug, Clone, PartialEq)]
pub struct RasterContent {
    /// The quad, or one per entry of the tile's mask.
    pub bucket: RasterBucket,
    /// The decoded tile, RGBA and premultiplied.
    pub image: alloc::sync::Arc<tessella_source::image::Image>,
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
    /// A data-driven pattern's rectangles, one pair per vertex.
    ///
    /// Empty unless the layer's pattern varies with the feature, which is the only case that
    /// cannot travel as a uniform. See [`PatternLookup`].
    pub pattern_vertices: PatternVertices,
    /// Whether a fill's outline draws *under* its triangles rather than over them.
    ///
    /// mbgl's `setSubLayerIndex(unevaluated.get<FillOutlineColor>().isUndefined() ? 2 : 0)`.
    /// Decided here, where the style layer is, because two places downstream need the same
    /// answer -- `order::bindings_for` numbers the drawables and `frame::part_of` maps a number
    /// back to the record it draws -- and a rule evaluated twice is a rule that can disagree
    /// with itself. False for everything that is not a fill.
    pub outline_under_fill: bool,
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
    /// A fill is its triangles and, where it draws one, its outline; a background is one.
    ///
    /// This has to be the number `order::bindings_for` emits, bucket for bucket, because the
    /// frame pairs a tile's bindings with its buckets by counting: each bucket takes the next
    /// `drawable_count` bindings. A fill counted as two while binding one handed its missing
    /// outline's slot to the *next* bucket's first drawable, and every bucket after it in the
    /// tile was encoded under its neighbor's ids. OpenFreeMap's `bright` writes
    /// `fill-antialias: false` on four fills, so its later layers drew each other's geometry --
    /// a transit layer's labels encoded as a dashed stream, placed by a symbol's matrices,
    /// covering the whole frame.
    #[must_use]
    pub fn drawable_count(&self) -> usize {
        if !self.content.has_data() {
            return 0;
        }
        match self.content {
            Content::Background => 1,
            Content::Fill(ref fill) => 1 + usize::from(fill.has_outline()),
            // A line layer is one drawable per tile: unlike a fill it has no outline
            // sublayer, because the extrusion already is the stroke.
            Content::Line(_) => 1,
            // As is a circle. Its stroke is a shader term, not a second draw.
            Content::Circle(_) => 1,
            // And a heatmap's kernels, which are one drawable in the *offscreen* view. The
            // quad that samples that view is per layer rather than per tile, so it is not
            // counted here — nothing in this tile becomes it.
            Content::Heatmap(_) => 1,
            // And a raster tile, whose quads share one drawable however many the mask made.
            Content::Raster(_) => 1,
            // And a hillshade, which is that quad over a slope field instead of a picture.
            Content::Hillshade(_) => 1,
            // And a color relief, which is that quad over the elevation itself.
            Content::ColorRelief(_) => 1,
            // The ground is one drawable: the shared mesh, once per tile.
            Content::Terrain(_) => 1,
            // A puck's accuracy circle is two -- the interior as a fan and the border as a strip,
            // over one vertex buffer, enabled and disabled together -- and then one per image
            // that resolved. The bucket decides, because it is what the encoder reads.
            Content::LocationIndicator(ref puck) => puck.drawables(),
            // An extrusion is two geometries — the roof and the walls raised over it — each
            // drawn once per pass. The depth pass is what stops every wall alpha-blending
            // against the walls behind it, and mbgl's `doDepthPass = (!opaque || hasPattern)`
            // decides whether there is one, so an opaque unpatterned extrusion is two drawables
            // and everything else is four.
            Content::Fill3d(ref bucket) => 2 * (usize::from(bucket.needs_depth_pass()) + 1),
            // And a symbol layer, whose labels share one buffer per tile — the golden's
            // twelve-glyph drawable is two labels, not two drawables. Two when the layer draws
            // sprites as well: the glyphs go through an SDF shader and the sprites through a
            // plain sampler, so the halves cannot share a vertex buffer and are two drawables.
            // The sprites, and the halo and the letters *per font stack* -- see
            // `SymbolLayout::parts`, which is the one place the numbering is decided and what
            // `order::bindings_for` counts the sub-layers with. Spelling the count out a second
            // time here is how this fell out of step: a layer whose `text-font` is data-driven
            // has two stacks and five drawables, this said three, and the frame dropped the two
            // it had not been told about -- every country label in the style, because the layer
            // that decides the count is not the layer that loses the drawables.
            Content::Symbol(ref layout) => layout.parts().len(),
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

/// Fills one pair of rectangles per vertex, from each feature's own pattern.
///
/// `ends` is cumulative, so feature *i* owns the vertices from `ends[i - 1]` to `ends[i]` — the
/// same boundaries the paint binder writes between.
///
/// A feature whose pattern does not resolve gets zeroes rather than being skipped. mbgl does the
/// same and says why: it cannot know at draw time whether every feature resolved, so a buffer
/// short of the vertex count is a read past its end for everything after the gap. Returning an
/// empty set when *nothing* resolved is different — then there is no pattern to draw and the
/// attributes are not written at all.
fn resolve_pattern_vertices(
    patterns: Option<&dyn PatternLookup>,
    layer: &tessella_style::Layer,
    zoom: f64,
    features: &[&dyn tessella_style::expression::Feature],
    ends: &[usize],
) -> PatternVertices {
    let Some(patterns) = patterns else {
        return PatternVertices::default();
    };

    let mut out = PatternVertices::default();
    let mut resolved_any = false;
    let mut start = 0;
    for (feature, end) in features.iter().zip(ends) {
        let pair = patterns.resolve(layer, zoom, *feature);
        resolved_any |= pair.is_some();
        let (from, to) = pair.unwrap_or(([0; 4], [0; 4]));
        for _ in start..*end {
            out.from.push(from);
            out.to.push(to);
        }
        start = *end;
    }

    if resolved_any {
        out
    } else {
        PatternVertices::default()
    }
}

/// Where a data-driven pattern's rectangles come from.
///
/// # Why a trait rather than the atlas itself
///
/// A tile builder has no business knowing what an atlas is. It knows a layer, a zoom and a
/// feature, and what it needs back is the pair of rectangles that feature's pattern resolves to
/// — mbgl threads `patternPositions` into `addFeature` for the same reason and with the same
/// shape. Passing the atlas would put sprite packing, the sheet, and the zoom history into a
/// module whose job is turning geometry into buckets.
pub trait PatternLookup {
    /// The `from` and `to` rectangles for this feature's pattern, or `None` when the layer has
    /// none, the expression names nothing, or the atlas does not hold what it named.
    fn resolve(
        &self,
        layer: &tessella_style::Layer,
        zoom: f64,
        feature: &dyn tessella_style::expression::Feature,
    ) -> Option<([u16; 4], [u16; 4])>;
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
    build_tile_with_patterns(style, source, tile, features, options, None)
}

/// As [`build_tile`], resolving each feature's pattern through `patterns`.
///
/// Separate rather than a sixth parameter on `build_tile`: sixty call sites want the plain one,
/// and a `None` at every one of them says less than a name does.
///
/// # Errors
///
/// As [`build_tile`].
pub fn build_tile_with_patterns(
    style: &Style,
    source: &str,
    tile: TileId,
    features: &[GeoJsonFeature],
    options: TilingOptions,
    patterns: Option<&dyn PatternLookup>,
) -> Result<Vec<LayerBucket>, TileError> {
    build_tile_on_with_patterns(
        style,
        source,
        tile,
        features,
        options,
        patterns,
        Surface::Plane,
    )
}

/// As [`build_tile_with_patterns`], for a named surface.
///
/// A GeoJSON source is split the same way an MVT one is and for the same reason: the geometry is
/// the same shape by the time it reaches the tessellator, and a line of GeoJSON coastline chords
/// through a globe exactly as a vector-tile one does.
///
/// # Errors
///
/// [`TileError`] when a layer's filter or paint properties do not compile.
#[allow(clippy::too_many_arguments)]
pub fn build_tile_on_with_patterns(
    style: &Style,
    source: &str,
    tile: TileId,
    features: &[GeoJsonFeature],
    options: TilingOptions,
    patterns: Option<&dyn PatternLookup>,
    surface: Surface,
) -> Result<Vec<LayerBucket>, TileError> {
    // The grid this tile's fills are split against, derived once. Zero on a plane -- which is the
    // flat path byte for byte, and what the oracle diff compares -- and zero on a sphere above
    // z10, where `edge_segments` asks for a single segment an edge.
    let fill_step = subdivide::step_for_surface(surface, tile.bucket_zoom(), EXTENT);
    let (lo, hi) = options.clip_range();
    let (lo, hi) = (f64::from(lo), f64::from(hi));
    let mut buckets = Vec::new();

    // Filters are evaluated at the tile's own zoom, as mbgl's layouts do — `zoom` there is
    // `tileID.overscaledZ`, and it reaches the filter through the same `EvaluationContext` the
    // paint properties use. A filter written `["step", ["zoom"], …]` is ordinary; evaluated
    // without a zoom it errors, and an erroring filter admits nothing, so the layer would draw
    // nothing at every zoom rather than at the wrong ones.
    let bucket_zoom = f64::from(tile.bucket_zoom());

    for (layer_index, layer) in style.layers.iter().enumerate() {
        if !layer.kind.is_built() || !draws_from(layer, source) || !draws_at(layer, bucket_zoom) {
            continue;
        }

        let paint = resolve_paint(layer).map_err(|source| TileError::Property {
            layer: layer.id.clone(),
            source,
        })?;

        let mut binder =
            PaintBinder::new(paint_specs(&layer.kind).unwrap_or(&[]), &paint, bucket_zoom);
        let mut pattern_vertices = PatternVertices::default();

        let content = match layer.kind {
            // A raster layer over a *feature* source draws nothing, and that is not a silent
            // skip: a raster layer's picture is its source's tile, so pointing one at a vector
            // or GeoJSON source names a source that has no picture to give. `build_raster_tile`
            // is where a raster layer is built, from a decoded image rather than from features.
            LayerKind::Raster => continue,
            LayerKind::Symbol => {
                let filter = match &layer.filter {
                    Some(value) => Filter::parse(value).map_err(|source| TileError::Filter {
                        layer: layer.id.clone(),
                        source,
                    })?,
                    None => Filter::always(),
                };

                let mut layout = SymbolLayout::new(layer, bucket_zoom, tile.overscale_factor());
                for shift in WORLD_COPIES {
                    let offset = world_offset(tile, shift);
                    let project = |p: &[f64; 2]| {
                        let at = projection::tile_local(p[0], p[1], tile.z, tile.x, tile.y);
                        [at[0] + offset, at[1]]
                    };

                    for feature in features {
                        if !copy_reaches(feature, tile, shift, lo, hi) {
                            continue;
                        }
                        if !filter.matches_on(
                            feature,
                            Some(bucket_zoom),
                            Some((tile.z, tile.x, tile.y)),
                        ) {
                            continue;
                        }

                        // A label is clipped by whether its *anchor* is on the tile, not by cutting
                        // its geometry: half a road name is not a label, and a point label has no
                        // geometry to cut. So the rings go in whole and placement decides.
                        //
                        // Rounded to tile units, as every other geometry kind here is. A fill or a
                        // line goes through `to_tile_ring`, which rounds because that is what
                        // geojson-vt does to a tile's coordinates before mbgl ever sees them; this
                        // path did not, and the symbol vertex packs its anchor by *truncating*. So a
                        // label whose anchor fell at 6317.68 was drawn at 6317 where the capture has
                        // 6318 — every point label up to a tile unit out, in a way no test could see
                        // until the probe emitted a per-attribute hash and the parity test stopped
                        // using its own projection.
                        #[allow(clippy::cast_possible_truncation)]
                        let to_tile_units = |p: &[f64; 2]| {
                            let at = project(p);
                            (at[0].round() as f32, at[1].round() as f32)
                        };
                        let rings: Vec<Vec<(f32, f32)>> = match &feature.geometry {
                            Geometry::Point(points) => points
                                .iter()
                                .map(|point| alloc::vec![to_tile_units(point)])
                                .collect(),
                            Geometry::LineString(lines) => lines
                                .iter()
                                .map(|line| line.iter().map(&to_tile_units).collect())
                                .collect(),
                            // A polygon labels at its rings, which is what mbgl does when a symbol
                            // layer reads an area source: the outline is what a line-placed label
                            // follows, and the first vertex is what a point-placed one anchors to.
                            Geometry::Polygon(polygons) => polygons
                                .iter()
                                .flatten()
                                .map(|ring| ring.iter().map(&to_tile_units).collect())
                                .collect(),
                        };

                        // Evaluated here, where the feature is, and written when `build_symbols`
                        // has decided the vertex order. Absent paint is empty rather than an error:
                        // the binder has no slot for a property the layer does not drive.
                        let paint_values = binder.evaluate(&paint, feature).map_err(|source| {
                            TileError::Binder {
                                layer: layer.id.clone(),
                                source,
                            }
                        })?;
                        layout.push(layer, bucket_zoom, feature, &rings, paint_values);
                    }
                }

                // A road is rarely one feature; joining its segments before anything is placed
                // is what makes it long enough to name.
                layout.merge_lines();
                Content::Symbol(layout)
            }
            LayerKind::Background => Content::Background,
            // A color relief reads a height field, so an MVT tile has nothing for it -- the same
            // reason a hillshade is absent from this builder. A location indicator reads neither:
            // it is at a place, and a tile has nothing to say about where the device is.
            // And a terrain reads a height field too: its layer names the DEM source, so the
            // DEM builder is what produces its bucket, not this one.
            LayerKind::ColorRelief | LayerKind::LocationIndicator | LayerKind::Terrain => continue,
            // Extrusions share the arm: they take the same features, the same clipping and the
            // same ring classification, and differ only in what the vertices are packed into.
            // mbgl's two buckets diverge at exactly the same point.
            LayerKind::Fill | LayerKind::FillExtrusion => {
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
                for shift in WORLD_COPIES {
                    let offset = world_offset(tile, shift);
                    for feature in features {
                        if !copy_reaches(feature, tile, shift, lo, hi) {
                            continue;
                        }
                        if !filter.matches_on(
                            feature,
                            Some(bucket_zoom),
                            Some((tile.z, tile.x, tile.y)),
                        ) {
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
                            Geometry::LineString(lines) => {
                                lines.iter().map(Vec::as_slice).collect()
                            }
                            Geometry::Point(points) => alloc::vec![points.as_slice()],
                        };
                        let points_only = matches!(feature.geometry, Geometry::Point(_));
                        let mut rings: Vec<Ring> = Vec::new();
                        for ring in parts {
                            let projected: Vec<[f64; 2]> = ring
                                .iter()
                                .map(|p| {
                                    let at =
                                        projection::tile_local(p[0], p[1], tile.z, tile.x, tile.y);
                                    [at[0] + offset, at[1]]
                                })
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
                }
                let borrowed: Vec<&[Ring]> = per_feature.iter().map(Vec::as_slice).collect();
                let (content, ends) = build_fill_content(layer, &paint, &borrowed, fill_step);
                for (feature, end) in kept.iter().zip(&ends) {
                    binder
                        .push(*end, &paint, *feature)
                        .map_err(|source| TileError::Binder {
                            layer: layer.id.clone(),
                            source,
                        })?;
                }
                // A pattern that varies with the feature, one pair of rectangles per vertex.
                // `ends` is the cumulative vertex count, so each feature owns the range from the
                // previous end to its own — the same boundaries the binder writes between.
                let borrowed_features: Vec<&dyn tessella_style::expression::Feature> = kept
                    .iter()
                    .map(|feature| *feature as &dyn tessella_style::expression::Feature)
                    .collect();
                pattern_vertices = resolve_pattern_vertices(
                    patterns,
                    layer,
                    bucket_zoom,
                    &borrowed_features,
                    &ends,
                );
                content
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
                // geojson-vt's `lineMetrics`: each piece of a line learns where along the whole
                // line it runs, and the bucket spreads its distances over that stretch instead of
                // over the piece alone. Only for a GeoJSON source that asks, and only for lines --
                // geojson-vt gives a polygon's rings no metrics, so its outlines keep their own.
                let metered = matches!(
                    style.source(source),
                    Some(tessella_style::document::Source::Geojson(geojson))
                        if geojson.line_metrics == Some(true)
                );
                let mut bucket = LineBucket::default();
                // The same two lists the fill arm keeps, and for the same reason: a data-driven
                // `line-pattern` needs to know which vertices belong to which feature, and the
                // boundary is already being computed here for the binder — it was simply not
                // being kept.
                let mut kept: Vec<&GeoJsonFeature> = Vec::new();
                let mut ends: Vec<usize> = Vec::new();
                for shift in WORLD_COPIES {
                    let offset = world_offset(tile, shift);
                    let project = |p: &[f64; 2]| {
                        let at = projection::tile_local(p[0], p[1], tile.z, tile.x, tile.y);
                        [at[0] + offset, at[1]]
                    };
                    for feature in features {
                        if !copy_reaches(feature, tile, shift, lo, hi) {
                            continue;
                        }
                        if !filter.matches_on(
                            feature,
                            Some(bucket_zoom),
                            Some((tile.z, tile.x, tile.y)),
                        ) {
                            continue;
                        }
                        match &feature.geometry {
                            Geometry::LineString(lines) => {
                                for line in lines {
                                    let projected: Vec<[f64; 2]> =
                                        line.iter().map(project).collect();
                                    if metered {
                                        // Measured in tile-local units where geojson-vt measures in
                                        // its projected world. The two differ by a uniform scale, so
                                        // the fraction of the line a piece covers is the same number.
                                        let length = tessella_source::clip::line_length(&projected);
                                        for piece in tessella_source::clip::clip_line_to_box_metered(
                                            &projected, lo, hi,
                                        ) {
                                            let ring = to_tile_ring(&piece.points);
                                            // A line of no length has no fraction to give, and mbgl's
                                            // division would carry a NaN into every vertex.
                                            let clip_distances = (length > 0.0).then(|| {
                                                tessella_layout::line::ClipDistances::for_piece(
                                                    &ring,
                                                    piece.seg_start / length,
                                                    piece.seg_end / length,
                                                )
                                            });
                                            bucket.add_geometry(
                                                &ring,
                                                &LineOptions {
                                                    clip_distances,
                                                    ..options
                                                },
                                            );
                                        }
                                        continue;
                                    }
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
                        kept.push(feature);
                        ends.push(bucket.vertices.len());
                    }
                }
                // A `line-pattern` that varies with the feature, exactly as a `fill-pattern`
                // does. The oracle settles what it binds: ids nine and ten at bindings seven and
                // eight, beside the line's own position and normal — the same two rectangles a
                // fill puts at ids four and five, at different slots because the line shader has
                // already spent its low bindings on color, blur, opacity, gapwidth, offset and
                // width.
                let borrowed_features: Vec<&dyn tessella_style::expression::Feature> = kept
                    .iter()
                    .map(|feature| *feature as &dyn tessella_style::expression::Feature)
                    .collect();
                pattern_vertices = resolve_pattern_vertices(
                    patterns,
                    layer,
                    bucket_zoom,
                    &borrowed_features,
                    &ends,
                );
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
                for shift in WORLD_COPIES {
                    let offset = world_offset(tile, shift);
                    for feature in features {
                        if !copy_reaches(feature, tile, shift, lo, hi) {
                            continue;
                        }
                        if !filter.matches_on(
                            feature,
                            Some(bucket_zoom),
                            Some((tile.z, tile.x, tile.y)),
                        ) {
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
                                let local =
                                    projection::tile_local(p[0], p[1], tile.z, tile.x, tile.y);
                                #[allow(clippy::cast_possible_truncation)]
                                [(local[0] + offset).round() as i16, local[1].round() as i16]
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
                }
                Content::Circle(bucket)
            }
            // The same geometry a circle builds, from the same features, by the same rules --
            // points only, projected but not clipped, dropped outside the tile proper. What
            // differs is where the drawable is bound and what the binder puts beside it: two
            // properties, `heatmap-weight` and `heatmap-radius`, against a circle's seven.
            LayerKind::Heatmap => {
                let filter = match &layer.filter {
                    Some(value) => Filter::parse(value).map_err(|source| TileError::Filter {
                        layer: layer.id.clone(),
                        source,
                    })?,
                    None => Filter::always(),
                };

                let mut bucket = HeatmapBucket::default();
                for shift in WORLD_COPIES {
                    let offset = world_offset(tile, shift);
                    for feature in features {
                        if !copy_reaches(feature, tile, shift, lo, hi) {
                            continue;
                        }
                        if !filter.matches_on(
                            feature,
                            Some(bucket_zoom),
                            Some((tile.z, tile.x, tile.y)),
                        ) {
                            continue;
                        }
                        let Geometry::Point(points) = &feature.geometry else {
                            continue;
                        };
                        let projected: Vec<Position> = points
                            .iter()
                            .map(|p| {
                                let local =
                                    projection::tile_local(p[0], p[1], tile.z, tile.x, tile.y);
                                #[allow(clippy::cast_possible_truncation)]
                                [(local[0] + offset).round() as i16, local[1].round() as i16]
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
                }
                Content::Heatmap(bucket)
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
            outline_under_fill: crate::ubo::fill_outline_under_fill(layer),
            pattern_vertices,
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
/// Whether a layer draws at this zoom.
///
/// mbgl's rule, and its asymmetry is the point: `minzoom` is inclusive and `maxzoom` exclusive, so
/// a layer with `maxzoom: 14` is the last thing drawn at 13.9 and gone at 14, while one with
/// `minzoom: 14` starts exactly there. The pair is what lets a style hand a feature from one layer
/// to another at a zoom without drawing it twice or dropping it.
///
/// Not applying this at all was worth about nine thousand labels a frame. liberty's POI layers
/// start at 15, 16 and 17; at z14 they were laid out, shaped, placed and drawn, which is why the
/// map was captioned with shop names the oracle does not show and why the visible type looked
/// larger -- a POI label is set larger than a street label, so drawing the wrong layers changes
/// the apparent size of the text as much as the amount of it.
fn draws_at(layer: &tessella_style::Layer, zoom: f64) -> bool {
    layer.minzoom.is_none_or(|min| zoom >= min) && layer.maxzoom.is_none_or(|max| zoom < max)
}

fn draws_from(layer: &tessella_style::Layer, source: &str) -> bool {
    layer.source.as_deref() == Some(source)
}

/// The world copies a GeoJSON feature is offered to a tile in, as whole worlds east.
///
/// geojson-vt's `wrap`, which mbgl's `GeoJSONVT` runs unconditionally: before any tile is cut, the
/// features are clipped to the world west of this one, to this one and to the one east of it, and
/// the outer two are moved one world across -- the western copy first and the eastern one last,
/// which is the order a tile's buckets receive them in. So a line drawn on past the antimeridian,
/// or a point at longitude 190, lands in this world too and every world copy on screen draws it.
/// Without it, display-line-that-crosses-180th-meridian drew its route only as far as 180.
///
/// Each of those three clips is wider than any tile's buffered box inside it, so clipping a moved
/// copy straight to the tile is the same geometry, and it can be done one tile at a time.
const WORLD_COPIES: [i8; 3] = [1, 0, -1];

/// How far a copy `shift` worlds east sits, in this tile's units.
fn world_offset(tile: TileId, shift: i8) -> f64 {
    f64::from(shift) * f64::from(EXTENT) * 2f64.powi(i32::from(tile.z))
}

/// Whether a feature's copy `shift` worlds east reaches the tile's buffered box.
///
/// This world's copy is always offered, as it was before there were copies, and its own clip
/// decides. The moved copies are tested first so that a tile away from the antimeridian costs one
/// pass over the longitudes rather than two more builds of nothing.
fn copy_reaches(feature: &GeoJsonFeature, tile: TileId, shift: i8, lo: f64, hi: f64) -> bool {
    if shift == 0 {
        return true;
    }
    let span = |(west, east): (f64, f64), p: &[f64; 2]| (west.min(p[0]), east.max(p[0]));
    let empty = (f64::INFINITY, f64::NEG_INFINITY);
    let (west, east) = match &feature.geometry {
        Geometry::Point(points) => points.iter().fold(empty, span),
        Geometry::LineString(lines) => lines.iter().flatten().fold(empty, span),
        Geometry::Polygon(polygons) => polygons.iter().flatten().flatten().fold(empty, span),
    };
    if west > east {
        return false;
    }
    // Mercator's x does not depend on latitude, so the longitudes' extremes are the copy's.
    let offset = world_offset(tile, shift);
    let x = |longitude| projection::tile_local(longitude, 0.0, tile.z, tile.x, tile.y)[0] + offset;
    x(west) <= hi && x(east) >= lo
}

/// Whether the style's background is the one the oracle replaces with a clear.
///
/// mbgl does not draw such a layer at all. `RenderOrchestrator` recognizes it —
/// `backgroundLayerAsColor && layer.baseImpl == layerImpls->front()` with a `getSolidBackground`
/// that answers for a background with no pattern and a positive opacity — takes its color as
/// the frame's clear color and drops the layer from the render items entirely. The clear covers
/// the whole renderable, which is what `commonClearPass` means by "this also paints in areas
/// where we don't have any tiles whatsoever".
///
/// That last part is the difference that shows. `util::tileCover` has no tiles past the pole, so
/// a background drawn per cover tile leaves that region unpainted: at a low zoom under pitch the
/// top of the viewport looks past the world's edge, and where the oracle shows the background
/// color a per-tile background shows whatever the pane was cleared to. Black, in a platform
/// view.
///
/// `backgroundLayerAsColor` is `ContextMode::Unique` — mbgl skips the clear entirely when it
/// shares the context, and draws the layer per cover tile instead. A pane owns its render pass,
/// so the unique case is the one that applies.
///
/// The zoom is needed because `minzoom`/`maxzoom` decide whether the layer draws at all.
///
/// A globe never takes it. The quad is placed by a matrix that does not consult the camera --
/// which is exactly right for something standing in for a clear, and is why it cannot be bent:
/// there is no tile behind it whose Mercator span the vertex stage could turn into a patch of
/// sphere. Bending everything else and leaving this flat draws a rectangle with a curved
/// coastline on it, so a globe takes the per-tile path, where every quad is a tile and every
/// tile has a placement.
#[must_use]
pub fn background_covers_viewport(style: &Style, zoom: f64, projection: ProjectionMode) -> bool {
    if projection == ProjectionMode::Globe {
        return false;
    }
    // A diagnostic escape hatch, read once: the viewport background is one quad over the whole
    // frame ordered by paint order, so it is the first suspect whenever everything under the
    // labels disappears. This is how that is tested without editing a style.
    //
    // Behind `std` for the reason the fade knob in `map.rs` is: reading the environment needs
    // one, and a target without it is not missing a feature.
    #[cfg(feature = "std")]
    {
        static OFF: std::sync::LazyLock<bool> =
            std::sync::LazyLock::new(|| std::env::var("TSL_NO_VIEWPORT_BG").is_ok());
        if *OFF {
            return false;
        }
    }
    let Some(layer) = style.layers.first() else {
        return false;
    };
    if !matches!(layer.kind, LayerKind::Background) || !draws_at(layer, zoom) {
        return false;
    }
    let Ok(paint) = resolve_paint(layer) else {
        return false;
    };
    // A patterned background keeps the per-tile path: its texture coordinates are anchored in
    // world space through the tile matrix, and a viewport quad has no tile to anchor to. mbgl
    // draws it per cover tile for the same reason.
    //
    // The *value*, not the key. `resolve_paint` fills every property the spec declares, so a
    // style that has never heard of `background-pattern` still has the entry -- testing for the
    // key alone put every style on the per-tile path and the fix drew nothing.
    if paint
        .get("background-pattern")
        .and_then(|source| {
            use tessella_style::crossfade::PatternSource as _;
            source.image_at(zoom)
        })
        .is_some_and(|name| !name.is_empty())
    {
        return false;
    }
    // `resolve_paint` fills the spec default, so an unset opacity reads as one here.
    crate::ubo::uniform_number(&paint, "background-opacity", zoom) > 0.0
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
        if layer.source.is_some()
            || !layer.kind.is_built()
            || !draws_at(layer, f64::from(tile.bucket_zoom()))
        {
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
            outline_under_fill: crate::ubo::fill_outline_under_fill(layer),
            // A background and a raster have no features, so no per-feature pattern.
            pattern_vertices: PatternVertices::default(),
        });
    }
    Ok(buckets)
}

/// Builds every location indicator the style draws at this camera.
///
/// Not a tile build, and it takes a camera where every other builder takes a coordinate. A puck's
/// accuracy circle is a ring of offsets in world pixels at the current scale -- mbgl projects it
/// through `state.getScale()` in `updateRadius` -- so the zoom is in the vertices rather than in
/// the matrix and the geometry is rebuilt every frame. The layer is also sourceless and singular:
/// there is one puck, wherever the cover happens to be.
///
/// Empty for a layer mbgl would disable rather than draw: no accuracy radius, or an interior and
/// a border that are both fully transparent. Both circle drawables go together, which is
/// `updateCircleDrawable`'s own test.
///
/// Empty on a globe as well, and that is a decision rather than an omission. The circle is a ring
/// of world-pixel offsets placed by the plane's projection; a sphere has neither. mbgl has no
/// globe and so no puck on one, which leaves nothing to be at parity with -- so this draws none
/// rather than inventing a bend for it.
///
/// # Errors
///
/// [`TileError`] when a layer's paint properties do not compile.
pub fn build_location_indicators(
    style: &Style,
    view: &ViewTransform,
    projection: ProjectionMode,
    sprites: Option<&tessella_glyph::sprite::Positions>,
) -> Result<Vec<LayerBucket>, TileError> {
    let mut buckets = Vec::new();
    if projection == ProjectionMode::Globe {
        return Ok(buckets);
    }
    for (layer_index, layer) in style.layers.iter().enumerate() {
        if layer.kind != LayerKind::LocationIndicator || !draws_at(layer, view.zoom) {
            continue;
        }
        let paint = resolve_paint(layer).map_err(|source| TileError::Property {
            layer: layer.id.clone(),
            source,
        })?;
        let mut bucket = accuracy_circle(&paint, view).unwrap_or_else(
            tessella_layout::location_indicator::LocationIndicatorBucket::without_circle,
        );
        bucket.quads = puck_quads(layer, &paint, view, sprites);
        if bucket.is_empty() {
            continue;
        }
        let binder = PaintBinder::new(paint_specs(&layer.kind).unwrap_or(&[]), &paint, view.zoom);
        buckets.push(LayerBucket {
            layer_index,
            layer_id: layer.id.clone(),
            content: Content::LocationIndicator(bucket),
            paint,
            binder,
            outline_under_fill: false,
            // No features, so nothing to vary a pattern over -- and the family has no pattern.
            pattern_vertices: PatternVertices::default(),
        });
    }
    Ok(buckets)
}

/// Whether a puck's paint draws an accuracy circle at all.
///
/// mbgl's `updateCircleDrawable` test, which enables and disables both circle drawables together:
/// a radius of nothing, or an interior and a border that are both fully transparent.
///
/// Public within the crate because the frame asks the same question from the other end. It has
/// the paint and the bindings but not the bucket, and it has to know whether sub-layers zero and
/// one are the circle's or the first two quads' -- a puck with images and no radius starts its
/// shadow at zero. Asking the paint twice is what keeps the two answers one answer.
pub(crate) fn puck_draws_circle(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    view: &ViewTransform,
) -> bool {
    let radius = f64::from(crate::ubo::uniform_number(
        paint,
        "accuracy-radius",
        view.zoom,
    ));
    if radius <= 0.0 {
        return false;
    }
    let interior = crate::ubo::uniform_color(paint, "accuracy-radius-color", view.zoom);
    let border = crate::ubo::uniform_color(paint, "accuracy-radius-border-color", view.zoom);
    interior.a != 0.0 || border.a != 0.0
}

/// The accuracy circle a puck's paint asks for, or nothing where mbgl would draw none.
fn accuracy_circle(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    view: &ViewTransform,
) -> Option<tessella_layout::location_indicator::LocationIndicatorBucket> {
    if !puck_draws_circle(paint, view) {
        return None;
    }
    let radius = f64::from(crate::ubo::uniform_number(
        paint,
        "accuracy-radius",
        view.zoom,
    ));
    let location = puck_location(paint, view.zoom)?;
    Some(
        tessella_layout::location_indicator::LocationIndicatorBucket::new(
            location,
            radius,
            view.bearing,
            tessella_tile::camera::world_size(view.zoom),
        ),
    )
}

/// The three textured quads a puck's images resolve to, in painter order.
///
/// Empty for every image the sheet has not got, rather than a quad with no area. mbgl builds all
/// three drawables always and lets a missing image give one a width of zero, which rasterizes
/// nothing; skipping it here is the same picture with one fewer drawable, and the bucket is the
/// one place the count is decided so nothing downstream can disagree.
///
/// # Why this needs the camera
///
/// Because the size does. `horizontal` is mbgl's `horizontalScaleFactor`, which mixes one toward
/// the world-pixel size of a *screen* pixel measured at the puck by `perspective-compensation`.
/// At zero the puck is a fixed number of world pixels and shrinks with the perspective like the
/// ground it sits on; at one it is a fixed number of screen pixels and stays the size of a
/// fingertip. The clamp to 0.8 is mbgl's own, with its own reason: a puck close to the camera
/// would otherwise grow without bound.
fn puck_quads(
    layer: &tessella_style::Layer,
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    view: &ViewTransform,
    sprites: Option<&tessella_glyph::sprite::Positions>,
) -> Vec<tessella_layout::location_indicator::PuckQuad> {
    use tessella_layout::location_indicator::{PuckImage, PuckQuad, puck_quad};

    let mut quads = Vec::new();
    let (Some(positions), Some(location)) = (sprites, puck_location(paint, view.zoom)) else {
        return quads;
    };
    let Some(screen_pixel) =
        tessella_tile::screen::world_pixels_per_screen_pixel(view, location[0], location[1])
    else {
        return quads;
    };

    let compensation = f64::from(crate::ubo::uniform_number(
        paint,
        "perspective-compensation",
        view.zoom,
    ));
    let horizontal = (1.0 - compensation) + screen_pixel.clamp(0.8, 10.1) * compensation;
    let displacement = f64::from(crate::ubo::uniform_number(
        paint,
        "image-tilt-displacement",
        view.zoom,
    ));
    let bearing = f64::from(crate::ubo::uniform_number(paint, "bearing", view.zoom));
    let tilt = tessella_tile::camera::pitch_radians(view);
    let shift = vertical_shift(view, location).unwrap_or([0.0, 0.0]);

    for image in PuckImage::ALL {
        let Some(name) =
            tessella_style::property::layout_value(layer, image.layout_property(), view.zoom, None)
        else {
            continue;
        };
        let Some(position) = name.as_str().and_then(|name| positions.get(name)) else {
            continue;
        };
        // The image's *logical* width, which is its pixels over its pixel ratio -- a sprite drawn
        // at twice the density is the same size on the map. Width alone, as mbgl reads it: the
        // quad is square whatever the picture's aspect, so a tall image is squashed into it.
        let (width, _) = position.display_size();
        let size = f64::from(crate::ubo::uniform_number(
            paint,
            image.size_property(),
            view.zoom,
        ));
        let half_diagonal = width * size * core::f64::consts::SQRT_2 * 0.5 * horizontal;
        let along = tilt * image.displacement_sign() * displacement * horizontal;
        quads.push(PuckQuad {
            image,
            corners: puck_quad(half_diagonal, bearing, [shift[0] * along, shift[1] * along]),
        });
    }
    quads
}

/// Which way is up the screen, expressed in world pixels, measured at the bottom of the viewport.
///
/// mbgl's `hatShadowShiftVector`, and its comment is the explanation: the obvious answer -- the
/// bearing's own up vector -- is only right down the vertical center line of the map, because the
/// perspective skews every other column toward the vanishing point. So the direction is found in
/// *screen* space, where up is up everywhere, and converted back.
///
/// The measurement is taken at the bottom edge rather than at the puck. mbgl says why: going
/// further from the convergence point gives a more convincing lift, and the bottom of the window
/// is the furthest it can go without picking up the wide skew near the top.
///
/// # Two screen conventions, one flip apart
///
/// [`tessella_tile::screen`] is `TransformState`'s, where y grows *up* from the bottom edge --
/// checked against mbgl's own numbers, which agree to eight figures. The location indicator layer
/// does not use that one. It declares its own `latLngToScreenCoordinate` and
/// `screenCoordinateToLatLng` beside it, each flipping y by `height - y`, so everything inside
/// that file is in viewport coordinates with y down from the top. That is why its comments read
/// the way they do: `posScreen.y = params.height - 1` really is "moving it to bottom" there.
///
/// So the flip is written out here and mbgl's two lines are transcribed literally on the other
/// side of it. Taken without the flip the puck's shadow rises and its hat sinks -- which at a
/// pitched camera is 72 gross pixels and a shadow above the thing casting it.
///
/// `None` when the view will not project, which is the same view that has no puck.
fn vertical_shift(view: &ViewTransform, location: [f64; 2]) -> Option<[f64; 2]> {
    // Into the layer's own convention, and back out of it when asking `screen` anything.
    let viewport = |y: f64| view.height - y;

    let screen = tessella_tile::screen::to_screen(view, location[0], location[1])?;
    // mbgl's `posScreen.y = params.height - 1`: the bottom row of the viewport.
    let bottom = [screen[0], view.height - 1.0];
    // And its `screenDy.y -= 1`: one pixel up the screen from there.
    let above = [bottom[0], bottom[1] - 1.0];

    let world = tessella_tile::camera::world_size(view.zoom);
    let at = tessella_tile::screen::from_screen(view, [bottom[0], viewport(bottom[1])])?;
    let here = projection::project(at[0], at[1], world);
    let up = tessella_tile::screen::from_screen(view, [above[0], viewport(above[1])])?;
    let there = projection::project(up[0], up[1], world);

    let delta = [there[0] - here[0], there[1] - here[1]];
    let length = delta[0].hypot(delta[1]);
    (length > 0.0).then(|| [delta[0] / length, delta[1] / length])
}

/// Where a puck is, as longitude then latitude.
///
/// `location` is latitude, longitude, altitude -- the order mbgl's `std::array<double, 3>` is
/// filled in and not the order the rest of a style writes a coordinate in. It is turned over here,
/// once, so that nothing downstream has to remember. The altitude is read and discarded: mbgl's
/// puck is placed by `LatLng` and its third number reaches no geometry.
///
/// Read in both places the puck needs it -- the circle's vertices and the matrix that places them
/// -- so that the two cannot disagree about which number is which.
pub(crate) fn puck_location(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    zoom: f64,
) -> Option<[f64; 2]> {
    let location = paint.get("location")?.coordinates_at(zoom)?;
    let [latitude, longitude] = location.get(..2)?.try_into().ok()?;
    Some([longitude, latitude])
}

/// Builds a flat fill or an extrusion from the same classified rings.
///
/// The two diverge only at the packing. An extrusion additionally needs to know whether the
/// layer is opaque, which decides how many drawables it becomes rather than what its geometry
/// is — mbgl's `opaque = evaluated.get<FillExtrusionOpacity>() >= 1`, read from the resolved
/// paint here for the same reason.
fn build_fill_content(
    layer: &tessella_style::Layer,
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    rings: &[&[Ring]],
    step: i32,
) -> (Content, Vec<usize>) {
    if layer.kind == LayerKind::FillExtrusion {
        // A data-driven opacity cannot be resolved to one number here, and mbgl does not try:
        // `FillExtrusionOpacity` is data-constant in the spec, so the value is always a
        // uniform. A style that made it otherwise is refused before this by the property table.
        // Zoom zero: `fill-extrusion-opacity` is data-constant in the spec but may still be a
        // zoom function, and mbgl reads it from the evaluated properties at the frame's zoom.
        // Reading it at the bucket's own zoom is the closest this side has — and the only thing
        // it decides is the drawable count, which a zoom-varying opacity would change between
        // frames either way.
        let opaque = crate::ubo::uniform_opacity(paint, "fill-extrusion-opacity") >= 1.0;
        // mbgl's `hasPattern`, and unevaluated: what matters is that the style asks for a
        // pattern, not that the atlas had one.
        let patterned = layer.paint.contains_key("fill-extrusion-pattern");
        let (bucket, ends) = fill_extrusion::build_features_tracked(rings, opaque, patterned);
        return (Content::Fill3d(bucket), ends);
    }
    // Split against the grid a globe would bend this level's tiles on. Unconditional, and it
    // costs a flat map nothing worth measuring: `step_for_level` answers zero from z11 up, so
    // every tile at the zooms a map is usually read at takes the same path it always did, and
    // below that a tile-covering ring is at most 42 vertices a side. Keyed to the tile's level and
    // not the camera, so §5.1's bucket stays camera-free and one set of vertices still serves a
    // globe view and a flat one at once -- which is the alternative this avoids: a bucket keyed by
    // surface, built twice, in an app that shows both.
    // Which outline this layer draws, if it draws one. `fill-antialias` and the pattern rule
    // settle whether, and the outline's own paint settles which form -- see `Outlines`.
    //
    // The polyline is not gated on the pattern: `encode_parts` decides that against the atlas
    // the frame actually resolved, and a style naming a sprite that never arrived draws as a
    // plain fill, which would then want the geometry this skipped. A patterned layer that draws
    // an outline therefore carries both forms, which is the only case that does.
    let outlines = if crate::ubo::fill_draws_outline(layer, paint) {
        fill::Outlines {
            lines: !crate::ubo::fill_outline_triangulates(paint, false)
                || layer.paint.contains_key("fill-pattern"),
            polyline: crate::ubo::fill_outline_triangulates(paint, false),
        }
    } else {
        fill::Outlines {
            lines: false,
            polyline: false,
        }
    };
    let (bucket, ends) = fill::build_features_tracked_on(rings, step, outlines);
    (Content::Fill(bucket), ends)
}

/// Builds a tile's buckets from a decoded raster image.
///
/// # Why this is a third builder rather than an arm of the other two
///
/// The other two take features and differ only in what a coordinate means. This takes no
/// features at all: a raster tile carries none, and every step the feature builders share —
/// filter, classify, tessellate, bind — has nothing to act on. What is left is a rectangle and a
/// picture, and expressing that as a feature builder with every stage skipped would be a
/// pipeline shaped around data that is not there.
///
/// It is also why a raster layer draws with an *empty* source tile and a fill layer does not. A
/// fill with no features has nothing to draw and correctly draws nothing; a raster tile with no
/// features is every raster tile there is.
///
/// The image is shared across the layers built here rather than copied into each. A style
/// drawing one imagery source twice — a base pass and a tinted overlay — is two buckets over one
/// picture, and the picture is a quarter of a megabyte.
///
/// # Errors
///
/// [`TileError`] when a layer's paint properties do not compile. There is no filter to compile:
/// a filter selects features and a raster tile has none, so `filter` on a raster layer is
/// ignored exactly as the spec says it is.
///
/// There is no tile id either, which the other two take. The geometry depends on the tile's
/// *mask* rather than on its address — which sub-tiles no better tile has covered — and that is a
/// question about the moment rather than about where the tile is. The mask therefore arrives as
/// an argument: it belongs to the view's renderable set, and two views loading at different rates
/// hold different masks for the same tile.
pub fn build_raster_tile(
    style: &Style,
    source: &str,
    image: alloc::sync::Arc<tessella_source::image::Image>,
    mask: &[tessella_tile::mask::MaskEntry],
) -> Result<Vec<LayerBucket>, TileError> {
    build_raster_tile_on(style, source, image, mask, 1)
}

/// As [`build_raster_tile`], with each mask entry gridded `cells` a side.
///
/// A sphere needs the grid for the reason `RasterBucket::add_quad_on` gives: four corners bent
/// onto it is a flat sheet through it. `cells` of one is the flat path byte for byte.
///
/// # Errors
///
/// As [`build_raster_tile`].
pub fn build_raster_tile_on(
    style: &Style,
    source: &str,
    image: alloc::sync::Arc<tessella_source::image::Image>,
    mask: &[tessella_tile::mask::MaskEntry],
    cells: u32,
) -> Result<Vec<LayerBucket>, TileError> {
    let mut buckets = Vec::new();

    for (layer_index, layer) in style.layers.iter().enumerate() {
        if layer.kind != LayerKind::Raster || !draws_from(layer, source) {
            continue;
        }

        let paint = resolve_paint(layer).map_err(|source| TileError::Property {
            layer: layer.id.clone(),
            source,
        })?;

        buckets.push(LayerBucket {
            layer_index,
            layer_id: layer.id.clone(),
            content: Content::Raster(RasterContent {
                bucket: RasterBucket::masked_on(mask, cells),
                image: alloc::sync::Arc::clone(&image),
            }),
            paint,
            // None of a raster layer's paint properties is data-driven, and that is structural
            // rather than a gap: there is no feature for one to vary over. The same is why it
            // carries no pattern rectangles.
            binder: PaintBinder::default(),
            // A raster layer has no outline. Stated rather than defaulted: every other bucket
            // says what it is, and one that did not would be the one nobody checked.
            outline_under_fill: false,
            pattern_vertices: PatternVertices::default(),
        });
    }
    Ok(buckets)
}

/// Builds a hillshade tile's buckets from a decoded DEM.
///
/// [`build_raster_tile_on`] over a slope field. The quad and the mask are the raster path's --
/// mbgl's `HillshadeBucket` shares `RasterBucket`'s mask handling, which is what
/// `tessella_tile::mask` already records -- and what differs is which layers ask for it and that
/// the picture is cut here rather than served.
///
/// The prepare pass runs once per tile, here, rather than per layer: two hillshade layers over one
/// DEM source read the same slope field, the same way two raster layers over one source read the
/// same picture.
///
/// # Errors
///
/// [`TileError::Property`] when a layer's paint does not resolve.
pub fn build_dem_tile_on(
    style: &Style,
    source: &str,
    dem: &tessella_source::dem::Dem,
    tile: TileId,
    mask: &[tessella_tile::mask::MaskEntry],
    cells: u32,
) -> Result<Vec<LayerBucket>, TileError> {
    let mut buckets = Vec::new();
    let wants = style
        .layers
        .iter()
        .any(|layer| layer.kind == LayerKind::Hillshade && draws_from(layer, source));
    if !wants {
        return Ok(buckets);
    }

    // The tile's own zoom, not the cover's: the prepare pass reads `tileID.canonical.z`, and an
    // overscaled tile's slope field is the one its own level produced.
    let prepared = alloc::sync::Arc::new(dem.prepare(tile.z));
    let lat_range = lat_range(tile);

    for (layer_index, layer) in style.layers.iter().enumerate() {
        if layer.kind != LayerKind::Hillshade || !draws_from(layer, source) {
            continue;
        }

        let paint = resolve_paint(layer).map_err(|source| TileError::Property {
            layer: layer.id.clone(),
            source,
        })?;

        buckets.push(LayerBucket {
            layer_index,
            layer_id: layer.id.clone(),
            content: Content::Hillshade(HillshadeContent {
                bucket: RasterBucket::masked_on(mask, cells),
                prepared: alloc::sync::Arc::clone(&prepared),
                lat_range,
            }),
            paint,
            // A hillshade's paint has no feature to vary over, for a raster layer's reason.
            binder: PaintBinder::default(),
            outline_under_fill: false,
            pattern_vertices: PatternVertices::default(),
        });
    }
    Ok(buckets)
}

/// How finely a DEM tile's ground has to be split, in cells a side.
///
/// The mesh's own grid where the ground is steep, halved for every level the tile's relief lets
/// it coarsen — `Relief::cells_within` against the bound `split_relief` computes for this zoom
/// and latitude. Both were written for this and nothing asked until now.
///
/// The bound is a screen-space one: half a pixel of vertical error at the worst pitch the camera
/// allows, at the top of the zoom level, so it holds for every camera a bucket built here will be
/// drawn under (§5.1). Smooth ground reaches one cell, which is a ground drawn as two triangles
/// and the layers standing on it split not at all.
fn terrain_cells(dem: &tessella_source::dem::Dem, tile: TileId, exaggeration: f64) -> u32 {
    /// Half a pixel of vertical error, which is the tolerance every other split in this build is
    /// solved for — `globe::edge_segments` uses the same one.
    const TOLERANCE: f64 = 0.5;
    let base = u32::from(tessella_layout::terrain::MESH_SIZE);
    // The tile's own center, because a world pixel is a different number of meters at Berlin
    // than at the equator.
    let side = 1u32 << tile.z;
    #[allow(clippy::cast_precision_loss)]
    let latitude =
        tessella_tile::camera::latitude_of((f64::from(tile.y) + 0.5) / f64::from(side).max(1.0));
    let relief = tessella_source::terrain::split_relief(tile.z, latitude, exaggeration, TOLERANCE);
    #[allow(clippy::cast_possible_truncation)]
    let relief = relief as f32;
    tessella_source::terrain::Relief::new(dem, base).cells_within(relief)
}

/// Builds the ground's bucket from a decoded DEM.
///
/// [`build_dem_tile_on`]'s other sibling, and not its arm for the same reason: a style may want a
/// hillshade, a relief and the ground from one tile, or any one of the three, and a builder that
/// produced all of them would upload two pictures a style never asked for.
///
/// One bucket at most. The terrain layer is synthesized -- `Style::synthesize_terrain` -- so there
/// is exactly one of it or none.
///
/// # Errors
///
/// [`TileError::Property`] when the layer's paint does not resolve, which for a synthesized layer
/// with no paint at all is not reachable.
pub fn build_terrain_tile_on(
    style: &Style,
    source: &str,
    dem: &alloc::sync::Arc<tessella_source::dem::Dem>,
    tile: TileId,
) -> Result<Vec<LayerBucket>, TileError> {
    let mut buckets = Vec::new();
    let Some(terrain) = style.terrain.as_ref() else {
        return Ok(buckets);
    };
    for (layer_index, layer) in style.layers.iter().enumerate() {
        if layer.kind != LayerKind::Terrain || !draws_from(layer, source) {
            continue;
        }
        let paint = resolve_paint(layer).map_err(|source| TileError::Property {
            layer: layer.id.clone(),
            source,
        })?;
        #[allow(clippy::cast_possible_truncation)]
        buckets.push(LayerBucket {
            layer_index,
            layer_id: layer.id.clone(),
            content: Content::Terrain(TerrainContent {
                dem: alloc::sync::Arc::clone(dem),
                exaggeration: terrain.exaggeration() as f32,
                cells: terrain_cells(dem, tile, terrain.exaggeration()),
                // The tile's own zoom, not the cover's: a skirt hides the seam between this tile
                // and its neighbors, and how wide that seam can be is a property of the level.
                skirt: tessella_layout::terrain::skirt_length(f64::from(tile.z)) as f32,
            }),
            paint,
            binder: PaintBinder::default(),
            outline_under_fill: false,
            pattern_vertices: PatternVertices::default(),
        });
    }
    Ok(buckets)
}

/// Builds a color relief tile's buckets from a decoded DEM.
///
/// [`build_dem_tile_on`]'s sibling and deliberately not its arm: the two read the same tile and
/// want different pictures of it, and a builder that produced both would upload a slope field for
/// a style that asked only for a relief.
///
/// # Errors
///
/// [`TileError::Property`] when a layer's paint does not resolve.
pub fn build_relief_tile_on(
    style: &Style,
    source: &str,
    dem: &alloc::sync::Arc<tessella_source::dem::Dem>,
    mask: &[tessella_tile::mask::MaskEntry],
    cells: u32,
) -> Result<Vec<LayerBucket>, TileError> {
    let mut buckets = Vec::new();

    for (layer_index, layer) in style.layers.iter().enumerate() {
        if layer.kind != LayerKind::ColorRelief || !draws_from(layer, source) {
            continue;
        }

        let paint = resolve_paint(layer).map_err(|source| TileError::Property {
            layer: layer.id.clone(),
            source,
        })?;

        buckets.push(LayerBucket {
            layer_index,
            layer_id: layer.id.clone(),
            content: Content::ColorRelief(ColorReliefContent {
                bucket: RasterBucket::masked_on(mask, cells),
                dem: alloc::sync::Arc::clone(dem),
            }),
            paint,
            binder: PaintBinder::default(),
            outline_under_fill: false,
            pattern_vertices: PatternVertices::default(),
        });
    }
    Ok(buckets)
}

/// A tile's north and south edges in degrees, which is mbgl's `getLatRange`.
///
/// North first. The shader interpolates between them across the tile to undo Mercator's stretch
/// before it reads the encoded slope as a real one -- without it a hillshade at high latitude
/// reads far steeper than the ground is.
fn lat_range(tile: TileId) -> [f32; 2] {
    lat_range_of(tile.z, tile.x, tile.y)
}

/// A tile's north and south edges in degrees, from its coordinate.
///
/// Public because the uniform that carries it is written where the *bindings* are, a frame later
/// and a file away from where the bucket was built, and a second derivation of one number is a
/// second thing to keep in agreement.
#[must_use]
pub fn lat_range_of(z: u8, x: u32, y: u32) -> [f32; 2] {
    let scale = f64::from(1u32 << z);
    let (_, north) = tessella_tile::projection::unproject([f64::from(x), f64::from(y)], scale);
    let (_, south) =
        tessella_tile::projection::unproject([f64::from(x), f64::from(y) + 1.0], scale);
    #[allow(clippy::cast_possible_truncation)]
    [north as f32, south as f32]
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
    build_mvt_tile_with_patterns(style, source, tile, decoded, None)
}

/// As [`build_mvt_tile`], for a named surface.
///
/// [`Surface::Plane`] is this function's other name: the buffers are the ones the oracle diff
/// compares, byte for byte. [`Surface::Sphere`] splits fill geometry against a grid so that
/// bending it per vertex follows the sphere rather than chording through it -- which is a
/// different vertex buffer, and the reason the surface is part of a tile's key rather than a
/// parameter of drawing it.
///
/// # Errors
///
/// [`TileError`] when a layer's filter or paint properties do not compile.
pub fn build_mvt_tile_on(
    style: &Style,
    source: &str,
    tile: TileId,
    decoded: &tessella_source::mvt::Tile,
    surface: Surface,
) -> Result<Vec<LayerBucket>, TileError> {
    build_mvt_tile_on_with_patterns(style, source, tile, decoded, None, surface)
}

/// As [`build_mvt_tile`], resolving each feature's pattern through `patterns`.
///
/// # Errors
///
/// As [`build_mvt_tile`].
pub fn build_mvt_tile_with_patterns(
    style: &Style,
    source: &str,
    tile: TileId,
    decoded: &tessella_source::mvt::Tile,
    patterns: Option<&dyn PatternLookup>,
) -> Result<Vec<LayerBucket>, TileError> {
    build_mvt_tile_on_with_patterns(style, source, tile, decoded, patterns, Surface::Plane)
}

/// As [`build_mvt_tile_with_patterns`], for a named surface.
///
/// # Errors
///
/// [`TileError`] when a layer's filter or paint properties do not compile.
pub fn build_mvt_tile_on_with_patterns(
    style: &Style,
    source: &str,
    tile: TileId,
    decoded: &tessella_source::mvt::Tile,
    patterns: Option<&dyn PatternLookup>,
    surface: Surface,
) -> Result<Vec<LayerBucket>, TileError> {
    // The grid this tile's fills are split against, derived once. Zero on a plane -- which is
    // the flat path byte for byte, and what the oracle diff compares -- and zero on a sphere
    // above z10, where `edge_segments` asks for a single segment an edge.
    let fill_step = subdivide::step_for_surface(surface, tile.bucket_zoom(), EXTENT);
    let mut buckets = Vec::new();

    // Filters are evaluated at the tile's own zoom, as mbgl's layouts do — `zoom` there is
    // `tileID.overscaledZ`, and it reaches the filter through the same `EvaluationContext` the
    // paint properties use. A filter written `["step", ["zoom"], …]` is ordinary; evaluated
    // without a zoom it errors, and an erroring filter admits nothing, so the layer would draw
    // nothing at every zoom rather than at the wrong ones.
    let bucket_zoom = f64::from(tile.bucket_zoom());

    for (layer_index, layer) in style.layers.iter().enumerate() {
        if !layer.kind.is_built() || !draws_from(layer, source) || !draws_at(layer, bucket_zoom) {
            continue;
        }

        let paint = resolve_paint(layer).map_err(|source| TileError::Property {
            layer: layer.id.clone(),
            source,
        })?;

        let mut binder =
            PaintBinder::new(paint_specs(&layer.kind).unwrap_or(&[]), &paint, bucket_zoom);
        let mut pattern_vertices = PatternVertices::default();

        let content = match layer.kind {
            LayerKind::Background => Content::Background,
            // A color relief reads a height field, so an MVT tile has nothing for it -- the same
            // reason a hillshade is absent from this builder. A location indicator reads neither:
            // it is at a place, and a tile has nothing to say about where the device is.
            // And a terrain reads a height field too: its layer names the DEM source, so the
            // DEM builder is what produces its bucket, not this one.
            LayerKind::ColorRelief | LayerKind::LocationIndicator | LayerKind::Terrain => continue,
            // Extrusions share the arm: they take the same features, the same clipping and the
            // same ring classification, and differ only in what the vertices are packed into.
            // mbgl's two buckets diverge at exactly the same point.
            LayerKind::Fill | LayerKind::FillExtrusion => {
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
                        if !filter.matches_on(
                            &feature,
                            Some(bucket_zoom),
                            Some((tile.z, tile.x, tile.y)),
                        ) {
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
                        // MVT version 1 left ring winding unspecified, so the exterior-and-holes
                        // structure has to be re-derived rather than read. mbgl gates the same
                        // repair on the same version, and skipping it on a v1 world tile costs
                        // both Americas.
                        // MVT version 1 left ring winding unspecified, so the exterior-and-holes
                        // structure has to be re-derived rather than read. mbgl gates the same
                        // repair on the same version, and skipping it on a v1 world tile costs
                        // both Americas.
                        //
                        // Every v1 polygon, as mbgl does, rather than only the ones that look
                        // broken. Repairing selectively was measured and gained 0.09% of a z0
                        // world frame -- not enough to buy a rule about which geometry to trust,
                        // and there is no well-formed v1 tile here to show it helping.
                        if named.version < 2 {
                            rings = fill::fixup_polygons(&rings);
                        }
                        if !rings.is_empty() {
                            per_feature.push(rings);
                            kept.push(feature);
                        }
                    }
                }
                let borrowed: Vec<&[Ring]> = per_feature.iter().map(Vec::as_slice).collect();
                let (content, ends) = build_fill_content(layer, &paint, &borrowed, fill_step);
                let borrowed_features: Vec<&dyn tessella_style::expression::Feature> = kept
                    .iter()
                    .map(|feature| feature as &dyn tessella_style::expression::Feature)
                    .collect();
                pattern_vertices = resolve_pattern_vertices(
                    patterns,
                    layer,
                    bucket_zoom,
                    &borrowed_features,
                    &ends,
                );
                for (feature, end) in kept.iter().zip(&ends) {
                    binder
                        .push(*end, &paint, feature)
                        .map_err(|source| TileError::Binder {
                            layer: layer.id.clone(),
                            source,
                        })?;
                }
                content
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
                        if !filter.matches_on(
                            &feature,
                            Some(bucket_zoom),
                            Some((tile.z, tile.x, tile.y)),
                        ) {
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
            // A raster layer over a *feature* source draws nothing, and that is not a silent
            // skip: a raster layer's picture is its source's tile, so pointing one at a vector
            // or GeoJSON source names a source that has no picture to give. `build_raster_tile`
            // is where a raster layer is built, from a decoded image rather than from features.
            LayerKind::Raster => continue,
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

                let mut layout = SymbolLayout::new(layer, bucket_zoom, tile.overscale_factor());
                if let Some(named) = named {
                    for feature in named.features() {
                        if !filter.matches_on(
                            &feature,
                            Some(bucket_zoom),
                            Some((tile.z, tile.x, tile.y)),
                        ) {
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

                        let paint_values = binder.evaluate(&paint, &feature).map_err(|source| {
                            TileError::Binder {
                                layer: layer.id.clone(),
                                source,
                            }
                        })?;
                        layout.push(layer, bucket_zoom, &feature, &rings, paint_values);
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
                        if !filter.matches_on(
                            &feature,
                            Some(bucket_zoom),
                            Some((tile.z, tile.x, tile.y)),
                        ) {
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
            // The same features a circle takes, by the same rules -- including not checking the
            // geometry type, which is mbgl's: a bucket takes whatever the feature has and a
            // line's vertices each get a kernel.
            LayerKind::Heatmap => {
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

                let mut bucket = HeatmapBucket::default();
                if let Some(named) = named {
                    for feature in named.features() {
                        if !filter.matches_on(
                            &feature,
                            Some(bucket_zoom),
                            Some((tile.z, tile.x, tile.y)),
                        ) {
                            continue;
                        }
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
                Content::Heatmap(bucket)
            }
            // Every built type has an arm above. Spelled out rather than left to a wildcard:
            // a wildcard here is what let a layer type be enabled in `is_built` and silently
            // draw nothing from a vector tile, which is the quietest kind of gap.
            //
            // It is also the gap that actually happened. `Heatmap` sat in *this* list while
            // `is_built` named it, so a heatmap over a GeoJSON source built kernels and one over
            // a vector tile built none -- and no compiler could say so, because the arm is
            // explicit by design. A type added to `is_built` has to be added here in the same
            // change, and nothing but this comment enforces it.
            LayerKind::Hillshade | LayerKind::Custom | LayerKind::Other(_) => {
                continue;
            }
        };

        buckets.push(LayerBucket {
            layer_index,
            layer_id: layer.id.clone(),
            content,
            paint,
            binder,
            outline_under_fill: crate::ubo::fill_outline_under_fill(layer),
            pattern_vertices,
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
    pub fn as_raster(&self) -> Option<&RasterContent> {
        match self {
            Self::Raster(content) => Some(content),
            Self::Background
            | Self::Fill(_)
            | Self::Fill3d(_)
            | Self::Line(_)
            | Self::Circle(_)
            | Self::Heatmap(_)
            | Self::Hillshade(_)
            | Self::ColorRelief(_)
            | Self::LocationIndicator(_)
            | Self::Terrain(_)
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
            | Self::Heatmap(_)
            | Self::Fill3d(_) => None,
            Self::Raster(_)
            | Self::Hillshade(_)
            | Self::ColorRelief(_)
            | Self::LocationIndicator(_)
            | Self::Terrain(_) => None,
        }
    }

    /// The hillshade's slope field and quad, if this is one.
    #[must_use]
    pub fn as_hillshade(&self) -> Option<&HillshadeContent> {
        match self {
            Self::Hillshade(content) => Some(content),
            Self::Background
            | Self::Fill(_)
            | Self::Fill3d(_)
            | Self::Line(_)
            | Self::Circle(_)
            | Self::Heatmap(_)
            | Self::Raster(_)
            | Self::ColorRelief(_)
            | Self::LocationIndicator(_)
            | Self::Terrain(_)
            | Self::Symbol(_) => None,
        }
    }

    /// The color relief's elevation and quad, if this is one.
    #[must_use]
    pub fn as_color_relief(&self) -> Option<&ColorReliefContent> {
        match self {
            Self::ColorRelief(content) => Some(content),
            Self::Background
            | Self::Fill(_)
            | Self::Fill3d(_)
            | Self::Line(_)
            | Self::Circle(_)
            | Self::Heatmap(_)
            | Self::Hillshade(_)
            | Self::Raster(_)
            | Self::LocationIndicator(_)
            | Self::Terrain(_)
            | Self::Symbol(_) => None,
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
            | Self::Heatmap(_)
            | Self::Symbol(_)
            | Self::Fill3d(_) => None,
            Self::Raster(_)
            | Self::Hillshade(_)
            | Self::ColorRelief(_)
            | Self::LocationIndicator(_)
            | Self::Terrain(_) => None,
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
            | Self::Heatmap(_)
            | Self::Symbol(_)
            | Self::Fill3d(_) => None,
            Self::Raster(_)
            | Self::Hillshade(_)
            | Self::ColorRelief(_)
            | Self::LocationIndicator(_)
            | Self::Terrain(_) => None,
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
            | Self::Heatmap(_)
            | Self::Symbol(_)
            | Self::Fill3d(_) => None,
            Self::Raster(_)
            | Self::Hillshade(_)
            | Self::ColorRelief(_)
            | Self::LocationIndicator(_)
            | Self::Terrain(_) => None,
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
            Self::Heatmap(bucket) => !bucket.segments.is_empty(),
            // Labels, not vertices: a symbol layer has data when it resolved text, whether or
            // not the glyphs to shape it with have arrived.
            Self::Symbol(layout) => !layout.is_empty(),
            Self::Raster(content) => !content.bucket.is_empty(),
            Self::Hillshade(content) => !content.bucket.is_empty(),
            Self::ColorRelief(content) => !content.bucket.is_empty(),
            Self::LocationIndicator(bucket) => !bucket.is_empty(),
            // Always: the mesh is the same for every tile and the DEM is why this exists. A
            // terrain tile with nothing to say is a tile that was never built.
            Self::Terrain(_) => true,
            Self::Fill3d(bucket) => !bucket.segments.is_empty(),
        }
    }

    /// Whether this bucket can be turned into records with the resources a frame holds.
    ///
    /// [`Self::has_data`] asks whether the bucket has content at all. This asks whether it can be
    /// *encoded now*, and a symbol layer is where the two part: it has data as soon as it
    /// resolved text, and cannot be encoded until the glyphs to shape that text have arrived.
    ///
    /// # Why the difference has to be visible here
    ///
    /// Because binding is what spends a drawable's one chance to be announced. An id is
    /// allocated in the binding pass and the bucket is *fresh* only the frame its id is new;
    /// every frame after, the incremental path skips it as known. So a bucket bound before it
    /// could be encoded is bound to silence: it announces nothing, is marked known, and is never
    /// looked at again — the labels arrive and no frame ever carries them.
    ///
    /// That is the normal case rather than a race. Which glyphs a style needs is discovered by
    /// evaluating `text-field` against a tile's own features, so the fetch cannot precede the
    /// first tile build.
    ///
    /// Patterns are deliberately not here. A pattern layer without its sprites draws as a plain
    /// fill, which is a frame the consumer can use; a symbol layer without its glyphs draws
    /// nothing at all.
    #[must_use]
    pub fn is_encodable(&self, fonts: bool) -> bool {
        // A symbol layer waits for glyphs only if it has text to set with them.
        //
        // `matches!(self, Self::Symbol(_))` held back every symbol layer, and a layer whose
        // symbols are all icons asks for no glyphs at all -- `dependencies` skips a pending
        // symbol with no fonts or no text -- so nothing ever fetched any and the layer never
        // became encodable. An `icon-image` layer with no `text-field` drew nothing, which is an
        // ordinary way to write a marker or a shield.
        self.has_data()
            && (fonts
                || match self {
                    Self::Symbol(layout) => !layout.has_text(),
                    _ => true,
                })
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
    /// The surface every key this builder makes names.
    ///
    /// A field rather than an argument on `key`: it is a property of the map, one map has one
    /// projection, and threading it through every call site would be three arguments saying the
    /// same thing. In the key rather than beside it because a plane's buckets and a sphere's are
    /// not interchangeable -- the plane's are byte-exact against mbgl and a split one is not.
    surface: Surface,
}

impl TileBuilder {
    /// A builder over a store of `capacity` tiles.
    #[must_use]
    pub fn new(capacity: usize, style_rev: u64) -> Self {
        Self {
            store: TileStore::new(capacity),
            style_rev,
            builds: 0,
            surface: Surface::Plane,
        }
    }

    /// Sets the surface this builder's tiles are built for.
    ///
    /// Keys change with it, so the tiles already built stay in the store under the old surface
    /// rather than being wrong under the new one: a projection switch rebuilds rather than
    /// reinterprets, and the old entries age out of the LRU as any superseded tile does.
    pub const fn build_for(&mut self, surface: Surface) {
        self.surface = surface;
    }

    /// The surface this builder's tiles are built for.
    #[must_use]
    pub const fn surface(&self) -> Surface {
        self.surface
    }

    /// The key a tile occupies in the store.
    ///
    /// Carries the tile's *used* zoom as well as its own. A bucket is only shareable between
    /// views that would build it identically, and a zoom-varying paint property is stored as
    /// its value at `overscaled_z` and `overscaled_z + 1` — so the same canonical tile standing
    /// in at two different zooms is two different buckets. Keying on `(z, x, y)` alone hands
    /// one view the other's endpoints: wrong colors and widths, and invisible at integer zoom,
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
        .on(self.surface)
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
