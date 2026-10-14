// SPDX-License-Identifier: BSD-2-Clause
//! A style whose source is an inline GeoJSON needs no tile beside it, and carries two shaders
//! nothing else here can reach.
//!
//! # Why this path exists
//!
//! `line-dasharray` and `line-gradient` are the producer's two remaining line shaders --
//! `LineSDFShader` and `LineGradientShader` -- and until this, no capture could carry either. Not
//! because the producer does not emit them: `tests/gradient_ramps.rs` and `tests/dash_atlases.rs`
//! pin both against the oracle's own dumps. Because the *styles that set them* are GeoJSON, and
//! this tool read vector tiles alone.
//!
//! A gradient has no choice about it. `line-gradient` reads how far along the whole of a line a
//! piece runs, which is `lineMetrics`, and a vector tile arrives already cut -- a line crossing a
//! tile edge is two lines there, each starting its gradient over. MapLibre allows the property on
//! a GeoJSON source for exactly that reason, so a tool that reads only cut tiles can never make a
//! capture carrying one.
//!
//! # What the two kinds do differently here
//!
//! A vector tile belongs at one address and the cover's other tiles get nothing, which is what
//! `--tile-at` is for. An inline GeoJSON is not cut at all: every tile of the cover is cut from
//! the same features, so each may have geometry of its own and `--tile-at` means nothing. This
//! checks that second half by counting the tiles that carried features.

use std::process::Command;

use tessella_capture_abi::envelope::{GeometryAdd, TextureUpdate, WireRecord as _};
use tessella_capture_abi::{BuiltIn, EnvelopeKind};

/// The style fixtures, which are the ones the golden dumps were captured from.
///
/// Named rather than copied: `tests/golden/gradient_style.dump` and `dash_style.dump` are mbgl's
/// answers for *these* documents, so a capture written from a copy of them would be a capture of
/// something the oracle never saw.
const GRADIENT: &str = "crates/tessella-style/tests/gradient_style.json";
const DASH: &str = "crates/tessella-style/tests/dash_style.json";

fn root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// What one run of the tool put in its capture.
struct Written {
    /// Shaders the drawables name, by `builtin_shader`.
    shaders: Vec<i32>,
    /// Every texture declared, as `(width, height)`.
    textures: Vec<(u32, u32)>,
    /// Tiles that carried at least one drawable, by the id the record names.
    tiles: std::collections::BTreeSet<(u8, u32, u32)>,
}

/// Runs the tool over a style fixture with no `--tile` at all.
fn written(style: &str) -> Written {
    let root = root();
    let name = style.rsplit('/').next().unwrap_or(style);
    let out = std::env::temp_dir().join(format!("capture-render-geojson-{name}.tslcap"));

    let run = Command::new(env!("CARGO_BIN_EXE_capture-render"))
        .arg("--style")
        .arg(root.join(style))
        .arg("--source")
        .arg("probe")
        // The fixtures' own camera: the polylines are around Westminster, and the two that cross
        // a z13 tile boundary are what `lineMetrics` is in the style for.
        .arg("--lon")
        .arg("-0.13")
        .arg("--lat")
        .arg("51.508")
        .arg("--zoom")
        .arg("13")
        .arg("--capture")
        .arg(&out)
        .output()
        .expect("the tool runs");
    assert!(
        run.status.success(),
        "capture-render failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );

    let bytes = std::fs::read(&out).expect("the capture is written");
    let read = tessella_capture_abi::capture::decode(&bytes).expect("a capture");
    let mut written = Written {
        shaders: Vec::new(),
        textures: Vec::new(),
        tiles: std::collections::BTreeSet::new(),
    };
    for record in read.records() {
        let kind = u16::try_from(record.kind)
            .ok()
            .and_then(EnvelopeKind::from_repr);
        match kind {
            Some(EnvelopeKind::GeometryAdd) => {
                if let Some(add) = GeometryAdd::from_bytes(record.record) {
                    written.shaders.push(add.builtin_shader);
                }
            }
            Some(EnvelopeKind::TextureUpdate) => {
                if let Some(update) = TextureUpdate::from_bytes(record.record) {
                    written
                        .textures
                        .push((update.size.width, update.size.height));
                }
            }
            Some(EnvelopeKind::ViewUse) => {
                if let Some(use_) =
                    tessella_capture_abi::envelope::ViewUse::from_bytes(record.record)
                    && use_.has_tile != 0
                {
                    written
                        .tiles
                        .insert((use_.tile.z, use_.tile.x, use_.tile.y));
                }
            }
            _ => {}
        }
    }
    written
}

/// A dashed line takes `LineSDFShader`, and the atlas it samples goes up with it.
#[test]
fn a_dasharray_reaches_the_capture() {
    let written = written(DASH);
    assert!(
        written.shaders.contains(&(BuiltIn::LineSDFShader as i32)),
        "no dashed line in the capture: {:?}",
        written.shaders
    );
    // And the plain layer beside it, which is what says the style was drawn rather than that
    // every line became a dash.
    assert!(
        written.shaders.contains(&(BuiltIn::LineShader as i32)),
        "the plain line did not draw: {:?}",
        written.shaders
    );
    // mbgl's dash atlas is 512 wide and one row per pattern, which `dash_atlases.rs` pins from
    // the producer's side. Here it is enough that one went up at all: a capture whose dashed
    // drawable names a texture nothing declared replays as a line with no dash.
    assert!(
        !written.textures.is_empty(),
        "a dashed line and no texture declared"
    );
}

/// A gradient takes `LineGradientShader`, and its ramp is 256 texels wide.
#[test]
fn a_line_gradient_reaches_the_capture() {
    let written = written(GRADIENT);
    assert!(
        written
            .shaders
            .contains(&(BuiltIn::LineGradientShader as i32)),
        "no gradient line in the capture: {:?}",
        written.shaders
    );
    // One 256x1 ramp per gradient layer, whatever the stops are -- mbgl bakes `line-gradient`
    // into one image of that width, which is the convention `gradient_ramps.rs` pins. The style
    // has two gradient layers.
    let ramps: Vec<(u32, u32)> = written
        .textures
        .iter()
        .copied()
        .filter(|(width, _)| *width == 256)
        .collect();
    assert_eq!(
        ramps,
        vec![(256, 1); 2],
        "the ramps are not two 256x1 images: {:?}",
        written.textures
    );
}

/// Every tile of the cover is cut from the same features, so more than one carries geometry.
///
/// The vector path cannot do this: a tile belongs at one address and the rest of the cover is
/// empty, which is what `--tile-at` and the tool's `placed` count are about. A GeoJSON source has
/// no address at all, and a capture that drew its features into one tile would be cutting them
/// wrongly -- the two polylines that cross a z13 boundary are in the fixture for that reason.
#[test]
fn the_cover_is_cut_from_the_same_features() {
    let written = written(GRADIENT);
    assert!(
        written.tiles.len() > 1,
        "only {} tile carried geometry, so the cover was not cut from the features",
        written.tiles.len()
    );
}
