// SPDX-License-Identifier: BSD-2-Clause
//! A DEM's slope field and a color relief's ramp, sized against the oracle.
//!
//! # What the oracle gives
//!
//! `tests/golden/relief_style.dump`: one synthetic DEM tile under a `color-relief` and a
//! `hillshade`, both reading the same source. The interesting lines are not drawables — each
//! layer draws a single quad, four vertices and six indices — but the textures around them:
//!
//! ```text
//! textures 5
//! texture 5x1 fmt=0 hash=3baf2439cead5d54
//! texture 5x1 fmt=0 hash=cbf29ce484222325
//! texture 258x258 fmt=0 hash=7bc2afdc5ce1fbec
//! rendertargets 2
//! rendertarget 256x256 ct=0
//! ```
//!
//! Two of those are shapes this build has to agree with, and the third is a divergence worth
//! pinning rather than fixing.
//!
//! - **5x1, twice.** The ramp in this style has five stops, and mbgl carries them as two one-row
//!   textures — the elevations in one, the colors in the other — rather than as uniforms. This
//!   build does the same, and the widths have to track the style's stop count.
//! - **258x258.** A 256-pixel DEM is stored one pixel wider on every side, because a slope needs
//!   its neighbors and the tile's own edge has none. Both builds store that border. What mbgl
//!   *uploads* is the bordered DEM itself.
//! - **256x256 render targets, two of them.** That is where mbgl differentiates: it uploads the
//!   raw DEM and computes the slope in an offscreen prepare pass.
//!
//! # The divergence
//!
//! This build differentiates on the CPU. `Dem::prepare` returns the finished slope field at the
//! tile's own size, so the texture that reaches the stream is **256x256 and there is no prepare
//! pass at all** — two render targets per hillshade tile that mbgl allocates and this does not.
//!
//! That is a deliberate difference and the numbers here are what makes it visible rather than
//! folklore. It is also why a pixel comparison cannot referee this family: both roads arrive at
//! the same shading, and the stream shapes on the way there are not the same.
//!
//! # Why a capture was needed for this
//!
//! The raster-dem family had no dump coverage at all. Its whole surface is textures and an
//! offscreen pass, which is exactly the part of the stream pixels are worst at reporting on: a
//! border filled wrongly, or a ramp table a row short, shades slightly differently everywhere and
//! identifiably nowhere.
//!
//! Gated on `image` because decoding the fixture's PNG is. CI runs `--all-features`.

#![cfg(feature = "image")]

use tessella_source::dem::{Dem, Encoding};
use tessella_style::Style;

const DEM: &[u8] = include_bytes!("../../../tests/dem-fixtures/8-127-85.png");
const DUMP: &str = include_str!("../../../tests/golden/relief_style.dump");
const STYLE: &str = include_str!("../../tessella-style/tests/relief_style.json");

/// Every `texture WxH` the capture recorded, as `(width, height)`.
fn oracle_textures() -> Vec<(u32, u32)> {
    DUMP.lines()
        .filter_map(|line| line.strip_prefix("texture "))
        .filter_map(|rest| rest.split_whitespace().next())
        .filter_map(|size| {
            let (w, h) = size.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .collect()
}

/// Every `texture ... hash=` the capture recorded, as `WxH` against the hash.
fn oracle_hashes() -> Vec<((u32, u32), u64)> {
    DUMP.lines()
        .filter_map(|line| line.strip_prefix("texture "))
        .filter_map(|rest| {
            let mut parts = rest.split_whitespace();
            let (w, h) = parts.next()?.split_once('x')?;
            let hash = parts.find_map(|part| part.strip_prefix("hash="))?;
            Some((
                (w.parse().ok()?, h.parse().ok()?),
                u64::from_str_radix(hash, 16).ok()?,
            ))
        })
        .collect()
}

/// FNV-1a's offset basis, which is also what the probe reports for a texture it saw no bytes for.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a, as the probe hashes texture bytes.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = FNV_OFFSET;
    for byte in bytes {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The DEM keeps the border mbgl uploads, and the slope field does not need it.
///
/// mbgl's `texture 258x258` is the bordered DEM going to the GPU to be differentiated there.
/// This build stores the same border — `stride` is `dim + 2` — and differentiates on the CPU, so
/// what it sends is the slope field at the tile's own size. The border is consumed rather than
/// uploaded, which is the whole of the difference.
#[test]
fn the_dem_keeps_its_border_and_the_slope_field_does_not_need_it() {
    let image = tessella_source::image::decode(DEM).expect("the fixture decodes");
    assert_eq!(
        (image.width, image.height),
        (256, 256),
        "the fixture is a 256-pixel tile"
    );

    let dem = Dem::new(&image, Encoding::Mapbox).expect("the DEM reads");
    assert_eq!(dem.dim(), 256, "the tile's own width");
    assert_eq!(
        dem.stride(),
        258,
        "stored one pixel wider each way, which is what the oracle uploads"
    );
    assert!(
        oracle_textures().contains(&(258, 258)),
        "the oracle uploads the bordered DEM: {:?}",
        oracle_textures()
    );

    // The border is real: a pixel outside the tile reads, because `new` filled it.
    assert!(
        dem.elevation(-1, -1).is_some(),
        "the border should be filled rather than absent"
    );

    let field = dem.prepare(8);
    assert_eq!(
        (field.width, field.height),
        (256, 256),
        "the slope field is the tile's own size — the border was consumed computing it"
    );
}

/// The ramp is one row per table, as wide as the style has stops.
///
/// Two of them — elevations and colors — which is why the capture shows `5x1` twice for a
/// five-stop ramp.
#[test]
fn the_ramp_is_one_row_per_table() {
    let style = Style::parse(STYLE).expect("the style parses");
    let layer = style.layer("relief").expect("the relief layer");
    let paint = tessella_style::property::resolve_paint(layer).expect("the paint resolves");
    let ramp = paint
        .get("color-relief-color")
        .map(|property| tessella_style::ramp::relief_ramp(&property.expression))
        .expect("the layer names a ramp")
        .expect("the ramp evaluates");

    assert_eq!(ramp.len(), 5, "the fixture's ramp has five stops");
    assert_eq!(
        ramp.elevations.len(),
        ramp.colors.len(),
        "a stop is an elevation and a color"
    );

    let rows: Vec<(u32, u32)> = oracle_textures()
        .into_iter()
        .filter(|&(_, h)| h == 1)
        .collect();
    #[allow(clippy::cast_possible_truncation)]
    let width = ramp.len() as u32;
    assert!(
        rows.iter().filter(|&&(w, _)| w == width).count() >= 2,
        "the oracle should carry two {width}-wide rows for this ramp: {rows:?}"
    );
}

/// mbgl prepares a hillshade offscreen, at the *tile's* size; this build allocates no target at
/// all.
///
/// Recorded rather than asserted against this build, because there is nothing here to compare it
/// to — that is the point. If the CPU differentiation is ever traded for a prepare pass, this is
/// the line that says what mbgl's costs.
///
/// # Why the size and not the count
///
/// This asserted exactly two targets until tessella#278, and two was incidental: mbgl allocates one
/// per hillshade tile it prepares, so the number tracks how far the cover had refined when the
/// capture was taken. That is a race, and it is visible — 200 captures of this style produced 3
/// targets in 7 of them, where a z8 DEM tile appeared overscaled to three levels rather than two.
/// A regeneration landing one of those would have failed this test, and the obvious repair would
/// have been to expect three, pinning an unsettled cover as the answer.
///
/// The size is what the test is actually about: a prepare pass is 256x256 because it is the tile's
/// own, not the 1024x768 viewport's. That holds in every capture measured.
#[test]
fn the_oracle_prepares_offscreen_and_this_build_does_not() {
    let targets: Vec<&str> = DUMP
        .lines()
        .filter_map(|line| line.strip_prefix("rendertarget "))
        .collect();
    assert!(
        !targets.is_empty(),
        "the oracle should prepare a hillshade offscreen"
    );
    for target in &targets {
        assert!(
            target.starts_with("256x256"),
            "a prepare pass is the tile's size, not the viewport's: {target}"
        );
    }
}

/// The color table's texels match the oracle's; the elevation table's cannot be compared.
///
/// `the_ramp_is_one_row_per_table` checks the two tables' *width* and nothing about their contents,
/// so a ramp with the stops in the wrong order, or colors narrowed with the wrong rounding, would
/// pass it (tessella#286). The capture records an FNV-1a per texture, so the color table is
/// comparable -- and it agrees exactly.
///
/// # One of the two 5x1 textures carries no bytes
///
/// The capture's second `5x1` hashes to `cbf29ce484222325`, which is FNV-1a's offset basis: the
/// probe's `hash()` leaves the seed untouched when it is handed a null pointer, so that texture
/// reached `onTextureUpdate` with no pixels. That is the *elevation* table, by elimination -- the
/// color table's bytes hash to the other value.
///
/// So this is an oracle gap rather than a difference. mbgl plainly uploads elevations, and the
/// reason the probe sees none is that the table is a float texture on a path that does not hand it
/// a host pointer. This build packs its elevations as RGBA float with the value in red
/// (`pack_relief_elevation_stops`), and nothing in any capture can confirm the layout. Asserted as
/// far as it goes and named for what it does not, so a clean run is not read as covering it.
#[test]
fn the_ramp_texels_match_the_oracle_where_the_capture_has_them() {
    let style = Style::parse(STYLE).expect("the style parses");
    let layer = style.layer("relief").expect("the relief layer");
    let paint = tessella_style::property::resolve_paint(layer).expect("the paint resolves");
    let ramp = paint
        .get("color-relief-color")
        .map(|property| tessella_style::ramp::relief_ramp(&property.expression))
        .expect("the layer names a ramp")
        .expect("the ramp evaluates");

    let colors = tessella_orchestrate::ubo::pack_relief_color_stops(&ramp);
    let elevations = tessella_orchestrate::ubo::pack_relief_elevation_stops(&ramp);
    assert_eq!(colors.len(), ramp.len() * 4, "RGBA per stop");
    assert_eq!(elevations.len(), ramp.len() * 16, "RGBA float per stop");

    #[allow(clippy::cast_possible_truncation)]
    let width = ramp.len() as u32;
    let rows: Vec<u64> = oracle_hashes()
        .into_iter()
        .filter(|&(size, _)| size == (width, 1))
        .map(|(_, hash)| hash)
        .collect();
    assert_eq!(
        rows.len(),
        2,
        "two {width}x1 tables in the capture: {rows:?}"
    );

    assert!(
        rows.contains(&fnv1a(&colors)),
        "the color table should be byte-identical to mbgl's: {:016x} against {rows:016x?}",
        fnv1a(&colors)
    );

    // The other row is the empty one, which is what makes the elevation table uncomparable.
    let empty = rows.iter().filter(|&&hash| hash == FNV_OFFSET).count();
    assert_eq!(
        empty, 1,
        "one of the two tables should have reached the probe with no bytes: {rows:016x?}"
    );
    assert_ne!(
        fnv1a(&elevations),
        FNV_OFFSET,
        "this build does pack elevations, so a match against the empty row would be meaningless"
    );
}
