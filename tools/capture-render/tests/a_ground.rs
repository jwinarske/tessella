// SPDX-License-Identifier: BSD-2-Clause
//! A DEM on the command line puts a ground in the capture, and nothing else does.
//!
//! The one thing about `--dem` that is not visible in the file it writes: a capture of a terrain
//! style without it is a perfectly good *flat* capture, with every record a consumer expects and
//! no raised drawable in it. That is what this tool wrote for every terrain style before the flag
//! existed, and what it wrote again when the flag was added and the ground's own layer was not --
//! `Style::synthesize_terrain` makes that layer, `Map::new` calls it before anything reads the
//! layer list, and a tool that parses a style itself has to as well.
//!
//! So the check is the pair: the same style and tile with the DEM and without, and the difference
//! is a drawable the producer marked `ON_TERRAIN`.
//!
//! Run as a process rather than as a function, because what is under test is the tool's own
//! assembly of a frame -- the arguments, the style preparation and the builders in the order it
//! runs them -- and none of that is reachable from a library.

use std::process::Command;

use tessella_capture_abi::EnvelopeKind;
use tessella_capture_abi::envelope::{DrawFlags, ViewUse, WireRecord as _};

/// A style with a ground over a raster-dem source, and one layer standing on it.
///
/// The background is the layer that matters here: it is sourceless, so it is built for every tile
/// of the cover whatever the vector tile holds, and the producer raises the bindings of any tile a
/// ground covers. A style that needed the tile's own features would be testing the fixture.
const STYLE: &str = r##"{
  "version": 8,
  "sources": {
    "world": { "type": "vector", "url": "http://127.0.0.1:1/world.json" },
    "dem": {
      "type": "raster-dem",
      "encoding": "mapbox",
      "tileSize": 256,
      "tiles": ["http://127.0.0.1:1/dem/{z}/{x}/{y}.png"],
      "maxzoom": 15
    }
  },
  "terrain": { "source": "dem", "exaggeration": 1.5 },
  "layers": [{ "id": "bg", "type": "background", "paint": { "background-color": "#eae6dd" } }]
}"##;

/// The address the fixtures are placed at, which is the DEM fixture's own.
const AT: &str = "8/127/85";

fn root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Runs the tool and returns how many drawables the capture it wrote are raised.
fn raised(with_dem: bool) -> usize {
    let root = root();
    let out = std::env::temp_dir().join(format!("capture-render-ground-{with_dem}.tslcap"));
    let style = std::env::temp_dir().join(format!("capture-render-ground-{with_dem}.json"));
    std::fs::write(&style, STYLE).expect("the style is written");

    let mut command = Command::new(env!("CARGO_BIN_EXE_capture-render"));
    command
        .arg("--style")
        .arg(&style)
        .arg("--tile")
        .arg(root.join("tests/live-fixtures/world_z7-5-14-9.mvt"))
        .arg("--tile-at")
        .arg(AT)
        .arg("--source")
        .arg("world")
        .arg("--lon")
        .arg("-1.4")
        .arg("--lat")
        .arg("51.6")
        .arg("--zoom")
        .arg("8")
        .arg("--capture")
        .arg(&out);
    if with_dem {
        command
            .arg("--dem")
            .arg(root.join("tests/dem-fixtures/8-127-85.png"));
    }
    let run = command.output().expect("the tool runs");
    assert!(
        run.status.success(),
        "capture-render failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );

    let bytes = std::fs::read(&out).expect("the capture is written");
    let read = tessella_capture_abi::capture::decode(&bytes).expect("a capture");
    let mut raised = 0;
    for record in read.records() {
        if u16::try_from(record.kind)
            .ok()
            .and_then(EnvelopeKind::from_repr)
            != Some(EnvelopeKind::ViewUse)
        {
            continue;
        }
        let Some(use_) = ViewUse::from_bytes(record.record) else {
            continue;
        };
        if use_.draw_flags.bits() & DrawFlags::ON_TERRAIN.bits() != 0 {
            raised += 1;
        }
    }
    raised
}

#[test]
fn a_dem_raises_the_drawables_it_covers_and_no_dem_raises_none() {
    assert_eq!(
        raised(false),
        0,
        "a terrain style with no DEM has nothing to raise from, so the capture is flat"
    );
    assert!(
        raised(true) > 0,
        "the DEM's own tile carries a ground, and what stands on it is raised"
    );
}
