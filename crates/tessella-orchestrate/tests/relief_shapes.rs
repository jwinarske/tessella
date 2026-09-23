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

/// mbgl allocates two offscreen targets per hillshade tile; this build allocates none.
///
/// Recorded rather than asserted against this build, because there is nothing here to compare it
/// to — that is the point. If the CPU differentiation is ever traded for a prepare pass, this is
/// the line that says what mbgl's costs.
#[test]
fn the_oracle_prepares_offscreen_and_this_build_does_not() {
    let targets: Vec<&str> = DUMP
        .lines()
        .filter_map(|line| line.strip_prefix("rendertarget "))
        .collect();
    assert_eq!(targets.len(), 2, "two targets: {targets:?}");
    for target in &targets {
        assert!(
            target.starts_with("256x256"),
            "a prepare pass is the tile's size, not the viewport's: {target}"
        );
    }
}
