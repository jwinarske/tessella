//! Turning a tile id into the URL that fetches it.
//!
//! Transcribed from mbgl's `Resource::tile` and `util::replaceTokens`
//! (`storage/resource.cpp`, `util/token.hpp`). Pure: no I/O, no source state beyond what a
//! `TileSource` already carries.
//!
//! # An unknown token is left alone, braces and all
//!
//! `replaceTokens` puts `{whatever}` back verbatim when the lookup declines. That is not
//! leniency for its own sake — a tile URL may legitimately contain braces that are not tokens,
//! and dropping them would produce a URL that 404s with no clue why. So an unrecognised token
//! survives into the request and shows up in the log as itself.
//!
//! # TMS flips y, and it flips it against the *tile* zoom
//!
//! A TMS source numbers rows from the south. The flip is `(1 << z) - y - 1` at the zoom the
//! tile is fetched at, which for an overscaled tile is its own zoom and not the zoom it is
//! displayed at — using the display zoom would ask for a row that does not exist.

use std::fmt::Write as _;

/// Which way a source numbers its rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scheme {
    /// Rows from the north, which is what a style means when it says nothing.
    #[default]
    Xyz,
    /// Rows from the south.
    Tms,
}

impl Scheme {
    /// Parses the `scheme` field of a TileJSON or an inline source.
    ///
    /// Anything other than `tms` is `xyz`, including nonsense: the spec has two values and a
    /// source that writes a third has not opted out of being drawn.
    #[must_use]
    pub fn parse(text: Option<&str>) -> Self {
        match text {
            Some("tms") => Self::Tms,
            _ => Self::Xyz,
        }
    }
}

/// Expands a tile URL template.
///
/// `pixel_ratio` selects the `{ratio}` suffix — `@2x` above one, empty at or below it — which
/// is the only token that depends on anything but the tile.
#[must_use]
pub fn expand(template: &str, z: u8, x: u32, y: u32, scheme: Scheme, pixel_ratio: f32) -> String {
    let y = match scheme {
        Scheme::Xyz => y,
        Scheme::Tms => (1u32 << z).saturating_sub(y).saturating_sub(1),
    };

    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        rest = &rest[open..];

        // A token runs to the next brace of either kind. An unclosed `{`, or one containing
        // another `{`, is not a token at all and is copied through.
        let Some(close) = rest[1..].find(['{', '}']).map(|index| index + 1) else {
            break;
        };
        if rest.as_bytes()[close] != b'}' {
            out.push_str(&rest[..close]);
            rest = &rest[close..];
            continue;
        }

        let token = &rest[1..close];
        match token_value(token, z, x, y, pixel_ratio) {
            Some(value) => out.push_str(&value),
            // Put it back exactly as written, braces included.
            None => {
                out.push('{');
                out.push_str(token);
                out.push('}');
            }
        }
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    out
}

fn token_value(token: &str, z: u8, x: u32, y: u32, pixel_ratio: f32) -> Option<String> {
    match token {
        "z" => Some(z.to_string()),
        "x" => Some(x.to_string()),
        "y" => Some(y.to_string()),
        "quadkey" => Some(quadkey(z, x, y)),
        "bbox-epsg-3857" => Some(tile_bbox(z, x, y)),
        // A two-character shard, so a request set spreads over sixteen paths.
        "prefix" => {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            Some(String::from_utf8_lossy(&[HEX[(x % 16) as usize], HEX[(y % 16) as usize]]).into())
        }
        "ratio" => Some(if pixel_ratio > 1.0 { "@2x" } else { "" }.to_string()),
        _ => None,
    }
}

/// The Bing-style quadkey for a tile: one base-4 digit per zoom level, most significant first.
#[must_use]
pub fn quadkey(z: u8, x: u32, y: u32) -> String {
    let mut out = String::with_capacity(z as usize);
    for level in (1..=z).rev() {
        let mask = 1u32 << (level - 1);
        let digit = u32::from(x & mask != 0) + 2 * u32::from(y & mask != 0);
        out.push(char::from(b'0' + digit as u8));
    }
    out
}

/// Half the equator, in metres — the Mercator plane's half-extent.
const HALF_EQUATOR: f64 = std::f64::consts::PI * 6_378_137.0;

/// A tile's bounds in EPSG:3857, as `minx,miny,maxx,maxy`.
///
/// The y is flipped first, because this token is defined against a south-origin plane whatever
/// the source's own scheme is. mbgl computes the corners from *pixel* coordinates at 256 per
/// tile rather than from the tile index directly; the two agree, and the pixel form is kept so
/// the correspondence is visible.
#[must_use]
pub fn tile_bbox(z: u8, x: u32, y: u32) -> String {
    let flipped = f64::from((1u32 << z).saturating_sub(y).saturating_sub(1));
    let resolution = (2.0 * HALF_EQUATOR / 256.0) / f64::from(1u32 << z);
    let corner = |px: f64| px * 256.0 * resolution - HALF_EQUATOR;

    let (min_x, min_y) = (corner(f64::from(x)), corner(flipped));
    let (max_x, max_y) = (corner(f64::from(x) + 1.0), corner(flipped + 1.0));

    let mut out = String::new();
    write!(out, "{min_x},{min_y},{max_x},{max_y}").expect("writing to a String cannot fail");
    out
}

/// The zooms a source provides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZoomRange {
    /// Below this the source has nothing.
    pub min: u8,
    /// At and above this, tiles are reused and overscaled rather than fetched deeper.
    pub max: u8,
}

impl Default for ZoomRange {
    /// The style spec's defaults: `minzoom` 0 and `maxzoom` 22.
    fn default() -> Self {
        Self { min: 0, max: 22 }
    }
}

/// Which zoom's tiles to fetch for a view that wants `display_zoom`.
///
/// `None` below the source's minimum — there is nothing to draw, which is different from
/// drawing the source's coarsest tile stretched over the world.
///
/// Above the maximum the answer is the maximum: the tile is fetched once and *used* at the
/// deeper zoom, which is what overscaling is. The pair `(fetch zoom, display zoom)` is exactly
/// `TileId::overscaled`'s two arguments, and it is why a bucket's identity carries both — a
/// zoom-varying paint property evaluates against the display zoom while the geometry comes
/// from the fetch zoom.
#[must_use]
pub fn fetch_zoom(display_zoom: u8, range: ZoomRange) -> Option<u8> {
    if display_zoom < range.min {
        return None;
    }
    Some(display_zoom.min(range.max))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ordinary_tokens_expand() {
        assert_eq!(
            expand(
                "https://host/{z}/{x}/{y}.pbf",
                13,
                4093,
                2724,
                Scheme::Xyz,
                1.0
            ),
            "https://host/13/4093/2724.pbf"
        );
    }

    /// TMS numbers rows from the south, so the y is mirrored within the zoom.
    #[test]
    fn tms_flips_the_row() {
        assert_eq!(
            expand("{z}/{x}/{y}", 2, 1, 0, Scheme::Tms, 1.0),
            "2/1/3",
            "four rows at zoom two"
        );
        assert_eq!(expand("{z}/{x}/{y}", 0, 0, 0, Scheme::Tms, 1.0), "0/0/0");
    }

    /// An unknown token survives with its braces, rather than vanishing into a URL that 404s
    /// for no visible reason.
    #[test]
    fn an_unknown_token_is_left_alone() {
        assert_eq!(
            expand("host/{z}/{nope}/{y}", 1, 2, 3, Scheme::Xyz, 1.0),
            "host/1/{nope}/3"
        );
    }

    /// Braces that are not a token are copied through: an unclosed one, and a nested one.
    #[test]
    fn malformed_braces_are_copied_through() {
        assert_eq!(expand("host/{z", 5, 0, 0, Scheme::Xyz, 1.0), "host/{z");
        assert_eq!(expand("host/{a{z}", 5, 0, 0, Scheme::Xyz, 1.0), "host/{a5");
        assert_eq!(expand("host/}{z}", 5, 0, 0, Scheme::Xyz, 1.0), "host/}5");
        assert_eq!(expand("no tokens", 5, 0, 0, Scheme::Xyz, 1.0), "no tokens");
    }

    /// `{ratio}` is the only token that reads anything but the tile.
    #[test]
    fn the_ratio_token_follows_the_pixel_ratio() {
        assert_eq!(expand("t{ratio}.png", 0, 0, 0, Scheme::Xyz, 1.0), "t.png");
        assert_eq!(
            expand("t{ratio}.png", 0, 0, 0, Scheme::Xyz, 2.0),
            "t@2x.png"
        );
        // Strictly greater than one, so 1.5 is still the single-density asset.
        assert_eq!(
            expand("t{ratio}.png", 0, 0, 0, Scheme::Xyz, 1.5),
            "t@2x.png"
        );
    }

    /// The quadkey is one base-4 digit per level, most significant first.
    #[test]
    fn quadkeys_match_the_bing_scheme() {
        assert_eq!(quadkey(0, 0, 0), "");
        assert_eq!(quadkey(1, 0, 0), "0");
        assert_eq!(quadkey(1, 1, 0), "1");
        assert_eq!(quadkey(1, 0, 1), "2");
        assert_eq!(quadkey(1, 1, 1), "3");
        assert_eq!(quadkey(3, 3, 5), "213");
    }

    /// The whole world at zoom zero, and the four quadrants at zoom one.
    #[test]
    fn the_bbox_token_spans_the_mercator_plane() {
        let world = tile_bbox(0, 0, 0);
        let parts: Vec<f64> = world
            .split(',')
            .map(|p| p.parse().expect("a number"))
            .collect();
        assert!((parts[0] + HALF_EQUATOR).abs() < 1e-6, "{world}");
        assert!((parts[3] - HALF_EQUATOR).abs() < 1e-6, "{world}");

        // The north-west tile at zoom one is the upper-left quadrant.
        let nw: Vec<f64> = tile_bbox(1, 0, 0)
            .split(',')
            .map(|p| p.parse().expect("a number"))
            .collect();
        assert!((nw[0] + HALF_EQUATOR).abs() < 1e-6);
        assert!(nw[1].abs() < 1e-6, "its south edge is the equator");
        assert!(nw[2].abs() < 1e-6, "its east edge is the meridian");
        assert!((nw[3] - HALF_EQUATOR).abs() < 1e-6);
    }

    /// Below the minimum there is nothing; above the maximum the deepest tile is reused.
    #[test]
    fn the_fetch_zoom_clamps_to_the_source() {
        let range = ZoomRange { min: 4, max: 14 };
        assert_eq!(fetch_zoom(3, range), None);
        assert_eq!(fetch_zoom(4, range), Some(4));
        assert_eq!(fetch_zoom(10, range), Some(10));
        assert_eq!(fetch_zoom(14, range), Some(14));
        assert_eq!(fetch_zoom(18, range), Some(14), "overscaled, not absent");
    }

    /// The spec's defaults let every zoom through.
    #[test]
    fn the_default_range_is_the_specs() {
        assert_eq!(ZoomRange::default(), ZoomRange { min: 0, max: 22 });
        assert_eq!(fetch_zoom(0, ZoomRange::default()), Some(0));
        assert_eq!(fetch_zoom(22, ZoomRange::default()), Some(22));
    }
}
