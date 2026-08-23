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

use std::collections::BTreeSet;

use crate::projection;

/// The highest zoom a cover is computed at.
///
/// A structural ceiling, not a policy one: tile indices are `u32`, so beyond z31 an address
/// cannot be represented, and `1u32 << z` stops being a shift at all. Thirty leaves a level of
/// headroom below that edge and sits well above the z22-ish where tile data actually ends. The
/// per-view `maxzoom` clamp of §5.4 is the policy limit and belongs above this, not here.
pub const MAX_ZOOM: u8 = 30;

/// The most tiles a single view's cover may contain.
///
/// A bound on work, not on geography. The tile loop is quadratic in the viewport's extent, so a
/// viewport of ten million pixels asks for four hundred million tiles and the loop becomes an
/// out-of-memory rather than an answer. Real covers are tens of tiles — a 4K display at any zoom
/// is well under a hundred — so this sits three orders of magnitude above the working range and
/// only ever fires on a viewport that is wrong.
pub const MAX_TILES: usize = 4096;

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
    ///
    /// Clamped to [`MAX_ZOOM`]. A camera is not necessarily trustworthy — with DR-9 it is the
    /// consumer's ECS that owns it and the value arrives over the reverse channel — and an
    /// unclamped zoom of 40 reaches `1u32 << 40`, which is a shift overflow rather than a large
    /// cover. Saturating `as u8` turns an absurd zoom into 255 and makes that worse, not better.
    #[must_use]
    pub fn tile_zoom(&self) -> u8 {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            self.zoom.clamp(0.0, f64::from(MAX_ZOOM)).floor() as u8
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
    /// The viewport asks for more tiles than [`MAX_TILES`].
    ///
    /// Reported rather than truncated. A silently truncated cover is a map with a missing
    /// corner, and the §13.3 coverage walk would then be measuring a hole this function chose
    /// to create.
    #[error("cover of {tiles} tiles exceeds the {MAX_TILES} limit")]
    TooLarge {
        /// How many tiles were asked for.
        tiles: u64,
    },
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

    // Checked before the loop rather than inside it: the point is not to stop after four
    // thousand tiles but never to begin. Widening to u64 keeps the multiply from wrapping on the
    // very input that makes it large.
    let span_x = (x1 - x0 + 1).max(0) as u64;
    let span_y = (y1 - y0 + 1).max(0) as u64;
    let demanded = span_x.saturating_mul(span_y);
    if demanded > MAX_TILES as u64 {
        return Err(CoverError::TooLarge { tiles: demanded });
    }

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

/// A viewport sample that no tile in a cover contains.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gap {
    /// Where in the viewport, as a fraction of width and height from the top left.
    pub screen: [f64; 2],
    /// Which tile the sample landed in.
    pub tile: TileCoord,
}

/// Walks a viewport looking for points no tile in `tiles` covers (§13.3, coverage completeness).
///
/// # What a gap means
///
/// Every pixel of a map has to come from some tile. A cover that misses one leaves a hole that
/// renders as background — the symptom §13.3 calls an uncovered frame — and it is the failure a
/// zoom sweep produces, because each level change recomputes the cover from scratch and an
/// off-by-one at a boundary shows up for exactly the frames that straddle it.
///
/// # What this proves and what it does not
///
/// For a bearing of zero the cover is the bounding box of the four corners, so an interior
/// sample being inside it is close to arithmetic identity. The walker earns its place at the
/// edges and in the cases the bounding box does not decide: the `floor` at each boundary, the
/// wrap arithmetic across the antimeridian, and the fractional-zoom scale. Those are where a
/// cover goes wrong, and they are all boundary conditions a corner check passes over.
///
/// Samples outside the world vertically are not gaps. A viewport past the pole sees empty space,
/// and [`cover`] drops those rows deliberately rather than clamping them; counting them as gaps
/// would demand tiles that do not exist.
///
/// `steps` is the sample grid per axis. It should exceed the tile count across the viewport, or
/// the walk can step over a missing column entirely.
///
/// # Errors
///
/// [`CoverError::Pitched`] when the view has pitch, matching [`cover`].
pub fn coverage_gaps(
    view: &ViewTransform,
    tiles: &[TileCoord],
    steps: usize,
) -> Result<Vec<Gap>, CoverError> {
    if view.pitch.abs() > f64::EPSILON {
        return Err(CoverError::Pitched);
    }

    let z = view.tile_zoom();
    let world = f64::from(1u32 << z);
    let centre = projection::tile_units(view.longitude, view.latitude, z);
    let scale = (view.zoom - f64::from(z)).exp2();
    let half_width = view.width / (2.0 * projection::TILE_SIZE * scale);
    let half_height = view.height / (2.0 * projection::TILE_SIZE * scale);

    let present: BTreeSet<(u32, u32)> = tiles.iter().map(|t| (t.x, t.y)).collect();
    let radians = view.bearing.to_radians();
    let (sin, cos) = radians.sin_cos();

    let mut gaps = Vec::new();
    let divisor = steps.max(1) as f64;
    for row in 0..=steps.max(1) {
        for column in 0..=steps.max(1) {
            let (u, v) = (column as f64 / divisor, row as f64 / divisor);

            // The same transform the corners take, so a gap is a real disagreement rather than
            // two different ideas of where the viewport is.
            let (dx, dy) = ((u - 0.5) * 2.0 * half_width, (v - 0.5) * 2.0 * half_height);
            let x = centre[0] + (dx * cos - dy * sin);
            let y = centre[1] + (dx * sin + dy * cos);

            let row_index = y.floor();
            if row_index < 0.0 || row_index >= world {
                continue;
            }
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let (tile_x, tile_y) = (x.rem_euclid(world).floor() as u32, row_index as u32);
            if !present.contains(&(tile_x, tile_y)) {
                #[allow(clippy::cast_possible_truncation)]
                gaps.push(Gap {
                    screen: [u, v],
                    tile: TileCoord {
                        z,
                        x: tile_x,
                        y: tile_y,
                        wrap: (x / world).floor() as i32,
                    },
                });
            }
        }
    }
    Ok(gaps)
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

    /// The probe's own cover leaves no hole in the probe's own viewport.
    #[test]
    fn a_cover_covers_the_view_it_was_computed_for() {
        let view = probe_view();
        let tiles = cover(&view).expect("covers");
        let gaps = coverage_gaps(&view, &tiles, 64).expect("walks");
        assert!(gaps.is_empty(), "{gaps:?}");
    }

    /// And the walker finds a hole when there is one, which is what makes the test above mean
    /// something. A walker that returned empty unconditionally would pass every sweep frame.
    #[test]
    fn the_walker_detects_a_missing_tile() {
        let view = probe_view();
        let full = cover(&view).expect("covers");
        let punctured: Vec<TileCoord> = full
            .iter()
            .copied()
            .filter(|t| !(t.x == 4093 && t.y == 2724))
            .collect();

        let gaps = coverage_gaps(&view, &punctured, 64).expect("walks");
        assert!(!gaps.is_empty(), "a removed tile is a hole");
        assert!(
            gaps.iter().all(|g| g.tile.x == 4093 && g.tile.y == 2724),
            "and the hole names the tile that is missing: {gaps:?}"
        );
    }

    /// Fractional zoom is where a cover most easily comes up short, because the viewport is
    /// sized in tiles that are no longer 512 pixels on screen. Ignoring the fraction would under-
    /// or over-fetch, and the under-fetch half of that is a hole.
    #[test]
    fn fractional_zoom_leaves_no_hole() {
        for tenths in 0..10 {
            let mut view = probe_view();
            view.zoom = 13.0 + f64::from(tenths) / 10.0;
            let tiles = cover(&view).expect("covers");
            let gaps = coverage_gaps(&view, &tiles, 64).expect("walks");
            assert!(gaps.is_empty(), "at zoom {}: {gaps:?}", view.zoom);
        }
    }

    /// A rotated viewport is covered by the bounding box of its rotated corners, at every
    /// bearing rather than the axis-aligned ones that happen to be easy.
    #[test]
    fn every_bearing_leaves_no_hole() {
        for degrees in (0..360).step_by(15) {
            let mut view = probe_view();
            view.bearing = f64::from(degrees);
            let tiles = cover(&view).expect("covers");
            let gaps = coverage_gaps(&view, &tiles, 64).expect("walks");
            assert!(gaps.is_empty(), "at bearing {degrees}: {gaps:?}");
        }
    }

    /// The antimeridian, where the wrap arithmetic decides whether the right half of the screen
    /// is covered or blank.
    #[test]
    fn a_view_across_the_antimeridian_leaves_no_hole() {
        let mut view = probe_view();
        view.longitude = 179.98;
        view.zoom = 13.0;
        let tiles = cover(&view).expect("covers");
        let gaps = coverage_gaps(&view, &tiles, 64).expect("walks");
        assert!(gaps.is_empty(), "{gaps:?}");
        assert!(
            tiles.iter().any(|t| t.wrap != 0),
            "and it genuinely straddles: {tiles:?}"
        );
    }

    /// A view over the pole sees empty space above it, not tiles. Those rows are dropped, and
    /// the walker must agree that dropped is correct rather than reporting the sky as a hole.
    #[test]
    fn space_beyond_the_pole_is_not_a_hole() {
        let view = ViewTransform {
            latitude: 85.0,
            zoom: 2.0,
            ..probe_view()
        };
        let tiles = cover(&view).expect("covers");
        let gaps = coverage_gaps(&view, &tiles, 64).expect("walks");
        assert!(gaps.is_empty(), "{gaps:?}");
        assert!(tiles.iter().all(|t| t.y < 4), "no row outside the world");
    }

    #[test]
    fn a_pitched_walk_is_refused_like_a_pitched_cover() {
        let view = ViewTransform {
            pitch: 30.0,
            ..probe_view()
        };
        assert_eq!(coverage_gaps(&view, &[], 8), Err(CoverError::Pitched));
    }

    /// An absurd zoom is clamped, not shifted. `1u32 << 40` is a shift overflow rather than a
    /// large number, and with DR-9 the camera arrives from the consumer rather than from here.
    #[test]
    fn an_absurd_zoom_is_clamped_rather_than_shifted() {
        for zoom in [40.0, 255.0, 1e9, f64::INFINITY] {
            let view = ViewTransform {
                zoom,
                ..probe_view()
            };
            assert_eq!(view.tile_zoom(), MAX_ZOOM, "at zoom {zoom}");
            // The point of the clamp: this call must return rather than panic.
            let tiles = cover(&view);
            assert!(tiles.is_ok() || tiles == Err(CoverError::TooLarge { tiles: 0 }));
        }
    }

    /// Negative and non-finite zooms land at the bottom of the range instead of wrapping through
    /// the `as u8` cast, which would turn -1.0 into 0 but 1e9 into 255.
    #[test]
    fn zoom_below_the_range_is_clamped_too() {
        for zoom in [-1.0, -1e9, f64::NEG_INFINITY] {
            let view = ViewTransform {
                zoom,
                ..probe_view()
            };
            assert_eq!(view.tile_zoom(), 0, "at zoom {zoom}");
        }
        // NaN has no ordering, so `clamp` is not defined on it; `tile_zoom` must still produce a
        // usable level rather than an arbitrary one.
        let nan = ViewTransform {
            zoom: f64::NAN,
            ..probe_view()
        };
        assert!(nan.tile_zoom() <= MAX_ZOOM);
    }

    /// A viewport large enough to ask for hundreds of millions of tiles is refused before the
    /// loop starts, rather than answered by exhausting memory.
    #[test]
    fn an_enormous_viewport_is_refused_not_enumerated() {
        let view = ViewTransform {
            width: 1e7,
            height: 1e7,
            zoom: 0.0,
            ..probe_view()
        };
        match cover(&view) {
            Err(CoverError::TooLarge { tiles }) => {
                assert!(tiles > MAX_TILES as u64, "{tiles}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// And the limit does not fire on anything real. A 4K viewport at every level of the §13.3
    /// sweep stays far below it, so the bound is a guard rather than a ceiling on use.
    #[test]
    fn a_real_viewport_is_nowhere_near_the_limit() {
        for zoom in 0..=22 {
            let view = ViewTransform {
                width: 3840.0,
                height: 2160.0,
                zoom: f64::from(zoom),
                ..probe_view()
            };
            let tiles = cover(&view).expect("a 4K viewport covers");
            assert!(
                tiles.len() < MAX_TILES / 32,
                "z{zoom} wanted {} tiles",
                tiles.len()
            );
        }
    }
}
