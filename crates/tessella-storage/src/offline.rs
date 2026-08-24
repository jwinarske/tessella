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
use tessella_tile::polygon::{Cover, Polygon};

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

/// The area a region covers.
///
/// A box is the cheap case and answers "how many tiles" with a closed formula. A shape is what
/// a user actually draws, and covering a coastal city by its bounding box downloads the sea —
/// at street zoom that is most of the tiles and all of the waiting.
#[derive(Debug, Clone, PartialEq)]
pub enum Area {
    /// A rectangle.
    Box(Bounds),
    /// A shape in one or more parts, each with optional holes.
    Shape(Vec<Polygon>),
}

impl Area {
    /// The box that contains this area.
    ///
    /// For a shape this is what it is stored as and what a list of regions shows on a map; the
    /// shape itself is what decides which tiles are fetched.
    #[must_use]
    pub fn bounds(&self) -> Bounds {
        match self {
            Self::Box(bounds) => *bounds,
            Self::Shape(parts) => {
                let mut bounds: Option<Bounds> = None;
                for point in parts
                    .iter()
                    .flat_map(|part| core::iter::once(&part.exterior).chain(&part.interiors))
                    .flatten()
                {
                    bounds = Some(match bounds {
                        None => Bounds::new(point[0], point[1], point[0], point[1]),
                        Some(held) => Bounds::new(
                            held.west.min(point[0]),
                            held.south.min(point[1]),
                            held.east.max(point[0]),
                            held.north.max(point[1]),
                        ),
                    });
                }
                bounds.unwrap_or_else(|| Bounds::new(0.0, 0.0, 0.0, 0.0))
            }
        }
    }

    /// How many tiles this area covers at `z`.
    ///
    /// A box answers by formula and never allocates. A shape has to be scanned — there is no
    /// closed form for an arbitrary outline — but the scan is an iterator, so counting a
    /// continent at street zoom costs time rather than memory, and the caller can still decline.
    #[must_use]
    pub fn tile_count(&self, z: u8) -> u64 {
        match self {
            Self::Box(bounds) => bounds.tile_count(z),
            Self::Shape(parts) => Cover::shape(parts, z).count() as u64,
        }
    }

    /// Every tile this area covers at `z`.
    ///
    /// # Errors
    ///
    /// [`CoverError::TooLarge`] when the level exceeds `limit`.
    pub fn tiles(&self, z: u8, limit: u64) -> Result<Vec<TileCoord>, CoverError> {
        match self {
            Self::Box(bounds) => bounds.tiles(z, limit),
            Self::Shape(parts) => {
                let mut tiles = Vec::new();
                for tile in Cover::shape(parts, z) {
                    if tiles.len() as u64 >= limit {
                        return Err(CoverError::TooLarge {
                            tiles: tiles.len() as u64 + 1,
                        });
                    }
                    tiles.push(tile);
                }
                // The scan can name an edge tile twice where a projection overshot the world.
                tiles.sort_unstable();
                tiles.dedup();
                Ok(tiles)
            }
        }
    }
}

impl Area {
    /// The area as GeoJSON MultiPolygon coordinates, or `None` for a box.
    ///
    /// A box needs no geometry: its four numbers are already columns, and writing them twice
    /// invites the two copies to disagree. A shape is stored in the form a client would have
    /// sent it in, which is also the form anyone debugging the database can read.
    #[must_use]
    pub fn geometry(&self) -> Option<serde_json::Value> {
        match self {
            Self::Box(_) => None,
            Self::Shape(parts) => Some(serde_json::Value::Array(
                parts
                    .iter()
                    .map(|part| {
                        serde_json::Value::Array(
                            core::iter::once(&part.exterior)
                                .chain(&part.interiors)
                                .map(|ring| {
                                    serde_json::Value::Array(
                                        ring.iter()
                                            .map(|point| serde_json::json!([point[0], point[1]]))
                                            .collect(),
                                    )
                                })
                                .collect(),
                        )
                    })
                    .collect(),
            )),
        }
    }

    /// Reads back what [`Self::geometry`] wrote.
    ///
    /// # Errors
    ///
    /// [`AreaError`] when the value is not MultiPolygon coordinates. A stored region that no
    /// longer parses is reported rather than silently downgraded to its bounding box — that
    /// would turn a city into the sea around it without saying so.
    pub fn from_geometry(value: &serde_json::Value) -> Result<Self, AreaError> {
        let parts = value.as_array().ok_or(AreaError::NotMultiPolygon)?;
        let mut shape = Vec::with_capacity(parts.len());
        for part in parts {
            let rings = part.as_array().ok_or(AreaError::NotMultiPolygon)?;
            let mut read = Vec::with_capacity(rings.len());
            for ring in rings {
                let points = ring.as_array().ok_or(AreaError::NotMultiPolygon)?;
                let mut out = Vec::with_capacity(points.len());
                for point in points {
                    let pair = point.as_array().ok_or(AreaError::NotMultiPolygon)?;
                    let longitude = pair
                        .first()
                        .and_then(serde_json::Value::as_f64)
                        .ok_or(AreaError::NotMultiPolygon)?;
                    let latitude = pair
                        .get(1)
                        .and_then(serde_json::Value::as_f64)
                        .ok_or(AreaError::NotMultiPolygon)?;
                    out.push([longitude, latitude]);
                }
                read.push(out);
            }
            let mut rings = read.into_iter();
            let exterior = rings.next().ok_or(AreaError::NoExterior)?;
            shape.push(Polygon {
                exterior,
                interiors: rings.collect(),
            });
        }
        if shape.is_empty() {
            return Err(AreaError::NoExterior);
        }
        Ok(Self::Shape(shape))
    }
}

/// Why a stored area could not be read back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AreaError {
    /// The value is not an array of parts of rings of points.
    #[error("stored geometry is not MultiPolygon coordinates")]
    NotMultiPolygon,
    /// A part had no rings at all.
    #[error("stored geometry has a part with no exterior ring")]
    NoExterior,
}

/// An area a user asked to have available offline.
#[derive(Debug, Clone, PartialEq)]
pub struct Region {
    /// The style to make available.
    pub style_url: String,
    /// The area.
    pub area: Area,
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
            .map(|z| self.area.tile_count(z))
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
            tiles.extend(self.area.tiles(z, limit)?);
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

/// Every glyph range in the plane, as `[start, end]` pairs.
///
/// 256 ranges of 256 code points, which is what the `{range}` token in a glyph URL selects.
const GLYPH_RANGE_STRIDE: u32 = 256;

/// The ranges a region asks for, given whether it wants ideographs.
fn glyph_ranges(include_ideographs: bool) -> impl Iterator<Item = (u32, u32)> {
    let count = if include_ideographs {
        GLYPH_RANGES_PER_FONT_STACK
    } else {
        NON_IDEOGRAPH_GLYPH_RANGES_PER_FONT_STACK
    };
    #[allow(clippy::cast_possible_truncation)]
    (0..count as u32).map(|index| {
        let start = index * GLYPH_RANGE_STRIDE;
        (start, start + GLYPH_RANGE_STRIDE - 1)
    })
}

/// What a download will fetch, resolved down to URLs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Manifests, glyphs, sprites and single documents.
    ///
    /// Separate from the tiles because they are the small, shared, always-needed part: a
    /// download fetches them first so that a cancelled or interrupted region still has a style
    /// that renders whatever tiles did arrive.
    pub assets: Vec<String>,
    /// Every tile, across every source.
    pub tiles: Vec<String>,
    /// Whether this is everything.
    ///
    /// False when a source's manifest has not been resolved, or when a `text-font` is
    /// data-driven and so names fonts that only the features themselves reveal.
    pub complete: bool,
}

impl Plan {
    /// How many resources this plan names.
    #[must_use]
    pub fn len(&self) -> usize {
        self.assets.len() + self.tiles.len()
    }

    /// True when the plan names nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Collects the font stacks a style names, and says whether it found all of them.
///
/// A `text-font` may be data-driven — `["get", "font"]`, or a `match` on a feature property —
/// in which case the fonts in play are a property of the data rather than of the style, and no
/// amount of reading the style reveals them. mbgl handles this by asking the expression for its
/// possible outputs, which answers for a `match` and not for a `get`. Here the constant forms
/// are collected and anything else sets the flag, so a download reports itself as a lower bound
/// rather than quietly shipping a region whose labels have no glyphs.
#[must_use]
pub fn font_stacks(style: &tessella_style::Style) -> (Vec<String>, bool) {
    let mut stacks: Vec<String> = Vec::new();
    let mut complete = true;

    for layer in &style.layers {
        let Some(value) = layer.layout.get("text-font") else {
            continue;
        };
        let fonts = match value {
            // A plain stack. `["Noto Sans Regular"]` reaches this arm rather than the other
            // one because [`tessella_style::Value::looks_like_expression`] checks the operator
            // registry, not merely the shape.
            tessella_style::PropertyValue::Literal(literal) => literal.as_array(),
            // `["literal", [...]]` is the one call whose fonts are stated rather than computed.
            tessella_style::PropertyValue::Expression(expression) => expression
                .value()
                .as_array()
                .filter(|call: &&[tessella_style::Value]| {
                    call.first().and_then(tessella_style::Value::as_str) == Some("literal")
                })
                .and_then(|call| call.get(1))
                .and_then(tessella_style::Value::as_array),
        };
        match fonts {
            Some(fonts) => {
                // The stack is the comma-joined list, which is what the `{fontstack}` token in
                // a glyph URL expects.
                let stack = fonts
                    .iter()
                    .filter_map(tessella_style::Value::as_str)
                    .collect::<Vec<_>>()
                    .join(",");
                if !stack.is_empty() && !stacks.contains(&stack) {
                    stacks.push(stack);
                }
            }
            None => complete = false,
        }
    }

    stacks.sort_unstable();
    (stacks, complete)
}

/// Turns a style and a region into the list of URLs a download must fetch.
///
/// `manifests` supplies the [`crate::tileset::TileSet`] for each source id whose manifest has
/// already been resolved. A tiled source missing from it contributes no tiles and makes the
/// plan incomplete — which is why a download resolves manifests first and plans afterwards.
///
/// The style URL itself is not included: the caller already has it, and having fetched it is
/// what made this call possible.
#[must_use]
pub fn plan(
    style: &tessella_style::Style,
    region: &Region,
    manifests: &std::collections::BTreeMap<String, crate::tileset::TileSet>,
) -> Plan {
    use tessella_style::Source;

    let mut plan = Plan {
        complete: true,
        ..Plan::default()
    };

    for (id, source) in &style.sources {
        let (kind, manifest_url) = match source {
            Source::Vector(tiles) => (SourceKind::Vector, tiles.url.as_deref()),
            Source::Raster(tiles) | Source::RasterDem(tiles) => (
                SourceKind::Raster {
                    #[allow(clippy::cast_possible_truncation)]
                    tile_size: tiles.tile_size.unwrap_or(512) as u16,
                },
                tiles.url.as_deref(),
            ),
            Source::Geojson(geojson) => {
                // A GeoJSON source by URL is one document; one given inline is already in the
                // style and costs nothing more.
                match crate::geojson::origin(geojson) {
                    Ok(crate::geojson::Origin::Url(url)) => plan.assets.push(url.to_string()),
                    Ok(crate::geojson::Origin::Inline) => {}
                    // A source whose `data` is neither is a style this cannot enumerate. It is
                    // not fatal to the region -- the other sources still download -- but the
                    // plan is no longer everything.
                    Err(_) => plan.complete = false,
                }
                continue;
            }
            // An image or video source, or something this build does not model. Its resources
            // cannot be named, so saying the plan is complete would be a lie.
            Source::Other(_) => {
                plan.complete = false;
                continue;
            }
        };

        if let Some(url) = manifest_url {
            plan.assets.push(url.to_string());
        }

        let Some(manifest) = manifests.get(id) else {
            plan.complete = false;
            continue;
        };

        let (min, max) = covering_zoom_range(region, kind, manifest.zooms);
        if min > max {
            continue;
        }
        for z in min..=max {
            // The limit is the count itself: a caller that wants a bound checks
            // [`Region::tile_count`] first and declines, rather than being surprised here.
            let Ok(coords) = region.area.tiles(z, u64::MAX) else {
                plan.complete = false;
                continue;
            };
            for coord in coords {
                // One template per tile. A source sharded across hosts states several so a
                // browser can open more connections; fetching the same tile from each of them
                // would download it two or three times over.
                let template = &manifest.templates
                    [(coord.x as usize + coord.y as usize) % manifest.templates.len().max(1)];
                plan.tiles.push(crate::url::expand(
                    template,
                    z,
                    coord.x,
                    coord.y,
                    manifest.scheme,
                    region.pixel_ratio,
                ));
            }
        }
    }

    if let Some(template) = &style.glyphs {
        let (stacks, all_found) = font_stacks(style);
        plan.complete &= all_found;
        for stack in stacks {
            for (start, end) in glyph_ranges(region.include_ideographs) {
                plan.assets.push(
                    template
                        // Font stacks have spaces in them, and a raw space is not a legal URI
                        // character: unencoded, the request fails at the transport rather than
                        // as a 404, and no style with a label in it can be downloaded at all.
                        .replace("{fontstack}", &crate::url::percent_encode(&stack))
                        .replace("{range}", &format!("{start}-{end}")),
                );
            }
        }
    }

    if let Some(sprite) = &style.sprite {
        // Both densities of both files: a region downloaded on a phone may be viewed after the
        // display scale changes, and a missing `@2x` sheet is a map with no icons.
        for suffix in ["", "@2x"] {
            for extension in ["json", "png"] {
                plan.assets.push(format!("{sprite}{suffix}.{extension}"));
            }
        }
    }

    plan
}
