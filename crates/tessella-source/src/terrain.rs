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
