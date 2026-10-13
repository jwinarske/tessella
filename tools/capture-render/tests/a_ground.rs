// SPDX-License-Identifier: BSD-2-Clause
//! A DEM on the command line puts a ground in the capture, and what stands on it names it.
//!
//! The one thing about `--dem` that is not visible in the file it writes: a capture of a terrain
//! style without it is a perfectly good *flat* capture, with every record a consumer expects and
//! no raised drawable in it. That is what this tool wrote for every terrain style before the flag
//! existed, and what it wrote again when the flag was added and the ground's own layer was not --
//! `Style::synthesize_terrain` makes that layer, `Map::new` calls it before anything reads the
//! layer list, and a tool that parses a style itself has to as well.
//!
//! So the check is the pair: the same style and tile with the DEM and without, and the difference
//! is a drawable the producer marked `ON_TERRAIN` and gave an elevation to.
//!
//! Run as a process rather than as a function, because what is under test is the tool's own
//! assembly of a frame -- the arguments, the style preparation and the builders in the order it
//! runs them -- and none of that is reachable from a library.

use std::process::Command;

use tessella_capture_abi::envelope::{
    DrawFlags, GeometryAdd, TextureRef, ViewUse, WireRecord as _,
};
use tessella_capture_abi::terrain_ubo::TERRAIN_ELEVATION_SLOT;
use tessella_capture_abi::{BuiltIn, EnvelopeKind};

/// A style with a ground over a raster-dem source, and two layers standing on it.
///
/// The background is sourceless, so it is built for every tile of the cover whatever the vector
/// tile holds, and the producer raises the bindings of any tile a ground covers -- which is what
/// makes the first assertion independent of the fixture.
///
/// The extrusion layer is the second assertion's, and it reads the fixture's `water` polygons
/// because what is under test is that an extrusion *on* the ground names it, not what it is an
/// extrusion of. `tests/fill_extrusion_tiles.rs` stands that layer on the same polygons for the
/// same reason.
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
  "layers": [
    { "id": "bg", "type": "background", "paint": { "background-color": "#eae6dd" } },
    {
      "id": "buildings",
      "type": "fill-extrusion",
      "source": "world",
      "source-layer": "water",
      "paint": { "fill-extrusion-height": 12, "fill-extrusion-opacity": 0.6 }
    }
  ]
}"##;

/// The address the fixtures are placed at.
///
/// The vector tile's own, and the DEM is placed there too: a DEM tile is a height field and the
/// one in `tests/dem-fixtures` is a real tile from elsewhere, so what it holds is heights and not
/// this address's heights. Which is all this needs -- the ground exists and raises what stands on
/// it.
const AT: &str = "0/0/0";

/// What one run of the tool put in its capture.
struct Written {
    /// Drawables the producer marked `ON_TERRAIN`.
    raised: usize,
    /// Extrusion drawables, roofs and walls alike.
    extrusions: usize,
    /// How many of those name the ground at the elevation's slot.
    on_the_ground: usize,
}

fn root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Runs the tool and reads back what its capture says about the ground.
fn written(with_dem: bool) -> Written {
    let root = root();
    let out = std::env::temp_dir().join(format!("capture-render-ground-{with_dem}.tslcap"));
    let style = std::env::temp_dir().join(format!("capture-render-ground-{with_dem}.json"));
    std::fs::write(&style, STYLE).expect("the style is written");

    let mut command = Command::new(env!("CARGO_BIN_EXE_capture-render"));
    command
        .arg("--style")
        .arg(&style)
        .arg("--tile")
        .arg(root.join("tests/mvt-fixtures/real-world-0-0-0.mvt"))
        .arg("--tile-at")
        .arg(AT)
        .arg("--source")
        .arg("world")
        .arg("--lon")
        .arg("0")
        .arg("--lat")
        .arg("0")
        .arg("--zoom")
        .arg("0")
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
    let mut written = Written {
        raised: 0,
        extrusions: 0,
        on_the_ground: 0,
    };
    for record in read.records() {
        let kind = u16::try_from(record.kind)
            .ok()
            .and_then(EnvelopeKind::from_repr);
        match kind {
            Some(EnvelopeKind::ViewUse) => {
                if let Some(use_) = ViewUse::from_bytes(record.record)
                    && use_.draw_flags.bits() & DrawFlags::ON_TERRAIN.bits() != 0
                {
                    written.raised += 1;
                }
            }
            Some(EnvelopeKind::GeometryAdd) => {
                let Some(add) = GeometryAdd::from_bytes(record.record) else {
                    continue;
                };
                let extrusion = add.builtin_shader == BuiltIn::FillExtrusionShader as i32
                    || add.builtin_shader == BuiltIn::FillExtrusionInstancedShader as i32;
                if extrusion {
                    written.extrusions += 1;
                    if names_the_ground(record.payload, add) {
                        written.on_the_ground += 1;
                    }
                }
            }
            _ => {}
        }
    }
    written
}

/// Whether a drawable names a texture at the slot a raised family samples the elevation at.
fn names_the_ground(payload: &[u8], add: GeometryAdd) -> bool {
    let size = core::mem::size_of::<TextureRef>();
    let start = add.texture_refs.offset as usize;
    (0..add.texture_refs.count as usize)
        .filter_map(|index| TextureRef::from_bytes(&payload[start + index * size..]))
        .any(|reference| reference.slot == TERRAIN_ELEVATION_SLOT)
}

#[test]
fn a_dem_raises_the_drawables_it_covers_and_what_stands_on_it_names_it() {
    let flat = written(false);
    assert_eq!(
        flat.raised, 0,
        "a terrain style with no DEM has nothing to raise from, so the capture is flat"
    );
    assert_eq!(
        flat.on_the_ground, 0,
        "and nothing names a ground that is not there"
    );
    assert!(
        flat.extrusions > 0,
        "the flat run drew no extrusion either, so the pair compares nothing"
    );

    let raised = written(true);
    assert!(
        raised.raised > 0,
        "the DEM's own tile carries a ground, and what stands on it is raised"
    );
    // The elevation is the texture a raised variant samples per vertex, so a raised drawable
    // without one is a set the consumer cannot write at all -- it draws nothing rather than
    // drawing flat. The fill, the line and the circle each named it and the extrusions did not,
    // which nothing could see until a consumer had a raised extrusion module to bind.
    //
    // Every one of them, not one: a roof and its walls are two drawables and they are encoded in
    // two places, so giving the ground to either alone leaves half the building unable to draw.
    assert!(raised.extrusions > 0, "the style drew no extrusion at all");
    assert_eq!(
        raised.on_the_ground, raised.extrusions,
        "{} of {} extrusion drawables name the ground they stand on",
        raised.on_the_ground, raised.extrusions
    );
}
