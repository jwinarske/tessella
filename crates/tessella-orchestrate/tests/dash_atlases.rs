// SPDX-License-Identifier: BSD-2-Clause
//! `line-dasharray`: what keys an atlas, what shape it is, and the gamma it is sampled with.
//!
//! # What the oracle gives
//!
//! `tests/golden/dash_style.dump`: three roads under seven layers -- a plain line, the same
//! dasharray at three line widths, that dasharray again with round caps and joins, and a
//! four-entry dasharray. One camera, one zoom, six z13 tiles.
//!
//! # Why this capture exists
//!
//! Dashes were implemented and compared against the oracle in one place only: `line_dash_stream.rs`
//! checks that a dasharray selects the SDF shader, that the atlas is uploaded before a drawable
//! names it, and that the period lands where a ruler finds it. All three reason from mbgl's source
//! rather than from a capture, and a single dashed layer at one width cannot see any of what
//! tessella#284 named:
//!
//! - **The period against `line-width`.** Three widths, one dasharray.
//! - **Caps.** `line-cap: round` gives the atlas a different shape, not just different content.
//! - **More than one dashed layer in a tile.** `LineSDFTilePropsUBO` is packed one entry per
//!   drawable at the union's stride, and with a single dashed layer a stride error is invisible --
//!   one entry is right at any stride.
//! - **A dasharray of more than two entries.**
//! - **The atlas's own shape**, which nothing asserted the width or the row count of.
//!
//! # What it settles
//!
//! **`line-width` does not key the atlas, and does not reach the gamma either.** Widths 1, 6 and
//! 24 bind one texture in the capture and pack one `sdfgamma`. Width enters through `floorwidth`,
//! which the shader divides the gamma by (`smoothstep(0.5 - sdfgamma/floorwidth, ...)`), so the
//! dash edge softens with the line rather than the pattern being rebuilt per width. That is
//! visible in the capture's `LineEvaluatedPropsUBO`, whose width/floorwidth pairs read 1/1, 6/6
//! and 24/24.
//!
//! **The cap does key it, and changes the row count.** Round caps give a 16-row atlas and butt
//! caps a single row, because a round cap's distance field varies down the row and a butt cap's
//! does not. The `y` and `height` move with it: 0.5/1 for butt, 0.46875/0.9375 for round.
//!
//! **The dasharray keys it**, and its sum sets the gamma: `sdfgamma` is `1/sum` here -- 1/3.5 for
//! `[2, 1.5]` and 1/8 for `[4, 1, 2, 1]`.
//!
//! # The gamma is where a factor of two hides
//!
//! `sdfgamma` is `atlasWidth / (min(widthA, widthB) * 256 * pixelRatio) / 2`, and the two widths
//! are the pattern's length scaled by the crossfade -- `posA.width * fromScale`. So the crossfade
//! decides the gamma, and its two branches put `fromScale` a factor of four apart:
//!
//! ```cpp
//! return z > zoomHistory.lastIntegerZoom
//!            ? CrossfadeParameters{.fromScale = 2.0f, ...}
//!            : CrossfadeParameters{.fromScale = 0.5f, ...};
//! ```
//!
//! At this capture's zoom 13.0 with `lastIntegerZoom` 13 the comparison is `13.0 > 13.0`, which is
//! false, so `fromScale` is 0.5 and the *from* pattern is the narrower of the two. Take the other
//! branch and it becomes the wider, so `min` picks `widthB` instead: the minimum doubles and the
//! gamma halves, which softens every dash edge and moves no geometry at all. A [`ZoomHistory`]
//! left at its `Default` rather than
//! [`ZoomHistory::new`] does exactly that: `Default` leaves `first` false, so
//! `Dashes::for_buckets`'s seeding is skipped, `last_integer_zoom` stays zero, and every positive
//! zoom takes the zooming-in branch. `a_default_zoom_history_halves_the_gamma` pins the difference
//! so it stays a caught mistake.
//!
//! # Where this build differs: one atlas per layer, not one per pattern
//!
//! mbgl's `LineAtlas` is keyed by (`from`, `to`, cap), so the three width layers *share* a texture
//! and the capture holds three dash textures for five dashed layers. This build keys by the layer
//! and holds five, two of which are byte-identical copies. That is deliberate and documented in
//! `dash.rs`: a dasharray may step with zoom, so the pattern a layer needs changes under a moving
//! camera, and numbering textures by the patterns a frame happened to want would hand a live id to
//! different pixels as the set shifted. It costs a texture object and an upload per redundant
//! dashed layer, which is why it is counted here rather than left implicit -- see tessella#287.

mod common;

use common::fnv1a;

use std::collections::{BTreeMap, BTreeSet};

use tessella_orchestrate::dash::Dashes;
use tessella_orchestrate::tile::{Content, TileId, build_tile};
use tessella_orchestrate::ubo::{self, DashPlacement};
use tessella_source::tiling::TilingOptions;
use tessella_style::Style;
use tessella_style::crossfade::ZoomHistory;

const DUMP: &str = include_str!("../../../tests/golden/dash_style.dump");
const STYLE: &str = include_str!("../../tessella-style/tests/dash_style.json");

/// The z13 cover of the capture's camera.
const TILES: [(u32, u32); 6] = [
    (4092, 2723),
    (4092, 2724),
    (4093, 2723),
    (4093, 2724),
    (4094, 2723),
    (4094, 2724),
];

/// `LineShader` and `LineSDFShader`, counting from `BuiltIn::None`.
const LINE: &str = "sh0025";
const LINE_SDF: &str = "sh0030";

/// The atlas is this wide whatever the pattern is, which is what stretches a pattern to fit.
const ATLAS_WIDTH: usize = 256;

/// The union stride `LineSDFTilePropsUBO` is packed at.
const TILE_PROPS_STRIDE: u32 = 64;

/// The layers the fixture declares, in order.
const DASHED: [&str; 5] = [
    "dash-thin",
    "dash-mid",
    "dash-thick",
    "dash-round",
    "dash-four",
];

/// The fixture's style, its features, and the dashes a frame of it needs.
///
/// `ZoomHistory::new` rather than `Default`, which is the trap the module doc describes and which
/// `a_default_zoom_history_halves_the_gamma` measures. It is what `frame.rs` passes when no layer
/// carries a sprite pattern, as none here does.
fn ours() -> (Style, Dashes) {
    let style = Style::parse(STYLE).expect("the style parses");
    let tessella_style::Source::Geojson(source) = style.source("probe").expect("a source") else {
        panic!("the fixture has one geojson source")
    };
    let features = tessella_source::geojson::read(&source.data).expect("features read");

    let mut buckets = Vec::new();
    for (x, y) in TILES {
        let tile = TileId::new(13, x, y);
        let built = build_tile(&style, "probe", tile, &features, TilingOptions::default())
            .expect("the tile builds");
        buckets.push((tile, std::sync::Arc::new(built)));
    }
    let dashes = Dashes::for_buckets(&style, &buckets, 13.0, ZoomHistory::new(), 200);
    (style, dashes)
}

/// A layer's index in the style, by id.
fn index(style: &Style, want: &str) -> usize {
    style
        .layers
        .iter()
        .position(|layer| layer.id == want)
        .unwrap_or_else(|| panic!("no layer {want}"))
}

/// Every `drawable` line the capture recorded for a layer.
fn drawables_of(layer: usize) -> Vec<&'static str> {
    let tag = format!("L{layer:05}.");
    DUMP.lines()
        .filter(|line| line.starts_with("drawable L") && line.contains(&tag))
        .collect()
}

/// The texture ids a layer's drawables bind, with their multiplicity.
fn bound_textures(layer: usize) -> Vec<u32> {
    let tag = format!("L{layer:05}.");
    DUMP.lines()
        .map(str::trim_start)
        .filter(|line| line.starts_with("tex ") && line.contains(&tag))
        .filter_map(|line| line.split("tex=").nth(1)?.trim().parse().ok())
        .collect()
}

/// Every `texture WxH fmt=F` the capture recorded.
fn oracle_textures() -> Vec<(u32, u32, u32)> {
    DUMP.lines()
        .filter_map(|line| line.strip_prefix("texture "))
        .filter_map(|rest| {
            let mut parts = rest.split_whitespace();
            let (w, h) = parts.next()?.split_once('x')?;
            let fmt = parts.next()?.strip_prefix("fmt=")?;
            Some((w.parse().ok()?, h.parse().ok()?, fmt.parse().ok()?))
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

/// A layer's UBO record at `slot`, as its raw bytes.
///
/// The dump prints these with their 16-byte blocks sorted, so what comes back is comparable as a
/// multiset of blocks and not as a byte sequence. That is enough for a block that repeats per
/// drawable, which is what this one is.
fn oracle_ubo(layer: usize, slot: u32) -> Vec<u8> {
    let tag = format!("ubo layer:{layer}/");
    let line = DUMP
        .lines()
        .filter(|line| line.starts_with(&tag))
        .find(|line| line.contains(&format!(" slot={slot} ")))
        .unwrap_or_else(|| panic!("no slot {slot} for layer {layer}"));
    let hex = line
        .split(" bytes=")
        .nth(1)
        .expect("a bytes= field")
        .split_whitespace()
        .next()
        .expect("the hex run");
    assert!(
        hex.bytes().all(|b| b.is_ascii_hexdigit()),
        "layer {layer} slot {slot} has elided fields, which cannot be compared: {hex}"
    );
    (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).expect("a hex byte"))
        .collect()
}

/// A buffer's 16-byte blocks, sorted, which is the form the dump prints.
fn blocks(bytes: &[u8]) -> Vec<[u8; 16]> {
    assert_eq!(
        bytes.len() % 16,
        0,
        "a UBO buffer is a whole number of blocks"
    );
    let mut out: Vec<[u8; 16]> = bytes.as_chunks::<16>().0.to_vec();
    out.sort_unstable();
    out
}

/// A dashed layer takes the SDF shader and binds an atlas; a plain line takes neither.
#[test]
fn a_dasharray_selects_the_sdf_shader_and_binds_an_atlas() {
    let (style, dashes) = ours();

    let plain = index(&style, "plain");
    let drawn = drawables_of(plain);
    assert!(!drawn.is_empty(), "the plain layer drew nothing");
    assert!(
        drawn.iter().all(|line| line.contains(LINE)),
        "an undashed line takes LineShader: {drawn:?}"
    );
    assert!(
        bound_textures(plain).is_empty(),
        "an undashed line has no atlas to bind"
    );
    assert!(
        dashes.get(plain).is_none(),
        "the plain layer should not have built an atlas"
    );

    for named in DASHED {
        let layer = index(&style, named);
        let drawn = drawables_of(layer);
        assert!(!drawn.is_empty(), "{named} drew nothing");
        assert!(
            drawn.iter().all(|line| line.contains(LINE_SDF)),
            "{named} should take LineSDFShader: {drawn:?}"
        );
        assert_eq!(
            bound_textures(layer).len(),
            drawn.len(),
            "{named}: every dashed drawable binds its atlas"
        );
        assert!(
            dashes.get(layer).is_some(),
            "{named} should have built an atlas"
        );
    }
}

/// `line-width` neither keys the atlas nor reaches the gamma.
///
/// Widths 1, 6 and 24 over the same dasharray. In the capture all three bind one texture id, and
/// here all three build byte-identical atlases. Width enters through `floorwidth`, which the
/// shader divides the gamma by -- see the module doc.
#[test]
fn line_width_does_not_key_the_atlas() {
    let (style, dashes) = ours();
    let widths = ["dash-thin", "dash-mid", "dash-thick"];

    let bound: BTreeSet<u32> = widths
        .iter()
        .flat_map(|named| bound_textures(index(&style, named)))
        .collect();
    assert_eq!(
        bound.len(),
        1,
        "the oracle binds one atlas across the three widths, not {}: {bound:?}",
        bound.len()
    );

    let atlases: Vec<&[u8]> = widths
        .iter()
        .map(|named| {
            dashes
                .get(index(&style, named))
                .unwrap_or_else(|| panic!("{named} has an atlas"))
                .atlas
                .data
                .as_slice()
        })
        .collect();
    for pair in atlases.windows(2) {
        assert_eq!(
            pair[0], pair[1],
            "the same dasharray at two widths should build the same distance field"
        );
    }

    // And the gamma, which is the other place a width could leak in.
    let gammas: BTreeSet<Vec<u8>> = widths
        .iter()
        .map(|named| tile_props(&style, &dashes, named, 1))
        .collect();
    assert_eq!(
        gammas.len(),
        1,
        "three widths should pack one sdfgamma, not {}",
        gammas.len()
    );
}

/// `line-cap: round` keys the atlas and gives it sixteen rows where butt gives one.
///
/// A round cap's distance field varies down the row -- the field has to describe a semicircular
/// end -- so the pattern occupies a block of rows. The `y` and `height` move with it, which is
/// what the drawable block's `-height/2` carries.
#[test]
fn the_cap_keys_the_atlas() {
    let (style, dashes) = ours();
    let butt = dashes.get(index(&style, "dash-mid")).expect("butt atlas");
    let round = dashes
        .get(index(&style, "dash-round"))
        .expect("round atlas");

    assert_eq!(butt.atlas.height, 1, "a butt cap needs one row");
    assert_eq!(round.atlas.height, 16, "a round cap needs sixteen");
    assert_ne!(
        butt.atlas.data, round.atlas.data,
        "two caps over one dasharray are two different distance fields"
    );

    // The placement follows the row count.
    assert!(
        (butt.from.y - 0.5).abs() < f32::EPSILON && (butt.from.height - 1.0).abs() < f32::EPSILON,
        "a single-row pattern sits at y 0.5 spanning the whole atlas: {:?}",
        butt.from
    );
    assert!(
        (round.from.y - 0.468_75).abs() < f32::EPSILON
            && (round.from.height - 0.937_5).abs() < f32::EPSILON,
        "a sixteen-row pattern's placement is inset by half a row: {:?}",
        round.from
    );

    // The oracle binds it separately, and its 256x16 is the only one of that shape.
    let mid = bound_textures(index(&style, "dash-mid"));
    let rounds = bound_textures(index(&style, "dash-round"));
    assert!(
        rounds.iter().all(|id| !mid.contains(id)),
        "the round layer should not share the butt layer's atlas: {rounds:?} against {mid:?}"
    );
    let tall: Vec<_> = oracle_textures()
        .into_iter()
        .filter(|&(_, height, _)| height > 1)
        .collect();
    assert_eq!(
        tall,
        vec![(256, 16, 1)],
        "one round-cap atlas, 256x16 single-channel"
    );
}

/// A four-entry dasharray is its own atlas, and its sum sets the gamma.
///
/// `sdfgamma` is `1/sum` at this capture's crossfade: 1/3.5 for `[2, 1.5]` and 1/8 for
/// `[4, 1, 2, 1]`. The pattern's `width` is that sum, which is what the atlas stretches to 256.
#[test]
fn a_longer_dasharray_is_its_own_atlas() {
    let (style, dashes) = ours();
    let two = dashes.get(index(&style, "dash-mid")).expect("two-entry");
    let four = dashes.get(index(&style, "dash-four")).expect("four-entry");

    assert!(
        (two.from.width - 3.5).abs() < f32::EPSILON,
        "[2, 1.5] sums to 3.5, not {}",
        two.from.width
    );
    assert!(
        (four.from.width - 8.0).abs() < f32::EPSILON,
        "[4, 1, 2, 1] sums to 8, not {}",
        four.from.width
    );
    assert_ne!(
        two.atlas.data, four.atlas.data,
        "two dasharrays should not build the same distance field"
    );

    // The oracle keeps them apart too, by id and by content hash.
    let mid = bound_textures(index(&style, "dash-mid"));
    let fours = bound_textures(index(&style, "dash-four"));
    assert!(
        fours.iter().all(|id| !mid.contains(id)),
        "a different dasharray is a different texture: {fours:?} against {mid:?}"
    );
    let rows: Vec<_> = oracle_textures()
        .into_iter()
        .filter(|&(width, height, _)| width == 256 && height == 1)
        .collect();
    assert_eq!(rows.len(), 2, "two single-row atlases, one per dasharray");
}

/// The atlas is 256 wide and one byte a pixel, with a power-of-two row count.
///
/// The width is what makes one row serve a pattern of any length: the shader divides the distance
/// along the line by the pattern's own width, so the row is a stretched period rather than a
/// period at scale. Every dash texture in the capture is `256xH fmt=1`.
#[test]
fn the_atlas_is_256_wide_and_single_channel() {
    let (style, dashes) = ours();
    for named in DASHED {
        let dash = dashes.get(index(&style, named)).expect("an atlas");
        assert!(
            dash.atlas.height.is_power_of_two(),
            "{named}: a row count of {} is not a power of two",
            dash.atlas.height
        );
        assert_eq!(
            dash.atlas.data.len(),
            ATLAS_WIDTH * dash.atlas.height as usize,
            "{named}: {ATLAS_WIDTH} by {} at one byte a pixel",
            dash.atlas.height
        );
    }

    for (width, height, fmt) in oracle_textures() {
        if height == 0 || (width, height) == (1, 1) {
            // The probe's empty and placeholder textures, which no dashed drawable binds.
            continue;
        }
        assert_eq!(width, 256, "a dash atlas is 256 wide: {width}x{height}");
        assert_eq!(fmt, 1, "a distance field is single-channel: fmt={fmt}");
    }
}

/// mbgl shares an atlas between layers that want the same one; this build does not.
///
/// Counted rather than left implicit. The capture holds three dash textures for five dashed
/// layers -- keyed by (`from`, `to`, cap) -- and this build holds five, one per layer, two of them
/// byte-identical copies of a third. The reason is id stability under a moving camera and it is
/// documented in `dash.rs`; the cost is an upload and a texture object per redundant layer, which
/// is what tessella#287 is about.
#[test]
fn mbgl_shares_an_atlas_between_layers_and_this_does_not() {
    let (style, dashes) = ours();

    let theirs: BTreeSet<u32> = DASHED
        .iter()
        .flat_map(|named| bound_textures(index(&style, named)))
        .collect();
    assert_eq!(
        theirs.len(),
        3,
        "five dashed layers, three distinct (dasharray, cap) pairs: {theirs:?}"
    );

    let ids: BTreeSet<u64> = DASHED
        .iter()
        .map(|named| {
            dashes
                .get(index(&style, named))
                .expect("an atlas")
                .texture
                .0
        })
        .collect();
    assert_eq!(
        ids.len(),
        5,
        "this build keys by the layer, so five layers are five ids: {ids:?}"
    );

    // Which is to say two of the five uploads carry bytes already on the device.
    //
    // Once, not once a frame. A generated texture is written only when its bytes differ from what
    // the consumer holds, and these ids are per layer and stable, so the redundant pair goes out on
    // the frame that declares the view and never again while the atlas is unchanged --
    // `incremental_frames.rs::the_redundant_dash_atlases_are_sent_once_and_not_per_frame` measures
    // that. So the divergence costs two uploads and two texture slots, not a per-frame tax, which is
    // what #287 was weighing against an id-lifetime contract.
    let mut seen: BTreeMap<&[u8], usize> = BTreeMap::new();
    for named in DASHED {
        let dash = dashes.get(index(&style, named)).expect("an atlas");
        *seen.entry(dash.atlas.data.as_slice()).or_default() += 1;
    }
    assert_eq!(
        seen.len(),
        3,
        "three distinct distance fields behind the five ids"
    );
    assert_eq!(
        seen.values().sum::<usize>() - seen.len(),
        2,
        "two redundant uploads, which is the whole of the divergence"
    );
}

/// A layer's `LineSDFTilePropsUBO` buffer, packed as `frame.rs` packs it.
fn tile_props(style: &Style, dashes: &Dashes, named: &str, count: usize) -> Vec<u8> {
    let dash = dashes
        .get(index(style, named))
        .unwrap_or_else(|| panic!("{named} has an atlas"));
    let placement = DashPlacement {
        from: dash.from,
        to: dash.to,
        crossfade: dash.crossfade,
        pixel_ratio: 1.0,
    };
    ubo::pack_line_sdf_tile_props(&placement, count, TILE_PROPS_STRIDE)
}

/// `LineSDFTilePropsUBO` matches the oracle, block for block, for every dashed layer.
///
/// This is the assertion a single dashed layer could not make. It pins three things at once: the
/// gamma's value, that one entry is packed per drawable, and that the entries sit at the union's
/// stride of sixty-four rather than at the block's own eight -- five entries at the wrong stride
/// would be forty bytes where the oracle has three hundred and twenty.
#[test]
fn the_tile_props_match_the_oracle_block_for_block() {
    let (style, dashes) = ours();

    for named in DASHED {
        let layer = index(&style, named);
        let count = drawables_of(layer).len();
        assert_eq!(count, 5, "{named} draws five tiles of the cover");

        let want = oracle_ubo(layer, 3);
        assert_eq!(
            want.len(),
            count * TILE_PROPS_STRIDE as usize,
            "{named}: the oracle packs one {TILE_PROPS_STRIDE}-byte entry per drawable"
        );
        let got = tile_props(&style, &dashes, named, count);
        assert_eq!(
            blocks(&got),
            blocks(&want),
            "{named}: the tile props disagree with the oracle"
        );
    }

    // An undashed line still gets the block, zero-filled, because the union is written whatever
    // the layer is. A frontend that skipped it would leave whatever the slot last held.
    let plain = index(&style, "plain");
    let want = oracle_ubo(plain, 3);
    assert_eq!(want.len(), 5 * TILE_PROPS_STRIDE as usize);
    assert!(
        want.iter().all(|&byte| byte == 0),
        "an undashed line's tile props are zeros"
    );
    let got = ubo::pack_tile_props_buffer(5, TILE_PROPS_STRIDE);
    assert_eq!(got, want, "the plain layer's block should match");
}

/// A `Default` zoom history halves every gamma, and moves not one vertex.
///
/// `Default` leaves `first` false, so `Dashes::for_buckets` does not seed the history, and
/// `last_integer_zoom` stays zero -- which makes `z > last_integer_zoom` true at every positive
/// zoom and takes the zooming-*in* crossfade branch. `from_scale` is then 2.0 where the oracle
/// has 0.5, `min(width_a, width_b)` doubles, and `sdfgamma` halves.
///
/// Pinned because the failure is a factor of two in one float: the dashes stay where they are and
/// their edges soften, which is not something a gross-pixel count is going to raise.
#[test]
fn a_default_zoom_history_halves_the_gamma() {
    let style = Style::parse(STYLE).expect("the style parses");
    let tessella_style::Source::Geojson(source) = style.source("probe").expect("a source") else {
        panic!("the fixture has one geojson source")
    };
    let features = tessella_source::geojson::read(&source.data).expect("features read");
    let mut buckets = Vec::new();
    for (x, y) in TILES {
        let tile = TileId::new(13, x, y);
        let built = build_tile(&style, "probe", tile, &features, TilingOptions::default())
            .expect("the tile builds");
        buckets.push((tile, std::sync::Arc::new(built)));
    }

    let seeded = Dashes::for_buckets(&style, &buckets, 13.0, ZoomHistory::new(), 200);
    let unseeded = Dashes::for_buckets(&style, &buckets, 13.0, ZoomHistory::default(), 200);
    let layer = index(&style, "dash-mid");

    let right = seeded.get(layer).expect("an atlas");
    let wrong = unseeded.get(layer).expect("an atlas");
    assert!(
        (right.crossfade.from_scale - 0.5).abs() < f32::EPSILON,
        "at zoom 13.0 with last_integer_zoom 13, from_scale is 0.5"
    );
    assert!(
        (wrong.crossfade.from_scale - 2.0).abs() < f32::EPSILON,
        "an unseeded history takes the zooming-in branch"
    );
    assert_eq!(
        right.atlas.data, wrong.atlas.data,
        "the atlas is unchanged, which is why only the gamma shows it"
    );

    let gamma = |dashes: &Dashes| -> f32 {
        let bytes = tile_props(&style, dashes, "dash-mid", 1);
        f32::from_le_bytes(bytes[..4].try_into().expect("four bytes"))
    };
    let (right, wrong) = (gamma(&seeded), gamma(&unseeded));
    assert!(
        (right - 1.0 / 3.5).abs() < 1e-7,
        "the oracle's gamma is 1/sum, which is 1/3.5 here, not {right}"
    );
    assert!(
        (wrong - right / 2.0).abs() < 1e-7,
        "an unseeded history should halve it: {wrong} against {right}"
    );
}

/// The line geometry agrees with the oracle, layer for layer.
///
/// The round-cap layer is the one worth having: it spends 96 vertices and 234 indices against the
/// 44 and 78 of a butt-capped line over the same three roads, so a cap or join regression shows
/// here as a count rather than as a handful of edge pixels.
#[test]
fn the_line_geometry_matches_the_oracle() {
    let style = Style::parse(STYLE).expect("the style parses");
    let tessella_style::Source::Geojson(source) = style.source("probe").expect("a source") else {
        panic!("the fixture has one geojson source")
    };
    let features = tessella_source::geojson::read(&source.data).expect("features read");

    let mut oracle: BTreeMap<usize, (usize, usize)> = BTreeMap::new();
    for line in DUMP.lines() {
        let Some(rest) = line.strip_prefix("drawable L") else {
            continue;
        };
        if !rest.contains(LINE) && !rest.contains(LINE_SDF) {
            continue;
        }
        let layer: usize = rest[..5].parse().expect("a five-digit layer index");
        let verts: usize = rest
            .split(".v")
            .nth(1)
            .and_then(|tail| tail.split('#').next())
            .expect("a vertex count")
            .parse()
            .expect("digits");
        let indices: usize = line
            .split("idx=")
            .nth(1)
            .and_then(|tail| tail.split(':').next())
            .expect("an index count")
            .parse()
            .expect("digits");
        let slot = oracle.entry(layer).or_default();
        slot.0 += verts;
        slot.1 += indices;
    }

    let mut ours: BTreeMap<usize, (usize, usize)> = BTreeMap::new();
    for (x, y) in TILES {
        for bucket in &build_tile(
            &style,
            "probe",
            TileId::new(13, x, y),
            &features,
            TilingOptions::default(),
        )
        .expect("the tile builds")
        {
            if let Content::Line(line) = &bucket.content {
                let slot = ours.entry(bucket.layer_index).or_default();
                slot.0 += line.vertices.len();
                slot.1 += line.indices.len();
            }
        }
    }

    assert_eq!(ours.len(), 6, "a plain line and five dashed ones");
    for (&layer, &counts) in &ours {
        let name = style.layers.get(layer).map_or("?", |l| l.id.as_str());
        let want = oracle
            .get(&layer)
            .copied()
            .unwrap_or_else(|| panic!("the oracle drew nothing for layer {layer} ({name})"));
        assert_eq!(
            counts, want,
            "layer {layer} ({name}): {counts:?} against the oracle's {want:?}"
        );
    }

    // The round-cap layer costs more than twice a butt-capped one over the same roads.
    let round = ours[&index(&style, "dash-round")];
    let butt = ours[&index(&style, "dash-mid")];
    assert_eq!(round, (96, 234), "round caps and joins");
    assert_eq!(butt, (44, 78), "butt caps and miter joins");
}

/// The distance fields match the oracle's, byte for byte.
///
/// The shape assertions above would pass on a field of the right size holding the wrong ramp, and a
/// dash's edge is a smoothstep across a few texels -- a field built with the wrong stretch, or with
/// the dash and gap swapped, moves very few pixels. The capture records each texture's FNV-1a, so
/// the bytes are comparable (tessella#286).
///
/// All three agree, including the 4096-byte round-cap field, so mbgl's `LineAtlas` arithmetic is
/// reproduced exactly and not merely to the shape.
#[test]
fn the_distance_fields_match_the_oracle() {
    let (style, dashes) = ours();

    // The three distinct fields behind the five per-layer copies -- see the sharing test above.
    let mut ours: Vec<u64> = ["dash-mid", "dash-round", "dash-four"]
        .iter()
        .map(|named| {
            fnv1a(
                &dashes
                    .get(index(&style, named))
                    .expect("an atlas")
                    .atlas
                    .data,
            )
        })
        .collect();

    let mut theirs: Vec<u64> = oracle_hashes()
        .into_iter()
        .filter(|&((width, _), _)| width == ATLAS_WIDTH as u32)
        .map(|(_, hash)| hash)
        .collect();
    assert_eq!(theirs.len(), 3, "three dash atlases in the capture");

    ours.sort_unstable();
    theirs.sort_unstable();
    assert_eq!(
        ours, theirs,
        "the distance fields should be byte-identical to mbgl's: {ours:016x?} against {theirs:016x?}"
    );

    // And the three layers that share a pattern really do hash alike, which is what makes the
    // comparison above three values rather than five.
    let shared: Vec<u64> = ["dash-thin", "dash-mid", "dash-thick"]
        .iter()
        .map(|named| {
            fnv1a(
                &dashes
                    .get(index(&style, named))
                    .expect("an atlas")
                    .atlas
                    .data,
            )
        })
        .collect();
    assert!(
        shared.windows(2).all(|pair| pair[0] == pair[1]),
        "three widths, one field: {shared:016x?}"
    );
}
