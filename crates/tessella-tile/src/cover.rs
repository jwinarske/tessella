//! Which tiles a view needs (§5.4, §12.7).
//!
//! # Cover is per view, the store beneath it is not
//!
//! §5.2 lists cover decisions as irreducibly per-view: they are a function of this view's
//! transform, and two views at different places want different tiles. §5.5 lists the store
//! beneath them as process-scoped. Both are true at once, and the combination is the point —
//! four views compute four covers and share whatever those covers overlap on.
//!
//! # Checked against the oracle
//!
//! The probe sits at 51.505, -0.11 with a 1024x768 viewport at zoom 13, and the golden dump
//! covers x in 4092..=4094 and y in 2723..=2724. A 1024-pixel viewport is two 512-pixel tiles
//! wide and a 768-pixel one is one and a half tall, so the camera's tile plus a half-tile margin
//! each way lands on exactly that 3x2 set. The test asserts the set, not the count: a cover
//! that was the right size in the wrong place would pass a count.
//!
//! # What is exact and what is not
//!
//! With no pitch, the visible region is a rectangle in tile space — rotated by the bearing, but
//! still flat — so its corners project exactly and the cover is the bounding box of those four
//! points. That is what this computes, and for bearing zero it is not merely a bound but the
//! answer.
//!
//! Pitch is different and deliberately refused. A pitched view sees a trapezoid whose far edge
//! recedes toward the horizon, and its tile footprint is unbounded as pitch approaches ninety
//! degrees — mbgl clamps it with a far-plane distance derived from the field of view. Treating a
//! pitched view as its bounding rectangle would cover a vast area cheaply and wrongly, fetching
//! tiles that are never drawn, so [`cover`] reports the limitation rather than guessing at it.

use crate::projection;

/// A tile address in a cover.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TileCoord {
    /// Zoom.
    pub z: u8,
    /// Column.
    pub x: u32,
    /// Row.
    pub y: u32,
    /// World copy, for a viewport crossing the antimeridian.
    pub wrap: i32,
}

/// The view a cover is computed for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewTransform {
    /// Centre longitude.
    pub longitude: f64,
    /// Centre latitude.
    pub latitude: f64,
    /// Fractional zoom.
    pub zoom: f64,
    /// Viewport width in pixels.
    pub width: f64,
    /// Viewport height in pixels.
    pub height: f64,
    /// Bearing in degrees, clockwise from north.
    pub bearing: f64,
    /// Pitch in degrees from straight down.
    pub pitch: f64,
}

impl ViewTransform {
    /// The integer zoom a cover is computed at.
    ///
    /// Floor, not round: a view at zoom 13.9 draws z13 tiles scaled up, because the next level
    /// does not exist yet at that fraction. Rounding would fetch z14 for the top tenth of every
    /// level and throw the work away on the way back down.
    #[must_use]
    pub fn tile_zoom(&self) -> u8 {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            self.zoom.max(0.0).floor() as u8
        }
    }
}

/// A cover that could not be computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CoverError {
    /// The view is pitched, which needs a frustum this does not compute.
    ///
    /// A pitched view sees a trapezoid receding toward the horizon whose footprint is unbounded
    /// as pitch approaches ninety degrees. Its bounding rectangle would be cheap to compute and
    /// would fetch a great many tiles that are never drawn.
    #[error("pitched views need a frustum cover, which is not implemented")]
    Pitched,
}

/// The tiles a view needs at its integer zoom.
///
/// Returned sorted, so two runs of the same transform produce the same list and a diff against
/// the oracle's cover is a set comparison rather than an ordering puzzle.
///
/// # Errors
///
/// [`CoverError::Pitched`] when the view has pitch.
pub fn cover(view: &ViewTransform) -> Result<Vec<TileCoord>, CoverError> {
    if view.pitch.abs() > f64::EPSILON {
        return Err(CoverError::Pitched);
    }

    let z = view.tile_zoom();
    let world = f64::from(1u32 << z);

    // The centre in tile units at the *fractional* zoom, then rescaled to the integer one. Using
    // the integer zoom directly would place the centre correctly but size the viewport wrongly,
    // because half a zoom level is a factor of √2 in tiles across.
    let centre = projection::tile_units(view.longitude, view.latitude, z);
    let scale = (view.zoom - f64::from(z)).exp2();

    // Half the viewport in tiles at this level. A 512-pixel tile is the unit, and `scale`
    // accounts for the fractional part: at zoom 13.5 each z13 tile covers √2 times more screen,
    // so fewer of them fit.
    let half_width = view.width / (2.0 * projection::TILE_SIZE * scale);
    let half_height = view.height / (2.0 * projection::TILE_SIZE * scale);

    let corners = rotated_corners(half_width, half_height, view.bearing);
    let (mut min_x, mut max_x) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut min_y, mut max_y) = (f64::INFINITY, f64::NEG_INFINITY);
    for [dx, dy] in corners {
        min_x = min_x.min(centre[0] + dx);
        max_x = max_x.max(centre[0] + dx);
        min_y = min_y.min(centre[1] + dy);
        max_y = max_y.max(centre[1] + dy);
    }

    let mut tiles = Vec::new();
    #[allow(clippy::cast_possible_truncation)]
    let (x0, x1) = (min_x.floor() as i64, max_x.floor() as i64);
    #[allow(clippy::cast_possible_truncation)]
    let (y0, y1) = (min_y.floor() as i64, max_y.floor() as i64);

    for y in y0..=y1 {
        // Latitude does not wrap. A viewport extending past the pole sees empty space there,
        // not the other side of the world, so those rows are dropped rather than clamped —
        // clamping would draw the polar row repeatedly down the screen.
        if y < 0 || y >= world as i64 {
            continue;
        }
        for x in x0..=x1 {
            // Longitude does wrap, and which world copy a tile belongs to is what `wrap`
            // records. The same tile in two copies is two entries with one shared store key,
            // which is how a viewport straddling the antimeridian draws both sides from one
            // fetch.
            let wrap = (x as f64 / world).floor();
            #[allow(clippy::cast_possible_truncation)]
            let wrapped = x - (wrap as i64) * (world as i64);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            tiles.push(TileCoord {
                z,
                x: wrapped as u32,
                y: y as u32,
                wrap: wrap as i32,
            });
        }
    }

    tiles.sort_unstable();
    Ok(tiles)
}

/// The four corners of a half-extent rectangle, rotated by a bearing.
fn rotated_corners(half_width: f64, half_height: f64, bearing: f64) -> [[f64; 2]; 4] {
    let radians = bearing.to_radians();
    let (sin, cos) = radians.sin_cos();
    let corners = [
        [-half_width, -half_height],
        [half_width, -half_height],
        [half_width, half_height],
        [-half_width, half_height],
    ];
    corners.map(|[x, y]| [x * cos - y * sin, x * sin + y * cos])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe_view() -> ViewTransform {
        ViewTransform {
            longitude: -0.11,
            latitude: 51.505,
            zoom: 13.0,
            width: 1024.0,
            height: 768.0,
            bearing: 0.0,
            pitch: 0.0,
        }
    }

    /// The oracle's cover, exactly. The set, not the count: a cover of the right size in the
    /// wrong place would pass a count.
    #[test]
    fn the_probe_view_covers_what_the_oracle_covers() {
        let tiles = cover(&probe_view()).expect("covers");
        let expected: Vec<TileCoord> = [
            (4092, 2723),
            (4092, 2724),
            (4093, 2723),
            (4093, 2724),
            (4094, 2723),
            (4094, 2724),
        ]
        .into_iter()
        .map(|(x, y)| TileCoord {
            z: 13,
            x,
            y,
            wrap: 0,
        })
        .collect();

        assert_eq!(tiles, expected);
    }

    /// A viewport one tile across still touches four tiles unless its centre sits exactly on a
    /// tile corner, because half a tile each way straddles a boundary on both axes.
    ///
    /// The probe's centre is at 4093.497, 2724.137 in tile units, so half a tile each way
    /// reaches back into 4092 and up into 2723. Assuming a one-tile viewport needs one tile is
    /// the intuitive error, and it under-fetches three quarters of the screen.
    #[test]
    fn a_one_tile_viewport_still_straddles_boundaries() {
        let mut view = probe_view();
        view.width = 512.0;
        view.height = 512.0;
        let tiles = cover(&view).expect("covers");

        assert_eq!(tiles.len(), 4, "two columns, two rows: {tiles:?}");
        assert!(tiles.iter().any(|t| t.x == 4092) && tiles.iter().any(|t| t.x == 4093));
        assert!(tiles.iter().any(|t| t.y == 2723) && tiles.iter().any(|t| t.y == 2724));

        // The centre's own tile is in there, which is the sanity check that catches a cover
        // computed in the right shape at the wrong offset.
        assert!(tiles.iter().any(|t| t.x == 4093 && t.y == 2724));
    }

    /// Zoom is floored, not rounded. A view at 13.9 draws z13 tiles scaled up, because z14 does
    /// not exist for it yet; rounding would fetch the next level for the top tenth of every
    /// level and discard it on the way back down.
    #[test]
    fn the_tile_zoom_is_floored() {
        let mut view = probe_view();
        for (zoom, expected) in [(13.0, 13), (13.4, 13), (13.9, 13), (14.0, 14)] {
            view.zoom = zoom;
            assert_eq!(view.tile_zoom(), expected, "at zoom {zoom}");
        }
    }

    /// The fractional part sizes the viewport. At zoom 13.5 each z13 tile covers more screen, so
    /// fewer of them fit — ignoring the fraction would over-fetch by up to a factor of two in
    /// area at the top of every level.
    #[test]
    fn a_fractional_zoom_narrows_the_cover() {
        let mut view = probe_view();
        let at_13 = cover(&view).expect("covers").len();

        view.zoom = 13.99;
        let near_14 = cover(&view).expect("covers").len();
        assert!(
            near_14 <= at_13,
            "zoomed in, so fewer z13 tiles fit: {near_14} vs {at_13}"
        );
    }

    /// A rotated view covers at least as much as an unrotated one, because a rotated rectangle's
    /// bounding box is larger. Forty-five degrees is the worst case.
    #[test]
    fn bearing_widens_the_cover() {
        let mut view = probe_view();
        let square = cover(&view).expect("covers").len();

        view.bearing = 45.0;
        let rotated = cover(&view).expect("covers").len();
        assert!(rotated >= square, "{rotated} vs {square}");

        // And a full turn is the same as none.
        view.bearing = 360.0;
        assert_eq!(cover(&view).expect("covers").len(), square);
    }

    /// Pitch is refused rather than approximated. A pitched view sees a trapezoid receding to the
    /// horizon whose footprint is unbounded near ninety degrees, and its bounding rectangle would
    /// fetch a great many tiles that are never drawn.
    #[test]
    fn pitch_is_refused() {
        let mut view = probe_view();
        view.pitch = 30.0;
        assert_eq!(cover(&view), Err(CoverError::Pitched));

        view.pitch = 0.0;
        assert!(cover(&view).is_ok());
    }

    /// Longitude wraps and latitude does not. A viewport past the antimeridian sees the other
    /// side of the world; one past the pole sees empty space, and clamping instead would draw
    /// the polar row repeatedly down the screen.
    #[test]
    fn longitude_wraps_and_latitude_clips() {
        let view = ViewTransform {
            longitude: 179.9,
            latitude: 0.0,
            zoom: 2.0,
            width: 1024.0,
            height: 512.0,
            bearing: 0.0,
            pitch: 0.0,
        };
        let tiles = cover(&view).expect("covers");
        assert!(
            tiles.iter().any(|t| t.wrap != 0),
            "the viewport crosses into the next world copy: {tiles:?}"
        );
        assert!(
            tiles.iter().all(|t| t.x < 4),
            "and every x is a real tile index at z2"
        );

        // At the pole, rows above the world are dropped rather than clamped.
        let polar = ViewTransform {
            latitude: 85.0,
            longitude: 0.0,
            ..view
        };
        let tiles = cover(&polar).expect("covers");
        assert!(tiles.iter().all(|t| t.y < 4), "{tiles:?}");
        assert!(!tiles.is_empty());
    }

    /// Two views at the same place produce the same cover, which is the precondition for the
    /// store sharing anything: covers that differed run to run would share nothing.
    #[test]
    fn the_same_transform_gives_the_same_cover() {
        assert_eq!(
            cover(&probe_view()).expect("covers"),
            cover(&probe_view()).expect("covers")
        );
    }

    /// Two views at different places overlap partially, which is the case §5.5's flatness
    /// counters are about: the shared part is built once and the rest is per view.
    #[test]
    fn nearby_views_overlap_partially() {
        let left = probe_view();
        let mut right = probe_view();
        right.longitude = -0.05;

        let a = cover(&left).expect("covers");
        let b = cover(&right).expect("covers");
        let shared = a.iter().filter(|tile| b.contains(tile)).count();

        assert!(shared > 0, "they overlap");
        assert!(shared < a.len(), "but are not identical");
    }
}
