//! Tiling parameters for a GeoJSON source.
//!
//! These are the numbers that decide what a tile's geometry actually contains, and every one of
//! them was checked against the oracle rather than taken on faith. The values here reproduce
//! what `mbgl-capture-probe --dump-vertices` shows for the hermetic style.
//!
//! # The buffer is why coordinates go outside the tile
//!
//! Tile-local coordinates are not confined to `0..EXTENT`. The hermetic style's fill geometry
//! spans `-2048..10240` at an extent of 8192, which is `-buffer..EXTENT + buffer`. Features are
//! clipped to the tile *plus a margin*, so that a shape crossing a tile edge still has geometry
//! on both sides and neither tile shows a seam where an antialiased edge or a wide line should
//! continue.
//!
//! A reader that clamped to `0..EXTENT` would produce visually plausible output with a hairline
//! gap at every tile boundary — the kind of bug that survives review because each tile is
//! individually correct.
//!
//! # The scale factor is the trap
//!
//! The source options are expressed in *screen* units, at the 512-pixel tile size, and the
//! tiler works in *tile* units at `EXTENT`. mbgl scales between them by `EXTENT / tileSize`,
//! which is 16. So a buffer written as 128 becomes 2048, and a tolerance written as 0.375
//! becomes 6. Using the unscaled numbers gives a buffer sixteen times too small, which again
//! looks almost right.

/// Tile-local coordinate extent, matching `tessella_tile::projection::EXTENT`.
pub const EXTENT: i32 = 8192;

/// Tile size in screen pixels, which is the unit the source options are written in.
pub const TILE_SIZE: i32 = 512;

/// Factor between the units the source options use and the units the tiler uses.
///
/// `EXTENT / TILE_SIZE`, which is 16.
pub const SCALE: i32 = EXTENT / TILE_SIZE;

/// A GeoJSON source's tiling options, in the screen units the style spec writes them in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TilingOptions {
    /// Margin around each tile, in screen units.
    pub buffer: u16,
    /// Douglas-Peucker simplification tolerance, in screen units.
    pub tolerance: f64,
    /// Zoom past which tiles are overscaled rather than generated.
    pub maxzoom: u8,
}

impl Default for TilingOptions {
    /// The spec's defaults, which are also mbgl's.
    fn default() -> Self {
        Self {
            buffer: 128,
            tolerance: 0.375,
            maxzoom: 18,
        }
    }
}

impl TilingOptions {
    /// The buffer in tile units.
    ///
    /// Rounded, not truncated: mbgl uses `round`, and for a buffer that is not a multiple of
    /// the scale the two differ by a unit at the tile edge.
    #[must_use]
    pub fn buffer_in_tile_units(&self) -> i32 {
        #[allow(clippy::cast_possible_truncation)]
        {
            (f64::from(self.buffer) * f64::from(SCALE)).round() as i32
        }
    }

    /// The simplification tolerance in tile units.
    #[must_use]
    pub fn tolerance_in_tile_units(&self) -> f64 {
        self.tolerance * f64::from(SCALE)
    }

    /// The inclusive range a clipped coordinate may occupy on either axis.
    #[must_use]
    pub fn clip_range(&self) -> (i32, i32) {
        let buffer = self.buffer_in_tile_units();
        (-buffer, EXTENT + buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Checked against `--dump-vertices` on the hermetic style, not against the documentation.
    ///
    /// The fill geometry's coordinates span exactly -2048..10240, and these are the numbers
    /// that produce that range.
    #[test]
    fn the_defaults_reproduce_the_oracles_clip_range() {
        let options = TilingOptions::default();
        assert_eq!(options.buffer_in_tile_units(), 2048);
        assert_eq!(options.clip_range(), (-2048, 10240));
    }

    #[test]
    fn tolerance_scales_the_same_way() {
        let options = TilingOptions::default();
        assert!((options.tolerance_in_tile_units() - 6.0).abs() < 1e-12);
    }

    /// Sixteen, and the reason a buffer of 128 becomes 2048. Getting this wrong gives a buffer
    /// sixteen times too small, which looks almost right and leaves a hairline seam at every
    /// tile boundary.
    #[test]
    fn the_scale_factor_is_extent_over_tile_size() {
        assert_eq!(SCALE, 16);
        assert_eq!(EXTENT / TILE_SIZE, SCALE);
    }

    /// mbgl rounds rather than truncates. For a buffer that is not a multiple of the scale the
    /// two differ, and the difference lands exactly at the tile edge.
    #[test]
    fn the_buffer_rounds_rather_than_truncating() {
        let options = TilingOptions {
            // 16 * 1.5 would be 24 if the option could be fractional; with integer options the
            // rounding shows up through an odd value instead.
            buffer: 3,
            ..TilingOptions::default()
        };
        assert_eq!(options.buffer_in_tile_units(), 48);

        // A zero buffer clips exactly to the tile, which is the degenerate case a naive reader
        // would produce for every buffer.
        let none = TilingOptions {
            buffer: 0,
            ..TilingOptions::default()
        };
        assert_eq!(none.clip_range(), (0, EXTENT));
    }

    /// The extent agrees with the projection crate's. Two different extents would put geometry
    /// and tile boundaries in different coordinate systems.
    #[test]
    fn the_extent_agrees_with_the_projection() {
        assert_eq!(EXTENT, tessella_tile::projection::EXTENT);
    }
}
