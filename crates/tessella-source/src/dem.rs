//! A raster-DEM tile: elevation packed into color channels, with a border.
//!
//! mbgl's `DEMData`. A hillshade reads the slope at every pixel, which needs the eight pixels
//! around it, which the pixels on a tile's edge do not have. So a DEM tile is stored one pixel
//! larger on every side and that ring is filled twice: once at construction by repeating the
//! nearest row or column, and again from the neighboring tiles as they arrive.
//!
//! # Why the border is filled twice
//!
//! Because the neighbors are not there yet. A tile is decoded the moment its bytes land and the
//! frame draws whatever it has, so a tile whose border waited for four neighbors would draw
//! nothing for as long as the slowest of them took. Repeating the edge is wrong by a pixel and
//! looks like a slightly flattened rim; waiting is a hole. mbgl's comment says it plainly --
//! "in order to avoid flashing seams between tiles" -- and the second fill is
//! [`Dem::backfill_border`], which replaces the guess once the real data exists.
//!
//! # The elevation is not decoded here
//!
//! What this holds is the packed image, one pixel wider on each side, and what reads it is the
//! shader: the prepare pass unpacks a texel with the same three multipliers and a bias.
//! [`Dem::elevation`] exists for tests and for anything on this side that needs a number, and it
//! is deliberately the same arithmetic in the same order -- a second formula for the same
//! quantity is a second thing to keep in agreement.

use alloc::vec;
use alloc::vec::Vec;

/// How elevation is packed into a DEM tile's channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Encoding {
    /// Mapbox Terrain-RGB: a tenth of a meter per unit over a 24-bit range, biased by 10,000.
    ///
    /// The spec's default, and what a source that names no encoding gets.
    #[default]
    Mapbox,
    /// Terrarium: meters in the red and green channels, 1/256 in blue, biased by 32,768.
    Terrarium,
}

impl Encoding {
    /// The style spec's name for it, or `None` for a name this does not read.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "mapbox" => Some(Self::Mapbox),
            "terrarium" => Some(Self::Terrarium),
            _ => None,
        }
    }

    /// The three multipliers and the bias, in the order the shader applies them.
    ///
    /// mbgl's `getUnpackVector`, and the values are its: `{6553.6, 25.6, 0.1, 10000}` for Mapbox
    /// and `{256, 1, 1/256, 32768}` for Terrarium. They travel to the shader as they are, so a
    /// difference here is a difference in every pixel of every hillshade.
    #[must_use]
    pub const fn unpack(self) -> [f32; 4] {
        match self {
            Self::Mapbox => [6553.6, 25.6, 0.1, 10000.0],
            Self::Terrarium => [256.0, 1.0, 1.0 / 256.0, 32768.0],
        }
    }
}

/// A decoded DEM tile, one pixel larger on every side than the tile it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Dem {
    /// The tile's own width, without the border. mbgl's `dim`.
    dim: u32,
    /// The stored width, which is `dim + 2`. mbgl's `stride`.
    stride: u32,
    encoding: Encoding,
    /// `stride * stride * 4` bytes of RGBA.
    pixels: Vec<u8>,
}

/// Why a DEM tile could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DemError {
    /// The image is not square. mbgl throws here too: a DEM tile's `dim` is one number.
    #[error("a raster-dem tile must be square, got {width}x{height}")]
    NotSquare {
        /// Its width.
        width: u32,
        /// Its height.
        height: u32,
    },
    /// The image has no area, so there is nothing to put a border around.
    #[error("a raster-dem tile must have area")]
    Empty,
}

impl Dem {
    /// Reads a decoded image as a DEM tile, and fills its border by repeating the edge.
    ///
    /// # Errors
    ///
    /// [`DemError`] when the image is not square or has no area.
    pub fn new(image: &crate::image::Image, encoding: Encoding) -> Result<Self, DemError> {
        if image.width != image.height {
            return Err(DemError::NotSquare {
                width: image.width,
                height: image.height,
            });
        }
        if image.width == 0 {
            return Err(DemError::Empty);
        }

        let dim = image.width;
        let stride = dim + 2;
        let mut dem = Self {
            dim,
            stride,
            encoding,
            pixels: vec![0; (stride as usize) * (stride as usize) * 4],
        };

        // The tile itself, one row at a time, offset by the border on both axes.
        for row in 0..dim as usize {
            let from = row * dim as usize * 4;
            let to = ((row + 1) * stride as usize + 1) * 4;
            let run = dim as usize * 4;
            dem.pixels[to..to + run].copy_from_slice(&image.pixels[from..from + run]);
        }

        dem.repeat_edges();
        Ok(dem)
    }

    /// Fills the border ring from the nearest row or column of the tile itself.
    ///
    /// The first of the two fills. It is wrong by a pixel at every edge -- the slope there is
    /// computed against a repeat of the edge rather than against the neighbor -- and it is what
    /// lets a tile draw before its neighbors exist. [`Self::backfill_border`] replaces it.
    fn repeat_edges(&mut self) {
        let (dim, stride) = (self.dim as usize, self.stride as usize);

        for row in 0..dim {
            let start = (row + 1) * stride;
            // Left, from the first real column; right, from the last.
            self.copy_texel(start + 1, start);
            self.copy_texel(start + dim, start + dim + 1);
        }

        // Top and bottom, corners included, which is why these run the whole stride: the corner
        // texels were just written by the loop above.
        let run = stride * 4;
        self.pixels.copy_within(stride * 4..stride * 4 + run, 0);
        let last = dim * stride * 4;
        self.pixels
            .copy_within(last..last + run, (dim + 1) * stride * 4);
    }

    fn copy_texel(&mut self, from: usize, to: usize) {
        let (from, to) = (from * 4, to * 4);
        let texel = [
            self.pixels[from],
            self.pixels[from + 1],
            self.pixels[from + 2],
            self.pixels[from + 3],
        ];
        self.pixels[to..to + 4].copy_from_slice(&texel);
    }

    /// Replaces part of the border with the real pixels from a neighboring tile.
    ///
    /// `dx` and `dy` say which neighbor it is, each in `-1..=1`: `(-1, -1)` is the tile above and
    /// to the left, which contributes exactly one corner texel, and `(0, -1)` is the tile above,
    /// which contributes the whole top edge. `(0, 0)` is this tile and is a caller's mistake --
    /// it copies the tile onto itself and is harmless, which is why it is not an error.
    ///
    /// A no-op when the two tiles are different sizes. mbgl asserts instead; tiles from one
    /// source are always the same size, so this is the case that cannot arise being declined
    /// rather than crashing a frame over it.
    pub fn backfill_border(&mut self, neighbor: &Self, dx: i8, dy: i8) {
        if self.dim != neighbor.dim {
            return;
        }
        let dim = i32::try_from(self.dim).unwrap_or(i32::MAX);
        let (dx, dy) = (i32::from(dx), i32::from(dy));

        // The range this neighbor owns. A neighbor to the side owns one column of the border
        // and the full height; one above or below owns one row and the full width; a diagonal
        // owns the single corner where the two meet.
        let (mut x_min, mut x_max) = (dx * dim, dx * dim + dim);
        let (mut y_min, mut y_max) = (dy * dim, dy * dim + dim);
        if dx == -1 {
            x_min = x_max - 1;
        } else if dx == 1 {
            x_max = x_min + 1;
        }
        if dy == -1 {
            y_min = y_max - 1;
        } else if dy == 1 {
            y_max = y_min + 1;
        }

        // The same point in the neighbor's own coordinates.
        let (ox, oy) = (-dx * dim, -dy * dim);

        for y in y_min..y_max {
            for x in x_min..x_max {
                let Some(to) = self.index(x, y) else { continue };
                let Some(from) = neighbor.index(x + ox, y + oy) else {
                    continue;
                };
                let texel = [
                    neighbor.pixels[from * 4],
                    neighbor.pixels[from * 4 + 1],
                    neighbor.pixels[from * 4 + 2],
                    neighbor.pixels[from * 4 + 3],
                ];
                self.pixels[to * 4..to * 4 + 4].copy_from_slice(&texel);
            }
        }
    }

    /// The texel index for a tile coordinate, which may be `-1` or `dim` for the border.
    ///
    /// `None` outside that, where mbgl asserts. A frame is not worth ending over a coordinate
    /// that cannot arise, and a silently wrong texel is worse than a skipped one.
    fn index(&self, x: i32, y: i32) -> Option<usize> {
        let dim = i32::try_from(self.dim).ok()?;
        if x < -1 || x > dim || y < -1 || y > dim {
            return None;
        }
        let stride = i32::try_from(self.stride).ok()?;
        usize::try_from((y + 1) * stride + (x + 1)).ok()
    }

    /// The elevation at a tile coordinate, in the units the encoding carries.
    ///
    /// mbgl's `DEMData::get`, truncating the same way. The truncation is mbgl's and is *not* what
    /// the prepare pass sees -- its `getElevation` is a float dot product with no rounding at all
    /// -- so this is for tests and for anything that wants a whole number of meters, and
    /// [`Self::elevation_exact`] is what the arithmetic uses.
    #[must_use]
    pub fn elevation(&self, x: i32, y: i32) -> Option<i32> {
        #[allow(clippy::cast_possible_truncation)]
        self.elevation_exact(x, y).map(|meters| meters as i32)
    }

    /// The elevation at a tile coordinate, unrounded.
    ///
    /// The shader's `getElevation`: the texel times 255, its alpha replaced by -1, dotted with the
    /// unpack vector. Written as the three products and the bias because that is the same
    /// arithmetic in the same order, and the channels are already bytes here where the shader has
    /// to scale them back up from normalized floats.
    #[must_use]
    pub fn elevation_exact(&self, x: i32, y: i32) -> Option<f32> {
        let index = self.index(x, y)?;
        let unpack = self.encoding.unpack();
        let texel = &self.pixels[index * 4..index * 4 + 4];
        Some(
            f32::from(texel[0]) * unpack[0]
                + f32::from(texel[1]) * unpack[1]
                + f32::from(texel[2]) * unpack[2]
                - unpack[3],
        )
    }

    /// The tile's own width, without the border.
    #[must_use]
    pub const fn dim(&self) -> u32 {
        self.dim
    }

    /// The stored width, which is [`Self::dim`] plus a pixel on each side.
    #[must_use]
    pub const fn stride(&self) -> u32 {
        self.stride
    }

    /// How its channels are packed.
    #[must_use]
    pub const fn encoding(&self) -> Encoding {
        self.encoding
    }

    /// The stored pixels, `stride * stride * 4` bytes of RGBA, for the texture upload.
    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// The slope field a hillshade reads, as a `dim` by `dim` image.
    ///
    /// mbgl's hillshade *prepare* pass, which it runs on the GPU into a render target of its own
    /// per tile. This runs it here instead, once, on the thread that just decoded the tile.
    ///
    /// # Why this is not an offscreen pass
    ///
    /// Because it does not depend on the camera. `HillshadePrepareLayerTweaker` sets
    /// `.zoom = tileID.canonical.z` and the matrix to a fixed `ortho(0, EXTENT, -EXTENT, 0)`;
    /// every other uniform it writes -- the unpack vector, the dimension, the maxzoom -- is a
    /// property of the tile. So the pass is a pure function of the DEM tile and its zoom, and
    /// mbgl runs it on the GPU because the DEM is already a texture there, not because anything
    /// about it needs to be.
    ///
    /// Run here it costs one Sobel per tile on a worker that is already decoding a PNG, and it
    /// costs no render target per tile, no offscreen view, and no second pass per frame. What a
    /// hillshade layer then draws is a tile quad sampling an ordinary texture, which is the shape
    /// of a raster layer and already at parity.
    ///
    /// # The arithmetic
    ///
    /// A Sobel operator over the eight neighbors, scaled from pixel slope to world slope and
    /// encoded into two channels:
    ///
    /// ```text
    /// deriv = ((c + 2f + i) - (a + 2d + g), (g + 2h + i) - (a + 2b + c))
    ///       * dim / 2^(exaggeration + 28.2562 - zoom)
    /// r, g  = clamp(deriv / 8 + 0.5, 0, 1)
    /// ```
    ///
    /// The center sample `e` is not read -- a Sobel's center weight is zero, and mbgl leaves the
    /// line commented out to say so. `28.2562` is the log2 of the meters per pixel at zoom zero,
    /// and the exaggeration is the zoom-dependent softening mapbox-gl-js#5286 settled on. The
    /// division by eight assumes a world slope no steeper than four, which is what makes the
    /// range fit in a byte.
    ///
    /// `u_maxzoom` is in mbgl's uniform block and no shader reads it, so nothing here carries it.
    ///
    /// The neighbors reach one pixel outside the tile, into the border -- which is the whole
    /// reason the border exists, and the reason this has to be re-run when
    /// [`Self::backfill_border`] replaces it.
    #[must_use]
    pub fn prepare(&self, zoom: u8) -> crate::image::Image {
        let dim = self.dim;
        let zoom = f32::from(zoom);

        // mapbox-gl-js#5286: the effect is softened at low zoom, where a pixel covers enough
        // ground that the true slope reads as noise.
        let factor = if zoom < 2.0 {
            0.4
        } else if zoom < 4.5 {
            0.35
        } else {
            0.3
        };
        let exaggeration = if zoom < 15.0 {
            (zoom - 15.0) * factor
        } else {
            0.0
        };
        #[allow(clippy::cast_precision_loss)]
        let scale = dim as f32 / libm::powf(2.0, exaggeration + (28.2562 - zoom));

        let mut pixels = Vec::with_capacity((dim as usize) * (dim as usize) * 4);
        for y in 0..dim {
            for x in 0..dim {
                #[allow(clippy::cast_possible_wrap)]
                let (x, y) = (x as i32, y as i32);
                let at = |dx: i32, dy: i32| self.elevation_exact(x + dx, y + dy).unwrap_or(0.0);
                let (a, b, c) = (at(-1, -1), at(0, -1), at(1, -1));
                let (d, f) = (at(-1, 0), at(1, 0));
                let (g, h, i) = (at(-1, 1), at(0, 1), at(1, 1));

                let dx = ((c + f + f + i) - (a + d + d + g)) * scale;
                let dy = ((g + h + h + i) - (a + b + b + c)) * scale;
                pixels.extend_from_slice(&[
                    quantize(dx / 8.0 + 0.5),
                    quantize(dy / 8.0 + 0.5),
                    255,
                    255,
                ]);
            }
        }

        crate::image::Image {
            width: dim,
            height: dim,
            pixels,
        }
    }
}

/// A clamped channel, as writing to an eight-bit render target produces one.
///
/// Rounds, which is what a GPU does storing a float into a `UnsignedByte` attachment, and what
/// truncating here would differ from by a whole level on half the values.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn quantize(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tile whose texel at `(x, y)` encodes `x * 1000 + y` meters, so a misplaced copy is a
    /// wrong number rather than a plausible one.
    fn ramp(dim: u32) -> crate::image::Image {
        let mut pixels = Vec::with_capacity((dim * dim * 4) as usize);
        for y in 0..dim {
            for x in 0..dim {
                let meters = f64::from(x) * 1000.0 + f64::from(y);
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let units = ((meters + 10000.0) / 0.1).round() as u32;
                pixels.extend_from_slice(&[
                    u8::try_from(units >> 16).unwrap_or(255),
                    u8::try_from((units >> 8) & 0xFF).unwrap_or(255),
                    u8::try_from(units & 0xFF).unwrap_or(255),
                    255,
                ]);
            }
        }
        crate::image::Image {
            width: dim,
            height: dim,
            pixels,
        }
    }

    fn dem(dim: u32) -> Dem {
        Dem::new(&ramp(dim), Encoding::Mapbox).expect("a square tile")
    }

    #[test]
    fn the_stored_image_is_two_wider_than_the_tile() {
        let dem = dem(8);
        assert_eq!(dem.dim(), 8);
        assert_eq!(dem.stride(), 10);
        assert_eq!(dem.pixels().len(), 10 * 10 * 4);
    }

    #[test]
    fn a_tiles_own_pixels_survive_the_copy() {
        let dem = dem(8);
        for y in 0..8 {
            for x in 0..8 {
                assert_eq!(dem.elevation(x, y), Some(x * 1000 + y), "at {x},{y}");
            }
        }
    }

    /// The first fill: the border repeats the nearest row or column, corners included.
    #[test]
    fn the_border_starts_as_a_repeat_of_the_edge() {
        let dem = dem(8);
        for i in 0..8 {
            assert_eq!(dem.elevation(-1, i), dem.elevation(0, i), "left at {i}");
            assert_eq!(dem.elevation(8, i), dem.elevation(7, i), "right at {i}");
            assert_eq!(dem.elevation(i, -1), dem.elevation(i, 0), "top at {i}");
            assert_eq!(dem.elevation(i, 8), dem.elevation(i, 7), "bottom at {i}");
        }
        // The corners come from the horizontal fill, which runs the whole stride.
        assert_eq!(dem.elevation(-1, -1), dem.elevation(0, 0));
        assert_eq!(dem.elevation(8, 8), dem.elevation(7, 7));
        assert_eq!(dem.elevation(-1, 8), dem.elevation(0, 7));
        assert_eq!(dem.elevation(8, -1), dem.elevation(7, 0));
    }

    /// The second fill: a neighbor to the east replaces the right column with its own first.
    #[test]
    fn a_side_neighbor_replaces_one_column() {
        let east = dem(8);
        let mut dem = dem(8);

        // Before: the right border repeats this tile's own last column.
        assert_eq!(dem.elevation(8, 3), Some(7 * 1000 + 3));
        dem.backfill_border(&east, 1, 0);
        // After: it is the neighbor's *first* column, which in the ramp is x = 0.
        assert_eq!(dem.elevation(8, 3), Some(3));
        // And nothing else moved.
        assert_eq!(dem.elevation(7, 3), Some(7 * 1000 + 3));
        assert_eq!(dem.elevation(-1, 3), Some(3));
    }

    #[test]
    fn a_neighbor_above_replaces_one_row() {
        let north = dem(8);
        let mut dem = dem(8);
        dem.backfill_border(&north, 0, -1);
        for x in 0..8 {
            // The neighbor's last row, which in the ramp is y = 7.
            assert_eq!(dem.elevation(x, -1), Some(x * 1000 + 7), "at {x}");
        }
    }

    /// A diagonal neighbor owns exactly one texel, and touches nothing else.
    #[test]
    fn a_diagonal_neighbor_replaces_one_corner() {
        let north_west = dem(8);
        let mut dem = dem(8);
        let before: Vec<Option<i32>> = (0..8).map(|x| dem.elevation(x, -1)).collect();

        dem.backfill_border(&north_west, -1, -1);
        // The neighbor's bottom-right texel.
        assert_eq!(dem.elevation(-1, -1), Some(7 * 1000 + 7));
        // The top edge proper is untouched -- that belongs to the tile above, not this one.
        let after: Vec<Option<i32>> = (0..8).map(|x| dem.elevation(x, -1)).collect();
        assert_eq!(before, after);
    }

    /// Outside the border is `None` rather than a panic. mbgl asserts; a frame is not worth
    /// ending over a coordinate that cannot arise.
    #[test]
    fn a_coordinate_past_the_border_has_no_elevation() {
        let dem = dem(8);
        assert_eq!(dem.elevation(-2, 0), None);
        assert_eq!(dem.elevation(9, 0), None);
        assert_eq!(dem.elevation(0, -2), None);
        assert_eq!(dem.elevation(0, 9), None);
        assert!(dem.elevation(-1, -1).is_some());
        assert!(dem.elevation(8, 8).is_some());
    }

    /// The unpack vectors are mbgl's, and a hillshade is wrong in every pixel if they are not.
    #[test]
    fn the_unpack_vectors_are_the_oracles() {
        assert_eq!(Encoding::Mapbox.unpack(), [6553.6, 25.6, 0.1, 10000.0]);
        assert_eq!(
            Encoding::Terrarium.unpack(),
            [256.0, 1.0, 1.0 / 256.0, 32768.0]
        );
        assert_eq!(Encoding::parse("mapbox"), Some(Encoding::Mapbox));
        assert_eq!(Encoding::parse("terrarium"), Some(Encoding::Terrarium));
        assert_eq!(Encoding::parse("srtm"), None);
        // The spec's default, which is what a source naming no encoding gets.
        assert_eq!(Encoding::default(), Encoding::Mapbox);
    }

    /// A flat tile has no slope, so both derivative channels land on the middle of the range.
    ///
    /// The exact byte matters: 0.5 quantizes to 128, and a `quantize` that truncated would give
    /// 127 -- a whole level of bias on every flat pixel of every hillshade.
    #[test]
    fn flat_ground_encodes_to_the_middle_of_the_range() {
        let image = crate::image::Image {
            width: 8,
            height: 8,
            // Whatever elevation, as long as it is the same everywhere.
            pixels: [1u8, 2, 3, 255].repeat(8 * 8),
        };
        let dem = Dem::new(&image, Encoding::Mapbox).expect("square");
        let prepared = dem.prepare(14);

        assert_eq!(prepared.width, 8);
        assert_eq!(prepared.height, 8);
        for texel in prepared.pixels.as_chunks::<4>().0 {
            assert_eq!(texel, &[128, 128, 255, 255]);
        }
    }

    /// A ramp in x tilts the red channel and leaves green alone, and the other way for a ramp in
    /// y. Which channel is which is the kind of thing that is invisible in a picture -- a
    /// hillshade with x and y swapped is lit from the wrong quarter and still looks like terrain.
    #[test]
    fn a_ramp_tilts_the_channel_it_runs_along() {
        let east = dem(8).prepare(14);
        // The ramp's texel at (x, y) is x * 1000 + y meters, so x rises by 1000 m a pixel and y
        // by 1 m -- a thousand to one, which no rounding can blur.
        let texel = &east.pixels[(3 * 8 + 3) * 4..(3 * 8 + 3) * 4 + 4];
        assert!(texel[0] > 200, "x runs uphill and saturates red: {texel:?}");
        assert!((127..=129).contains(&texel[1]), "y barely moves: {texel:?}");
    }

    /// The border is what the edge pixels read, so replacing it changes the slope there and
    /// nowhere else. This is why `prepare` has to be re-run after a backfill.
    #[test]
    fn a_backfilled_border_changes_the_edge_and_only_the_edge() {
        let before = dem(8).prepare(14);

        let mut after = dem(8);
        // A neighbor to the east whose elevations are a long way from this tile's.
        let mut far = ramp(8);
        for texel in far.pixels.as_chunks_mut::<4>().0 {
            texel[0] = texel[0].saturating_add(8);
        }
        after.backfill_border(&Dem::new(&far, Encoding::Mapbox).expect("square"), 1, 0);
        let after = after.prepare(14);

        let column = |image: &crate::image::Image, x: usize| -> Vec<u8> {
            (0..8).map(|y| image.pixels[(y * 8 + x) * 4]).collect()
        };
        assert_ne!(
            column(&before, 7),
            column(&after, 7),
            "the last column reads the border"
        );
        for x in 0..6 {
            assert_eq!(
                column(&before, x),
                column(&after, x),
                "column {x} is interior"
            );
        }
    }

    /// The zoom-dependent softening from mapbox-gl-js#5286, at the breakpoints mbgl uses.
    ///
    /// Same terrain, five zooms. The scale is `dim / 2^(exaggeration + 28.2562 - zoom)` and the
    /// exaggeration stops at 15, so the encoded slope rises with zoom the whole way -- steeply up
    /// to 15 and then at half the rate.
    ///
    /// The slope is 300 m a pixel, which is absurd terrain and the point: at the ramp fixture's
    /// 1000 m the channel saturates at every zoom and the curve is invisible, and at anything
    /// gentle it rounds to 128 at every zoom and the curve is invisible the other way. A fixture
    /// that shows nothing passes whatever the arithmetic does.
    #[test]
    fn the_exaggeration_follows_the_zoom() {
        let mut image = ramp(8);
        for (index, texel) in image.pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let meters = (index % 8) as f64 * 300.0;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let units = ((meters + 10000.0) / 0.1).round() as u32;
            texel[0] = u8::try_from(units >> 16).unwrap_or(255);
            texel[1] = u8::try_from((units >> 8) & 0xFF).unwrap_or(255);
            texel[2] = u8::try_from(units & 0xFF).unwrap_or(255);
        }
        let dem = Dem::new(&image, Encoding::Mapbox).expect("square");
        let at = |zoom: u8| dem.prepare(zoom).pixels[(4 * 8 + 4) * 4];

        let curve: Vec<u8> = [10, 12, 14, 15, 16].into_iter().map(at).collect();
        assert!(
            curve.windows(2).all(|pair| pair[0] < pair[1]),
            "the encoded slope rises with zoom: {curve:?}"
        );
        // And none of it is against a stop, which is what would make the comparison vacuous.
        assert!(
            curve.iter().all(|level| (1..255).contains(level)),
            "{curve:?}"
        );
    }

    #[test]
    fn a_tile_that_is_not_square_is_refused() {
        let image = crate::image::Image {
            width: 4,
            height: 8,
            pixels: vec![0; 4 * 8 * 4],
        };
        assert_eq!(
            Dem::new(&image, Encoding::Mapbox),
            Err(DemError::NotSquare {
                width: 4,
                height: 8
            })
        );
    }

    #[test]
    fn a_tile_with_no_area_is_refused() {
        let image = crate::image::Image {
            width: 0,
            height: 0,
            pixels: Vec::new(),
        };
        assert_eq!(Dem::new(&image, Encoding::Mapbox), Err(DemError::Empty));
    }

    /// A neighbor of a different size is declined rather than asserted away.
    #[test]
    fn a_neighbor_of_a_different_size_changes_nothing() {
        let mut dem = dem(8);
        let before = dem.clone();
        dem.backfill_border(&Dem::new(&ramp(4), Encoding::Mapbox).expect("square"), 1, 0);
        assert_eq!(dem, before);
    }
}
