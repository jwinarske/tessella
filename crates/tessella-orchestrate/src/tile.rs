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
//! their own index list, which is line-list rather than triangle-list and belongs with the line
//! work.
//!
//! # Layer index is the style's order
//!
//! The layer index the stream carries is the layer's position in the style document, not a
//! count of layers that produced geometry. A layer that draws nothing still occupies its index,
//! because the index is what painter order is expressed in and what a consumer keys uniforms
//! by. Skipping unimplemented layers while renumbering the rest would silently restack the map.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tessella_layout::fill::{self, FillBucket, Ring};
use tessella_source::clip::{clip_ring_to_box, round_to_tile_units};
use tessella_source::geojson::{GeoJsonFeature, Geometry};
use tessella_source::tiling::TilingOptions;
use tessella_style::property::{ResolvedProperty, resolve_paint};
use tessella_style::{Filter, LayerKind, Style};
use tessella_tile::projection;

/// The tile being built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileId {
    /// Zoom.
    pub z: u8,
    /// Column.
    pub x: u32,
    /// Row.
    pub y: u32,
}

/// What a layer contributed to a tile.
#[derive(Debug, Clone, PartialEq)]
pub enum Content {
    /// A full-viewport background. Draws from no source, so it has no features and its
    /// geometry is a quad the consumer can synthesize.
    Background,
    /// Triangles, and a count of the outline drawable that accompanies them.
    Fill(FillBucket),
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
}

impl LayerBucket {
    /// How many drawables this becomes on the stream.
    ///
    /// A fill is two — triangles and outline — and a background is one.
    #[must_use]
    pub fn drawable_count(&self) -> usize {
        match self.content {
            Content::Background => 1,
            Content::Fill(_) => 2,
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
    tile: TileId,
    features: &[GeoJsonFeature],
    options: TilingOptions,
) -> Result<Vec<LayerBucket>, TileError> {
    let (lo, hi) = options.clip_range();
    let (lo, hi) = (f64::from(lo), f64::from(hi));
    let mut buckets = Vec::new();

    for (layer_index, layer) in style.layers.iter().enumerate() {
        if !layer.kind.is_r0() {
            continue;
        }

        let paint = resolve_paint(layer).map_err(|source| TileError::Property {
            layer: layer.id.clone(),
            source,
        })?;

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

                let mut rings: Vec<Ring> = Vec::new();
                for feature in features {
                    if !filter.matches(feature, None) {
                        continue;
                    }
                    let Geometry::Polygon(polygons) = &feature.geometry else {
                        // A fill layer's filter usually excludes non-polygons, but a style is
                        // free to write one that does not. mbgl draws nothing for those rather
                        // than treating a line as a degenerate ring.
                        continue;
                    };
                    for polygon in polygons {
                        for ring in polygon {
                            let projected: Vec<[f64; 2]> = ring
                                .iter()
                                .map(|p| projection::tile_local(p[0], p[1], tile.z, tile.x, tile.y))
                                .collect();
                            let clipped = clip_ring_to_box(&projected, lo, hi);
                            if clipped.is_empty() {
                                continue;
                            }
                            rings.push(to_tile_ring(&clipped));
                        }
                    }
                }
                Content::Fill(fill::build(&rings))
            }
            // `is_r0` gates this, so anything else is unreachable rather than merely unhandled.
            _ => continue,
        };

        buckets.push(LayerBucket {
            layer_index,
            layer_id: layer.id.clone(),
            content,
            paint,
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
    /// The fill bucket, if this is one.
    #[must_use]
    pub fn as_fill(&self) -> Option<&FillBucket> {
        match self {
            Self::Fill(bucket) => Some(bucket),
            Self::Background => None,
        }
    }
}

impl TileId {
    /// A tile at a zoom.
    #[must_use]
    pub const fn new(z: u8, x: u32, y: u32) -> Self {
        Self { z, x, y }
    }
}

impl core::fmt::Display for TileId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}/{}/{}", self.z, self.x, self.y)
    }
}

impl TileError {
    /// The layer the error came from.
    #[must_use]
    pub fn layer(&self) -> &str {
        match self {
            Self::Filter { layer, .. } | Self::Property { layer, .. } => layer,
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
