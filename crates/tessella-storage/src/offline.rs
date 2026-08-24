//! Offline regions: an area a user picked, and everything needed to draw it without a network.
//!
//! # What a region is
//!
//! A style URL, an area, a zoom range and a pixel ratio. From those, every resource the map
//! would ever ask for inside that area can be enumerated: the style itself, each source's
//! manifest, every tile of every source within the area and zooms, the glyph ranges each font
//! stack needs, and the sprite sheets.
//!
//! # Why the count is separate from the download
//!
//! A user picks an area and is told what it will cost before it starts. At zoom 16 over a
//! country that is millions of tiles and gigabytes, and the answer they want is "no" — which
//! they can only give if asking is cheaper than doing. So [`Region::tile_count`] closes a
//! formula and never allocates the list.
//!
//! # Why the estimate is not exact at first
//!
//! A source given by URL states its zoom range in a manifest that has not been fetched yet, so
//! the tiles it contributes cannot be counted until it has. mbgl reports this as
//! `requiredResourceCountIsPrecise`, and it matters to a progress bar: a total that grows as
//! the download proceeds is confusing, and one that claims precision it does not have is worse.
//! [`Estimate::precise`] says which it is.

use tessella_tile::cover::{Bounds, CoverError, TileCoord};

use crate::url::ZoomRange;

/// How a source's zooms are quantised into tile zooms.
///
/// A vector tile is 512 units and floors; a raster tile may be 256 and rounds. The difference
/// is mbgl's `coveringZoomLevel`, and it is not cosmetic: a 256-pixel raster source needs one
/// zoom level *more* than a vector source to fill the same screen, and rounding rather than
/// flooring is what stops it being consistently one level too coarse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// Vector tiles, always 512 units.
    Vector,
    /// Raster tiles of the given pixel size.
    Raster {
        /// Tile side in pixels, typically 256 or 512.
        tile_size: u16,
    },
}

impl SourceKind {
    /// The tile zoom that serves `zoom` for this source.
    #[must_use]
    pub fn covering_zoom(self, zoom: f64) -> f64 {
        match self {
            Self::Vector => zoom.floor(),
            Self::Raster { tile_size } => {
                let adjusted = zoom + (512.0 / f64::from(tile_size.max(1))).log2();
                adjusted.round()
            }
        }
    }
}

/// An area a user asked to have available offline.
#[derive(Debug, Clone, PartialEq)]
pub struct Region {
    /// The style to make available.
    pub style_url: String,
    /// The area.
    pub bounds: Bounds,
    /// Lowest zoom to include.
    pub min_zoom: f64,
    /// Highest zoom to include.
    pub max_zoom: f64,
    /// Device pixel ratio, which selects between `@2x` and plain assets.
    pub pixel_ratio: f32,
    /// Whether to download CJK glyph ranges.
    ///
    /// They are the bulk of a glyph download and are usually rendered locally, which is why
    /// mbgl makes this a choice rather than always fetching them.
    pub include_ideographs: bool,
}

/// The zooms a region and a source have in common, as tile zooms.
///
/// The intersection, not the region's own range: asking a source that stops at zoom 6 for zoom
/// 14 tiles produces a download of 404s.
#[must_use]
pub fn covering_zoom_range(region: &Region, kind: SourceKind, source: ZoomRange) -> (u8, u8) {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let quantise = |zoom: f64| kind.covering_zoom(zoom).clamp(0.0, 30.0) as u8;
    (
        quantise(region.min_zoom).max(source.min),
        quantise(region.max_zoom).min(source.max),
    )
}

impl Region {
    /// How many tiles one source contributes, without enumerating them.
    #[must_use]
    pub fn tile_count(&self, kind: SourceKind, source: ZoomRange) -> u64 {
        let (min, max) = covering_zoom_range(self, kind, source);
        if min > max {
            // The region and the source do not overlap in zoom. Not an error: a style may carry
            // a source that simply has nothing to offer at the zooms asked for.
            return 0;
        }
        (min..=max)
            .map(|z| self.bounds.tile_count(z))
            .fold(0u64, u64::saturating_add)
    }

    /// Every tile one source contributes.
    ///
    /// # Errors
    ///
    /// [`CoverError::TooLarge`] when a single zoom level exceeds `limit`. Checked per level
    /// rather than in total, so the error names the level that is unreasonable rather than the
    /// sum of every reasonable one.
    pub fn tiles(
        &self,
        kind: SourceKind,
        source: ZoomRange,
        limit: u64,
    ) -> Result<Vec<TileCoord>, CoverError> {
        let (min, max) = covering_zoom_range(self, kind, source);
        if min > max {
            return Ok(Vec::new());
        }
        let mut tiles = Vec::new();
        for z in min..=max {
            tiles.extend(self.bounds.tiles(z, limit)?);
        }
        Ok(tiles)
    }
}

/// What a source contributes to a region, once its manifest is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceContribution {
    /// A tiled source whose zoom range is known.
    Tiles {
        /// What sort of tiles.
        kind: SourceKind,
        /// The zooms it provides.
        zooms: ZoomRange,
        /// Whether its manifest had to be fetched to learn that.
        ///
        /// A source given inline states its range in the style; one given by URL costs a
        /// resource of its own to find out.
        from_manifest: bool,
    },
    /// A single document — a GeoJSON source by URL, or an image source.
    Document,
    /// A tiled source whose manifest has not been fetched, so its tiles cannot be counted.
    Unknown,
}

/// How many glyph ranges a font stack needs.
///
/// mbgl's constants. The whole Unicode plane is 256 ranges of 256 code points; without the
/// ideographs it is the first few, which is the difference between a glyph download of
/// megabytes and one of kilobytes.
pub const GLYPH_RANGES_PER_FONT_STACK: u64 = 256;
/// As [`GLYPH_RANGES_PER_FONT_STACK`], for a region that excludes CJK.
pub const NON_IDEOGRAPH_GLYPH_RANGES_PER_FONT_STACK: u64 = 5;
/// Files per sprite: JSON and PNG, at one and two times.
pub const RESOURCES_PER_SPRITE: u64 = 4;

/// What a region will cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Estimate {
    /// Tiles, across every source.
    pub tiles: u64,
    /// Every resource, tiles included.
    pub resources: u64,
    /// Whether [`Self::resources`] is exact or a lower bound.
    pub precise: bool,
}

/// What a style needs downloading, beside its sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StyleAssets {
    /// Font stacks the style names.
    pub font_stacks: u64,
    /// Custom font faces, which are fetched whole rather than by range.
    pub font_faces: u64,
    /// Sprite sheets.
    pub sprites: u64,
    /// Whether the style names a glyph URL at all.
    pub has_glyphs: bool,
}

/// Sizes a region.
///
/// `sources` is what each of the style's sources contributes; `assets` is what the style itself
/// needs. Both come from parsing the style, which is itself the first resource a download
/// fetches — so an estimate before that has nothing to go on and says so.
#[must_use]
pub fn estimate(region: &Region, sources: &[SourceContribution], assets: StyleAssets) -> Estimate {
    // The style document itself.
    let mut resources = 1u64;
    let mut tiles = 0u64;
    let mut precise = true;

    for source in sources {
        match source {
            SourceContribution::Tiles {
                kind,
                zooms,
                from_manifest,
            } => {
                if *from_manifest {
                    resources = resources.saturating_add(1);
                }
                let count = region.tile_count(*kind, *zooms);
                tiles = tiles.saturating_add(count);
                resources = resources.saturating_add(count);
            }
            SourceContribution::Document => resources = resources.saturating_add(1),
            SourceContribution::Unknown => {
                // Its manifest costs a resource whether or not its tiles can be counted, and
                // the tiles are the part that is not yet knowable.
                resources = resources.saturating_add(1);
                precise = false;
            }
        }
    }

    if assets.has_glyphs {
        let per_stack = if region.include_ideographs {
            GLYPH_RANGES_PER_FONT_STACK
        } else {
            NON_IDEOGRAPH_GLYPH_RANGES_PER_FONT_STACK
        };
        resources = resources.saturating_add(
            assets
                .font_stacks
                .saturating_mul(per_stack + assets.font_faces),
        );
    }
    resources = resources.saturating_add(assets.sprites.saturating_mul(RESOURCES_PER_SPRITE));

    Estimate {
        tiles,
        resources,
        precise,
    }
}
