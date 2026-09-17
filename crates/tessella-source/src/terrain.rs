// SPDX-License-Identifier: BSD-2-Clause
//! Reading an elevation off a DEM the way a terrain does.
//!
//! Everything a terrain draws follows from one question -- how high is the ground at this point
//! in this tile -- and this is that question. The mesh, the drape and the displacement all call
//! it; none of them is here.
//!
//! # The reference is MapLibre GL JS, because there is no other
//!
//! maplibre-native has no terrain at the pinned revision or upstream: no `style/terrain.hpp`, no
//! `render_terrain*`, no `setTerrain`, and a style carrying `{"terrain": …}` renders identically
//! to one without it. So there is no oracle for the picture and plan.md §1 says as much. What
//! there *is* an oracle for is this: the arithmetic below is
//! `Terrain.getElevationSampler` and `DEMData.sampleBilinear` transcribed, and the elevation it
//! returns can be checked against a height field whose value is known in closed form.
//!
//! # Where the half pixel goes
//!
//! A DEM pixel is a *cell*, and pixel `i` describes the cell centered at `(i + 0.5) / dim` of the
//! tile. So a tile coordinate scaled into pixels lands half a cell past the pixel that owns it,
//! and [`DEM_CELL_CENTER_OFFSET`] takes it back. GL JS names the constant and says why in the
//! same words; it is spelled out here because it is the one number in the mapping that is not
//! obviously there, and dropping it shifts an entire terrain by half a cell -- a few meters of
//! ground at street zoom, which looks like nothing rather than like a bug.
//!
//! Hillshade and color relief place their pixels the same way, which is what makes a hillshade
//! drawn over a terrain line up with the surface under it.

use tessella_style::Terrain;

use crate::dem::Dem;
use crate::tiling::EXTENT;

/// Offset, in DEM pixels, from a tile coordinate scaled by `dim` to the pixel index
/// [`Dem::sample_bilinear`] expects.
///
/// GL JS's own constant and its own value. See the module note for what it is for.
pub const DEM_CELL_CENTER_OFFSET: f32 = -0.5;

/// Which DEM tile a tile reads, and where in it.
///
/// A tile is not always drawn over a DEM of its own zoom. A source has a maxzoom, and a deeper
/// tile reads an ancestor's DEM and takes the quarter, sixteenth or smaller of it that covers its
/// own ground -- which is what `dz` and the offsets are. `dz` of zero is the ordinary case and
/// makes the offsets zero with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DemCover {
    /// How many zooms below the DEM tile this tile is.
    pub dz: u8,
    /// Which column of the `2^dz` grid within the DEM tile.
    pub dx: u32,
    /// And which row.
    pub dy: u32,
}

impl DemCover {
    /// The DEM tile a tile at `z/x/y` reads, given the DEM's own zoom.
    ///
    /// `None` when the DEM is *deeper* than the tile: a tile cannot be drawn over a fraction of
    /// its own ground, and a caller in that position wants several DEM tiles rather than one.
    #[must_use]
    pub fn new(z: u8, x: u32, y: u32, dem_z: u8) -> Option<Self> {
        let dz = z.checked_sub(dem_z)?;
        // The tile's position within the DEM tile: its own address minus the address of the DEM
        // tile's top-left corner at this zoom, which is `(x >> dz) << dz`.
        Some(Self {
            dz,
            dx: x - ((x >> dz) << dz),
            dy: y - ((y >> dz) << dz),
        })
    }

    /// The DEM tile itself, as `z/x/y`.
    #[must_use]
    pub const fn dem_tile(self, z: u8, x: u32, y: u32) -> (u8, u32, u32) {
        (z - self.dz, x >> self.dz, y >> self.dz)
    }
}

/// How a tile's coordinates become DEM pixel positions.
///
/// GL JS builds a matrix for this -- `_getDEMTileMatrix`, a scale and a translate -- and then
/// reads four of its sixteen numbers back out. The four are what the mapping is, so they are what
/// this carries: the transform is diagonal and there is no third dimension to it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DemSampler {
    scale: f32,
    offset_x: f32,
    offset_y: f32,
}

impl DemSampler {
    /// The mapping for a tile drawn over `dem`, covering it as `cover` says.
    #[must_use]
    pub fn new(dem: &Dem, cover: DemCover) -> Self {
        #[allow(clippy::cast_precision_loss)]
        let dim = dem.dim() as f32;
        // `1 << dz` tiles across the DEM tile, so a tile's own extent is that fraction of it.
        #[allow(clippy::cast_precision_loss)]
        let tiles = (1u32 << cover.dz) as f32;
        #[allow(clippy::cast_precision_loss)]
        let scale = dim / (EXTENT as f32 * tiles);
        #[allow(clippy::cast_precision_loss)]
        Self {
            scale,
            offset_x: cover.dx as f32 * dim / tiles + DEM_CELL_CENTER_OFFSET,
            offset_y: cover.dy as f32 * dim / tiles + DEM_CELL_CENTER_OFFSET,
        }
    }

    /// The DEM pixel position a tile coordinate lands on.
    ///
    /// `extent` is the coordinate's own, which is [`EXTENT`] for anything this build produces.
    /// GL JS carries it because its symbol placement asks in other extents; it is here for the
    /// same reason and costs a multiply by one in the common case.
    #[must_use]
    pub fn pixel(self, x: f32, y: f32, extent: i32) -> [f32; 2] {
        #[allow(clippy::cast_precision_loss)]
        let rescale = if extent == EXTENT {
            1.0
        } else {
            EXTENT as f32 / extent as f32
        };
        [
            x * rescale * self.scale + self.offset_x,
            y * rescale * self.scale + self.offset_y,
        ]
    }
}

/// The elevation at a tile coordinate, in meters, with the style's exaggeration applied.
///
/// GL JS's `getElevation`, which is `getDEMElevation` times the exaggeration and nothing else.
/// The two are separate there and separate here: a hillshade wants the ground's own height and a
/// terrain wants the stretched one, and multiplying twice is a cliff where a hill should be.
///
/// `None` when the sample falls outside the DEM and its border, which is a caller asking the
/// wrong tile.
#[must_use]
pub fn elevation(
    dem: &Dem,
    sampler: DemSampler,
    terrain: &Terrain,
    x: f32,
    y: f32,
    extent: i32,
) -> Option<f32> {
    let [px, py] = sampler.pixel(x, y, extent);
    #[allow(clippy::cast_possible_truncation)]
    let exaggeration = terrain.exaggeration() as f32;
    dem.sample_bilinear(px, py)
        .map(|meters| meters * exaggeration)
}

/// The elevation range over any region of a tile, in meters, answered in constant time.
///
/// # Why a pyramid and not a walk
///
/// Subdivision asks this once per candidate triangle, recursively. Walking the DEM pixels under a
/// box costs the box's area, and the recursion's boxes sum to the whole tile at every depth -- a
/// 256-pixel tile subdivided eight levels deep reads half a million pixels per layer per tile,
/// which at a twenty-tile cover is tens of millions of reads in a tile build. A pyramid costs a
/// third again over the base level once, and answers every query with four reads.
///
/// # Why the base is the mesh's cell and not the DEM's pixel
///
/// Nothing ever needs finer. The drawn surface is piecewise linear over the terrain mesh's grid,
/// so a triangle inside one mesh cell already agrees with the surface; asking about a region
/// smaller than a cell is asking about detail that is not drawn. Starting at the cell rather than
/// the pixel is a quarter of the memory for a 256-pixel DEM and a sixteenth for a 1024-pixel one.
#[derive(Debug, Clone, PartialEq)]
pub struct Relief {
    /// `[min, max]` per cell, coarsest level last. Level 0 is `base` cells on a side.
    levels: alloc::vec::Vec<alloc::vec::Vec<[f32; 2]>>,
    /// The largest relief any one cell of each level holds, parallel to [`Self::levels`].
    ///
    /// Computed while the pyramid is built, because [`Self::cells_within`] asks for it once per
    /// tile per layer and recomputing it would walk every cell of every level each time.
    worst: alloc::vec::Vec<f32>,
    /// Cells on a side at level 0.
    base: u32,
}

impl Relief {
    /// Builds the pyramid over `dem`, with `cells` cells on a side at the base.
    ///
    /// `cells` is the terrain mesh's own grid -- see the type note for why that is the floor.
    /// Rounded down to a power of two so every level halves exactly; a DEM whose dimension is not
    /// a multiple of the cell count has its last row and column of pixels fold into the edge
    /// cells, which widens a cell rather than losing ground.
    #[must_use]
    pub fn new(dem: &Dem, cells: u32) -> Self {
        let base = cells.max(1).next_power_of_two().min(dem.dim().max(1));
        #[allow(clippy::cast_possible_wrap)]
        let span = (dem.dim().div_ceil(base)) as i32;

        // Level zero, straight off the DEM: each cell is the range over the pixels it covers.
        let mut level = alloc::vec::Vec::with_capacity((base * base) as usize);
        for row in 0..base {
            for column in 0..base {
                #[allow(clippy::cast_possible_wrap)]
                let (x0, y0) = ((column as i32) * span, (row as i32) * span);
                let mut range = [f32::MAX, f32::MIN];
                for y in y0..y0 + span {
                    for x in x0..x0 + span {
                        if let Some(meters) = dem.elevation_exact(x, y) {
                            range[0] = range[0].min(meters);
                            range[1] = range[1].max(meters);
                        }
                    }
                }
                // A cell entirely outside the tile has no elevation at all. Flat at zero rather
                // than `[MAX, MIN]`, which would widen every range that touched it to the whole
                // float line and split the geometry over it to the finest grid there is.
                if range[0] > range[1] {
                    range = [0.0, 0.0];
                }
                level.push(range);
            }
        }

        let mut worst = alloc::vec![
            level
                .iter()
                .map(|cell| cell[1] - cell[0])
                .fold(0.0_f32, f32::max)
        ];
        let mut levels = alloc::vec![level];
        let mut side = base;
        while side > 1 {
            let previous = levels.last().expect("a level was just pushed");
            let half = side / 2;
            let mut coarser = alloc::vec::Vec::with_capacity((half * half) as usize);
            for row in 0..half {
                for column in 0..half {
                    let mut range = [f32::MAX, f32::MIN];
                    for (dy, dx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                        let at = ((row * 2 + dy) * side + column * 2 + dx) as usize;
                        let cell = previous[at];
                        range[0] = range[0].min(cell[0]);
                        range[1] = range[1].max(cell[1]);
                    }
                    coarser.push(range);
                }
            }
            worst.push(
                coarser
                    .iter()
                    .map(|cell| cell[1] - cell[0])
                    .fold(0.0_f32, f32::max),
            );
            levels.push(coarser);
            side = half;
        }

        Self {
            levels,
            worst,
            base,
        }
    }

    /// The coarsest grid whose every cell rises and falls by at most `relief` meters.
    ///
    /// The step subdivision should use, as a cell count across the tile. One when the tile is
    /// flat enough at its own scale to need no splitting at all, which is most tiles of most
    /// maps -- a city is not a mountain range.
    ///
    /// [`Self::base`] is the floor and the answer whenever the bound cannot be met, which on
    /// genuinely steep ground is most of the time. That is not a failure to meet it: the base is
    /// the terrain mesh's own cell, the drawn surface is linear inside one, and a layer split to
    /// the same grid follows that surface exactly. See [`split_relief`] for what the bound is
    /// measured against.
    ///
    /// # Why the worst cell and not the tile's total relief
    ///
    /// A tile that climbs three hundred meters smoothly across its width has very little relief
    /// in any one cell, and a triangle inside a cell chords across only that. Choosing from the
    /// tile's total would split such a tile as finely as one with a cliff in it, which is most of
    /// the alpine world subdivided for nothing.
    ///
    /// # Why one step for the whole tile
    ///
    /// Splitting a tile's mountain finer than its plain would be fewer triangles and would put a
    /// T-junction along every boundary between the two: one side's edge has a vertex the other
    /// does not, the two displace to different heights, and the surface cracks. Crack-free
    /// adaptive subdivision is a restricted quadtree and an edge-matching pass; a uniform grid
    /// has no junctions by construction, which is the same reason the terrain mesh itself is
    /// uniform. The adaptivity that matters is *between* tiles, and this is per tile.
    #[must_use]
    pub fn cells_within(&self, relief: f32) -> u32 {
        // NaN and a bound of nothing ask for everything. An *unbounded* bound is not nonsense:
        // it is what `split_relief` answers when the exaggeration is zero, because no relief is
        // visible at all, and it asks for nothing. Treated as unknown it split a terrain drawn
        // flat at the mesh's finest grid -- five times the geometry of the same map with no
        // terrain, which was enough to run the slab region out and lose every label.
        if relief.is_nan() || relief <= 0.0 {
            return self.base;
        }
        // Coarsest first: the last level is the single cell covering the tile.
        for (level, worst) in self.worst.iter().enumerate().rev() {
            if *worst <= relief {
                #[allow(clippy::cast_possible_truncation)]
                return self.base >> (level as u32);
            }
        }
        self.base
    }

    /// The elevation range over a box in tile coordinates, as `[min, max]` meters.
    ///
    /// The box is clamped into the tile: geometry is buffered past a tile's edge and those
    /// vertices are real, but the elevation out there belongs to the neighbor and is that tile's
    /// question. Clamping answers with the edge's own relief, which is what the surface does
    /// there too.
    #[must_use]
    pub fn range(&self, x0: f32, y0: f32, x1: f32, y1: f32) -> [f32; 2] {
        #[allow(clippy::cast_precision_loss)]
        let scale = self.base as f32 / EXTENT as f32;
        let last = self.base.saturating_sub(1);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let cell = |value: f32| (value * scale).clamp(0.0, last as f32) as u32;
        let (left, right) = (cell(x0.min(x1)), cell(x0.max(x1)));
        let (top, bottom) = (cell(y0.min(y1)), cell(y0.max(y1)));

        // The coarsest level at which the box still spans at most two cells on each axis, so the
        // answer is four reads rather than the box's area. `>> level` is the cell index there.
        let mut level = 0;
        while level + 1 < self.levels.len()
            && ((right >> level) - (left >> level) > 1 || (bottom >> level) - (top >> level) > 1)
        {
            level += 1;
        }
        let side = self.base >> level;
        let cells = &self.levels[level];

        let mut range = [f32::MAX, f32::MIN];
        for y in (top >> level)..=(bottom >> level).min(side - 1) {
            for x in (left >> level)..=(right >> level).min(side - 1) {
                let at = (y * side + x) as usize;
                let Some(cell) = cells.get(at) else { continue };
                range[0] = range[0].min(cell[0]);
                range[1] = range[1].max(cell[1]);
            }
        }
        if range[0] > range[1] {
            [0.0, 0.0]
        } else {
            range
        }
    }

    /// How far the ground rises and falls over a box, which is what subdivision asks.
    #[must_use]
    pub fn relief(&self, x0: f32, y0: f32, x1: f32, y1: f32) -> f32 {
        let [low, high] = self.range(x0, y0, x1, y1);
        high - low
    }

    /// Cells on a side at the base level.
    #[must_use]
    pub const fn base(&self) -> u32 {
        self.base
    }
}

/// The relief, in meters, a triangle may span before it has to be split.
///
/// # What is being bounded
///
/// A flat triangle over displaced ground chords across whatever the ground does between its
/// corners, and the worst that chord can be wrong by is the relief over the triangle's own
/// footprint. Turn that into screen pixels and it is an error bound with an answer, the way
/// `globe::edge_segments` bounds a chord against a sphere -- rather than a subdivision table
/// chosen and then re-tuned.
///
/// # Wrong by comparison to what
///
/// To the *drawn* surface, not to the ground. The surface is piecewise linear over the terrain
/// mesh's grid, so within one mesh cell it chords across the ground exactly as a layer's triangle
/// does -- and the two chord together, which is the only agreement that shows. Below a mesh cell
/// there is nothing left to gain, which is why [`Relief`]'s base is that cell and why
/// [`Relief::cells_within`] saturates there rather than asking for more.
///
/// So on steep ground this bound is not met and the answer is the mesh's own grid. Measured on
/// the parity harness's height field, which is 15% gradient at every wavelength by construction:
/// a z14 tile has 361 m of relief and 7.5 m still inside a mesh cell, against a bound of 0.73 m.
/// The layer follows the surface there exactly; both of them miss the ground by the same 7.5 m,
/// and drawing the surface finer is a mesh question rather than a subdivision one.
///
/// # Why no camera
///
/// §5.1 makes a bucket a function of `(tile, layer, tile zoom)` and camera-free, which is what
/// lets one set of vertices serve four views at four fractional zooms and what stops the sweep
/// app rebuilding every bucket every frame. So this takes the worst case within the level instead:
/// the zoom at the top of it, where a meter is the most pixels it will ever be, and a pitch at the
/// horizon, where a vertical displacement projects most directly onto the screen. At a pitch of
/// zero the camera looks straight down and elevation is invisible -- bounding at that would split
/// nothing and be wrong the moment anybody tilted.
///
/// `latitude` is the tile's own, because a world pixel is a different number of meters at Berlin
/// than at the equator and the tile knows which it is.
#[must_use]
pub fn split_relief(z: u8, latitude: f64, exaggeration: f64, tolerance: f64) -> f64 {
    // The top of the level, as `subdivide::step_for_level` evaluates at.
    let world = tessella_tile::camera::world_size(f64::from(z) + 1.0);
    let meters_across = (latitude.to_radians()).cos().abs()
        * core::f64::consts::TAU
        * tessella_tile::camera::EARTH_RADIUS_M;
    if meters_across <= 0.0 || world <= 0.0 {
        return f64::MAX;
    }
    let pixels_per_meter = world / meters_across;
    // A vertical displacement projects onto the screen by the sine of the pitch. At the clamp the
    // sine is one to four decimal places, so this is the worst case and not an estimate of it.
    let projected = pixels_per_meter * tessella_tile::camera::MAX_PITCH.sin();
    let stretched = projected * exaggeration.max(f64::MIN_POSITIVE);
    tolerance / stretched
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dem::Encoding;
    use alloc::vec::Vec;

    /// The parity harness's own height field, in closed form.
    ///
    /// `tools/parity/scenes/dem.py` serves exactly this: six separable sinusoids at named ground
    /// wavelengths over a sea level, sampled at cell centers and encoded as Terrain-RGB. Written
    /// out here so a sample can be checked against the surface it came from rather than against
    /// another reading of the same pixels -- which is the nearest thing to an oracle a feature
    /// with no oracle gets.
    const SEA_LEVEL: f64 = 400.0;
    const EARTH_CIRCUMFERENCE: f64 = 40_075_016.686;
    const WAVELENGTHS_M: [f64; 6] = [8000.0, 4000.0, 2000.0, 1000.0, 500.0, 250.0];
    const SLOPE: f64 = 0.15;

    /// The phases `dem.py` derives from a hash. Reproduced as literals: the test needs *a*
    /// surface with curvature, not that particular one, and a SHA-256 here would be a second
    /// implementation to keep in step for no gain.
    const PHASES: [(f64, f64); 6] = [
        (0.31, 2.11),
        (1.77, 4.02),
        (5.13, 0.44),
        (2.60, 3.35),
        (4.81, 1.92),
        (0.96, 5.57),
    ];

    fn height(u: f64, v: f64) -> f64 {
        let mut total = SEA_LEVEL;
        for (index, wavelength) in WAVELENGTHS_M.into_iter().enumerate() {
            let cycles = EARTH_CIRCUMFERENCE / wavelength;
            let amplitude = SLOPE * wavelength / core::f64::consts::TAU;
            let f = core::f64::consts::TAU * cycles;
            let (u_phase, v_phase) = PHASES[index];
            total += amplitude * (f * u + u_phase).sin() * (f * v + v_phase).cos();
        }
        total
    }

    /// One tile of that field, sampled at cell centers exactly as `dem.py` samples it.
    fn tile(z: u8, x: u32, y: u32, dim: u32) -> Dem {
        let n = f64::from(1u32 << z);
        let mut pixels = Vec::with_capacity((dim * dim * 4) as usize);
        for row in 0..dim {
            let v = (f64::from(y) + (f64::from(row) + 0.5) / f64::from(dim)) / n;
            for column in 0..dim {
                let u = (f64::from(x) + (f64::from(column) + 0.5) / f64::from(dim)) / n;
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let units =
                    (((height(u, v) + 10000.0) / 0.1).round() as u32).min(256 * 256 * 256 - 1);
                pixels.extend_from_slice(&[
                    #[allow(clippy::cast_possible_truncation)]
                    ((units >> 16) as u8),
                    #[allow(clippy::cast_possible_truncation)]
                    (((units >> 8) & 0xFF) as u8),
                    #[allow(clippy::cast_possible_truncation)]
                    ((units & 0xFF) as u8),
                    255,
                ]);
            }
        }
        let image = crate::image::Image {
            width: dim,
            height: dim,
            pixels,
        };
        Dem::new(&image, Encoding::Mapbox).expect("the tile decodes")
    }

    fn terrain(exaggeration: f64) -> Terrain {
        Terrain {
            source: alloc::string::String::from("dem"),
            exaggeration,
        }
    }

    const Z: u8 = 14;
    const X: u32 = 8802;
    const Y: u32 = 5373;
    const DIM: u32 = 256;

    /// A sample at a cell's center is that cell's own value, to the encoding's tenth of a meter.
    ///
    /// This is the half pixel. Without [`DEM_CELL_CENTER_OFFSET`] the sample lands on the corner
    /// between four cells and comes back as their average, which on this surface is wrong by
    /// meters -- and is a perfectly plausible elevation, which is what makes the offset worth a
    /// test rather than a comment.
    #[test]
    fn a_cell_center_reads_its_own_elevation() {
        let dem = tile(Z, X, Y, DIM);
        let sampler = DemSampler::new(&dem, DemCover::new(Z, X, Y, Z).expect("same zoom"));
        let n = f64::from(1u32 << Z);
        for (column, row) in [(0, 0), (1, 1), (128, 64), (255, 255), (200, 17)] {
            // The tile coordinate of that cell's center: `(i + 0.5) / dim` of the tile.
            #[allow(clippy::cast_precision_loss)]
            let x = (column as f32 + 0.5) / DIM as f32 * EXTENT as f32;
            #[allow(clippy::cast_precision_loss)]
            let y = (row as f32 + 0.5) / DIM as f32 * EXTENT as f32;
            let got = elevation(&dem, sampler, &terrain(1.0), x, y, EXTENT).expect("in range");

            let u = (f64::from(X) + (f64::from(column) + 0.5) / f64::from(DIM)) / n;
            let v = (f64::from(Y) + (f64::from(row) + 0.5) / f64::from(DIM)) / n;
            #[allow(clippy::cast_possible_truncation)]
            let want = height(u, v) as f32;
            assert!(
                (got - want).abs() < 0.06,
                "cell {column},{row}: {got} against {want}"
            );
        }
    }

    /// And between two centers it is between the two values, which is what bilinear means.
    #[test]
    fn a_sample_between_centers_interpolates() {
        let dem = tile(Z, X, Y, DIM);
        let sampler = DemSampler::new(&dem, DemCover::new(Z, X, Y, Z).expect("same zoom"));
        #[allow(clippy::cast_precision_loss)]
        let at = |cell: f32| cell / DIM as f32 * EXTENT as f32;
        let flat = terrain(1.0);

        let left = elevation(&dem, sampler, &flat, at(100.5), at(100.5), EXTENT).expect("in range");
        let right =
            elevation(&dem, sampler, &flat, at(101.5), at(100.5), EXTENT).expect("in range");
        let middle =
            elevation(&dem, sampler, &flat, at(101.0), at(100.5), EXTENT).expect("in range");
        assert!(
            (middle - (left + right) / 2.0).abs() < 0.01,
            "{left} {middle} {right}"
        );
        // And it really is a surface with relief, or the check above passes on flat ground.
        assert!((left - right).abs() > 0.5, "{left} {right}");
    }

    /// Exaggeration scales the height and nothing else, and zero is flat rather than absent.
    #[test]
    fn exaggeration_scales_the_height() {
        let dem = tile(Z, X, Y, DIM);
        let sampler = DemSampler::new(&dem, DemCover::new(Z, X, Y, Z).expect("same zoom"));
        #[allow(clippy::cast_precision_loss)]
        let x = 4096.0;
        let one = elevation(&dem, sampler, &terrain(1.0), x, x, EXTENT).expect("in range");
        let twice = elevation(&dem, sampler, &terrain(2.0), x, x, EXTENT).expect("in range");
        let flat = elevation(&dem, sampler, &terrain(0.0), x, x, EXTENT).expect("in range");
        assert!((twice - one * 2.0).abs() < 0.01, "{one} {twice}");
        assert_eq!(flat, 0.0);
        // A value the spec excludes is clamped into it rather than mirroring the terrain.
        let negative = elevation(&dem, sampler, &terrain(-3.0), x, x, EXTENT).expect("in range");
        assert_eq!(negative, 0.0);
    }

    /// A tile deeper than its DEM reads the part of it that covers its own ground.
    ///
    /// The quarter, sixteenth or smaller -- and the elevation it gets is the elevation the DEM
    /// tile has at the same place on the planet, which is the whole point of the offsets.
    #[test]
    fn a_deeper_tile_reads_its_share_of_the_dem() {
        let parent = tile(Z, X, Y, DIM);
        let (cz, cx, cy) = (Z + 1, X * 2 + 1, Y * 2);
        let cover = DemCover::new(cz, cx, cy, Z).expect("one zoom deeper");
        assert_eq!(
            cover,
            DemCover {
                dz: 1,
                dx: 1,
                dy: 0
            }
        );
        assert_eq!(cover.dem_tile(cz, cx, cy), (Z, X, Y));

        let child = DemSampler::new(&parent, cover);
        let whole = DemSampler::new(&parent, DemCover::new(Z, X, Y, Z).expect("same zoom"));
        let flat = terrain(1.0);

        // The child's center is three quarters across the parent and one quarter down it.
        let from_child =
            elevation(&parent, child, &flat, 4096.0, 4096.0, EXTENT).expect("in range");
        let from_parent =
            elevation(&parent, whole, &flat, 6144.0, 2048.0, EXTENT).expect("in range");
        assert!(
            (from_child - from_parent).abs() < 0.01,
            "{from_child} {from_parent}"
        );
    }

    /// A DEM deeper than the tile is not a cover, because one tile of it is not enough ground.
    #[test]
    fn a_deeper_dem_is_no_cover() {
        assert_eq!(DemCover::new(Z, X, Y, Z + 1), None);
    }

    /// The pyramid's answer is the DEM's own for a cell-aligned box, and never tighter for any
    /// other.
    ///
    /// The levels hold *aligned* blocks, so a box that straddles a block boundary is answered at
    /// a coarser level and covers up to twice the ground it asked about. That is the trade a
    /// pyramid makes against a sparse table, which would answer any box exactly and cost a level
    /// per position rather than per power of two. It is the right way round for what asks: a
    /// range that is too wide splits geometry that did not need splitting, which is a cost, where
    /// one that was too tight would leave a triangle chording across a ridge, which is the defect.
    ///
    /// And subdivision's own boxes are cell-aligned -- it splits against a grid anchored at the
    /// tile origin with the base cell as its step -- so the conservative case is the one nothing
    /// actually asks for.
    ///
    /// Checked against a walk of the pixels rather than against another pyramid: the whole point
    /// of the structure is that it answers in four reads what the walk answers in an area, and a
    /// test that agreed with it by construction would only be checking the query arithmetic
    /// against itself.
    #[test]
    fn the_pyramid_is_the_dems_own_range() {
        let dem = tile(Z, X, Y, DIM);
        let relief = Relief::new(&dem, 128);
        assert_eq!(relief.base(), 128);

        let walk = |x0: f32, y0: f32, x1: f32, y1: f32| {
            // Every DEM pixel whose cell the box touches, which is what the pyramid aggregates.
            #[allow(clippy::cast_precision_loss)]
            let to_cell = |value: f32| {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                {
                    (value * 128.0 / EXTENT as f32).clamp(0.0, 127.0) as i32
                }
            };
            let span = i32::try_from(DIM / 128).expect("two pixels a cell");
            let (left, right) = (to_cell(x0.min(x1)), to_cell(x0.max(x1)));
            let (top, bottom) = (to_cell(y0.min(y1)), to_cell(y0.max(y1)));
            let mut range = [f32::MAX, f32::MIN];
            for cy in top..=bottom {
                for cx in left..=right {
                    for y in cy * span..(cy + 1) * span {
                        for x in cx * span..(cx + 1) * span {
                            if let Some(meters) = dem.elevation_exact(x, y) {
                                range[0] = range[0].min(meters);
                                range[1] = range[1].max(meters);
                            }
                        }
                    }
                }
            }
            range
        };

        // Cell-aligned, which is what subdivision asks: exact.
        for box_ in [
            [0.0, 0.0, 8191.0, 8191.0],
            [0.0, 0.0, 63.0, 63.0],
            [4096.0, 4096.0, 4159.0, 4159.0],
            [2048.0, 512.0, 2111.0, 575.0],
            [4096.0, 0.0, 8191.0, 4095.0],
        ] {
            let [x0, y0, x1, y1] = box_;
            let got = relief.range(x0, y0, x1, y1);
            let want = walk(x0, y0, x1, y1);
            assert!(
                (got[0] - want[0]).abs() < 1e-3 && (got[1] - want[1]).abs() < 1e-3,
                "{box_:?}: {got:?} against {want:?}"
            );
        }

        // Straddling a boundary: never tighter than the truth, which is the property that
        // matters. It is wider here -- 432.2 against 501.7 on the first of these -- because the
        // answer comes off a coarser level than the box.
        for box_ in [
            [100.0, 200.0, 2000.0, 3000.0],
            [7000.0, 100.0, 8191.0, 900.0],
            [33.0, 33.0, 97.0, 97.0],
        ] {
            let [x0, y0, x1, y1] = box_;
            let got = relief.range(x0, y0, x1, y1);
            let want = walk(x0, y0, x1, y1);
            assert!(
                got[0] <= want[0] + 1e-3,
                "{box_:?}: {got:?} against {want:?}"
            );
            assert!(
                got[1] >= want[1] - 1e-3,
                "{box_:?}: {got:?} against {want:?}"
            );
        }
    }

    /// A coarser box is never tighter than a finer one inside it, which is what makes the
    /// pyramid safe to descend: a subdivision that stops on a coarse answer has not been told
    /// the ground is flatter than it is.
    #[test]
    fn a_coarser_range_contains_a_finer_one() {
        let dem = tile(Z, X, Y, DIM);
        let relief = Relief::new(&dem, 128);
        let whole = relief.range(0.0, 0.0, 8191.0, 8191.0);
        for (x, y) in [
            (0.0, 0.0),
            (2048.0, 512.0),
            (6000.0, 7000.0),
            (4096.0, 4096.0),
        ] {
            let part = relief.range(x, y, x + 64.0, y + 64.0);
            assert!(part[0] >= whole[0] - 1e-3, "{part:?} {whole:?}");
            assert!(part[1] <= whole[1] + 1e-3, "{part:?} {whole:?}");
            // And a cell's relief is a real number rather than the sentinel an empty box gets.
            assert!(part[1] >= part[0], "{part:?}");
        }
        // The surface has relief at tile scale, or none of the above is testing anything.
        assert!(whole[1] - whole[0] > 50.0, "{whole:?}");
    }

    /// The chosen grid is the coarsest whose cells all stay inside the bound.
    ///
    /// Checked against the pyramid's own cells rather than against a number: at the answer every
    /// cell is within the bound, and at the next step coarser some cell is not. That is what
    /// "coarsest" means and it is checkable without knowing which level it lands on.
    #[test]
    fn the_grid_is_the_coarsest_that_holds() {
        let dem = tile(Z, X, Y, DIM);
        let relief = Relief::new(&dem, 128);

        for bound in [0.5_f32, 2.0, 8.0, 40.0, 200.0] {
            let cells = relief.cells_within(bound);
            assert!(cells.is_power_of_two() && cells <= relief.base(), "{cells}");

            #[allow(clippy::cast_precision_loss)]
            let step = EXTENT as f32 / cells as f32;
            let worst_cell = (0..cells)
                .flat_map(|row| (0..cells).map(move |column| (row, column)))
                .map(|(row, column)| {
                    #[allow(clippy::cast_precision_loss)]
                    let (x, y) = (column as f32 * step, row as f32 * step);
                    relief.relief(x, y, x + step - 1.0, y + step - 1.0)
                })
                .fold(0.0_f32, f32::max);
            // Either the bound holds, or the grid is the mesh's own and there is nothing finer
            // to ask for. This surface is 15% gradient by construction, so the tight bounds land
            // on the floor -- see `split_relief` for why that is the right answer and not a miss.
            assert!(
                worst_cell <= bound + 1e-3 || cells == relief.base(),
                "{bound}: {cells} cells still hold {worst_cell}"
            );

            // And one step coarser does not hold -- unless the whole tile already fits, which is
            // the answer of one cell.
            if cells > 1 && worst_cell <= bound {
                let coarser = cells / 2;
                #[allow(clippy::cast_precision_loss)]
                let wide = EXTENT as f32 / coarser as f32;
                let worst = (0..coarser)
                    .flat_map(|row| (0..coarser).map(move |column| (row, column)))
                    .map(|(row, column)| {
                        #[allow(clippy::cast_precision_loss)]
                        let (x, y) = (column as f32 * wide, row as f32 * wide);
                        relief.relief(x, y, x + wide - 1.0, y + wide - 1.0)
                    })
                    .fold(0.0_f32, f32::max);
                assert!(
                    worst > bound,
                    "{bound}: {coarser} cells would have held at {worst}"
                );
            }
        }
    }

    /// A tighter bound never asks for a coarser grid.
    #[test]
    fn a_tighter_bound_is_never_coarser() {
        let dem = tile(Z, X, Y, DIM);
        let relief = Relief::new(&dem, 128);
        let mut previous = 0;
        for bound in [1000.0_f32, 200.0, 40.0, 8.0, 2.0, 0.5, 0.01] {
            let cells = relief.cells_within(bound);
            assert!(cells >= previous, "{bound}: {cells} after {previous}");
            previous = cells;
        }
        // A bound past the tile's whole relief needs no subdivision at all.
        let whole = relief.relief(0.0, 0.0, 8191.0, 8191.0);
        assert_eq!(relief.cells_within(whole + 1.0), 1);
        // And a bound of nothing, or of nonsense, asks for everything rather than dividing.
        assert_eq!(relief.cells_within(0.0), relief.base());
        assert_eq!(relief.cells_within(f32::NAN), relief.base());
        // An unbounded one asks for nothing: it is what a zero exaggeration produces.
        assert_eq!(relief.cells_within(f32::INFINITY), 1);
    }

    /// No exaggeration, no splitting: the bound is unbounded and the grid is one cell.
    #[test]
    fn a_flat_terrain_asks_for_one_cell() {
        let bound = split_relief(14, 52.5, 0.0, 0.5);
        assert!(
            bound.is_infinite() || bound > f64::from(f32::MAX),
            "{bound}"
        );
        #[allow(clippy::cast_possible_truncation)]
        let as_f32 = bound as f32;
        assert!(as_f32.is_infinite());
    }

    /// Flat ground needs no splitting, however tight the bound.
    ///
    /// The case that decides whether this is worth having: most tiles of most maps are a city
    /// rather than a mountain range, and a grid chosen from the tile's own relief gives them one
    /// cell where a fixed grid would give them sixteen thousand.
    #[test]
    fn flat_ground_is_one_cell() {
        let flat = crate::image::Image {
            width: DIM,
            height: DIM,
            // 500 m everywhere, in Terrain-RGB.
            pixels: core::iter::repeat_n(
                {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let units = ((500.0_f64 + 10000.0) / 0.1).round() as u32;
                    #[allow(clippy::cast_possible_truncation)]
                    [
                        (units >> 16) as u8,
                        ((units >> 8) & 0xFF) as u8,
                        (units & 0xFF) as u8,
                        255,
                    ]
                },
                (DIM * DIM) as usize,
            )
            .flatten()
            .collect(),
        };
        let dem = Dem::new(&flat, Encoding::Mapbox).expect("the tile decodes");
        let relief = Relief::new(&dem, 128);
        assert_eq!(relief.relief(0.0, 0.0, 8191.0, 8191.0), 0.0);
        assert_eq!(relief.cells_within(0.01), 1);
    }

    /// A box outside the tile answers with the edge's relief rather than with nothing.
    ///
    /// Geometry is buffered past a tile's edge and those vertices are real; the ground out there
    /// belongs to the neighbor and is its question. An empty answer would read as flat and stop
    /// the geometry being split at exactly the edge where two tiles have to agree.
    #[test]
    fn a_box_outside_the_tile_clamps_to_its_edge() {
        let dem = tile(Z, X, Y, DIM);
        let relief = Relief::new(&dem, 128);
        let outside = relief.range(-2048.0, -2048.0, -1024.0, -1024.0);
        let corner = relief.range(0.0, 0.0, 63.0, 63.0);
        assert_eq!(outside, corner);
    }

    /// The split bound is meters of relief, and it tightens as the ground gets bigger on screen.
    ///
    /// Half a pixel of error at z14 over Berlin is under a meter of ground, which is the right
    /// order: a z14 tile is about 1.9 km of ground across 512 world pixels at the top of its
    /// level, so a world pixel is a few meters and half of one is a fraction of that.
    #[test]
    fn the_split_bound_tightens_with_the_zoom() {
        let berlin = 52.52;
        let mut previous = f64::MAX;
        for z in 4..18 {
            let meters = split_relief(z, berlin, 1.0, 0.5);
            assert!(meters < previous, "z{z}: {meters} against {previous}");
            previous = meters;
        }
        let at14 = split_relief(14, berlin, 1.0, 0.5);
        assert!(at14 > 0.05 && at14 < 5.0, "{at14}");

        // Exaggeration stretches the ground, so the same tolerance admits proportionally less of
        // it -- a terrain at twice the height needs twice the subdivision to stay as accurate.
        let doubled = split_relief(14, berlin, 2.0, 0.5);
        assert!((doubled * 2.0 - at14).abs() < 1e-9, "{at14} {doubled}");

        // And a flattened terrain never needs splitting, rather than dividing by zero.
        assert!(split_relief(14, berlin, 0.0, 0.5).is_finite());
        assert!(split_relief(14, berlin, 0.0, 0.5) > 1e30);
    }

    /// Latitude is the tile's, because a world pixel is fewer meters the further from the equator
    /// it is -- so the same relief is more pixels at Tromso than at Nairobi, and the bound is
    /// correspondingly tighter.
    #[test]
    fn the_split_bound_follows_the_latitude() {
        let equator = split_relief(14, 0.0, 1.0, 0.5);
        let berlin = split_relief(14, 52.52, 1.0, 0.5);
        let tromso = split_relief(14, 69.65, 1.0, 0.5);
        assert!(berlin < equator, "{berlin} {equator}");
        assert!(tromso < berlin, "{tromso} {berlin}");
    }

    /// A sample past the border is refused rather than clamped: it is a question about the
    /// neighboring tile, and answering it from this one puts a cliff along every tile edge.
    #[test]
    fn a_sample_past_the_border_is_refused() {
        let dem = tile(Z, X, Y, DIM);
        let sampler = DemSampler::new(&dem, DemCover::new(Z, X, Y, Z).expect("same zoom"));
        let flat = terrain(1.0);
        // One whole cell outside the tile, which is past the single-pixel border.
        #[allow(clippy::cast_precision_loss)]
        let outside = -2.0 / DIM as f32 * EXTENT as f32;
        assert_eq!(
            elevation(&dem, sampler, &flat, outside, 4096.0, EXTENT),
            None
        );
        // And the edge itself is inside it, because the border is what the edge samples.
        assert!(elevation(&dem, sampler, &flat, 0.0, 0.0, EXTENT).is_some());
        assert!(elevation(&dem, sampler, &flat, EXTENT as f32 - 1.0, 4096.0, EXTENT).is_some());
    }
}
