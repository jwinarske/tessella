// SPDX-License-Identifier: BSD-2-Clause
//! A sprite sheet on the command line puts a pattern in the capture, and an atlas under it.
//!
//! What `--sprite` is for is not visible in the file it writes unless the pair is compared: a
//! capture written without it is a perfectly good capture of the *same style*, with the same layers
//! and the same features, in which every patterned layer has quietly become a plain one. The
//! producer's own arm says so --
//!
//! > A pattern binds the atlas and a different shader; without one the layer draws as a plain fill,
//! > which is what a frame with no sprites fetched should do.
//!
//! -- and that is right for a frame whose sheet has not arrived. It is wrong as the *only* thing a
//! capture tool can write, because then nothing downstream of this tool has ever seen a resolved
//! pattern: no `FillPatternShader`, no atlas, no `TextureRef` naming one.
//!
//! So the check is the pair, and the two halves are the same run with one flag.
//!
//! Run as a process rather than as a function, for `tests/a_ground.rs`'s reason: what is under test
//! is the tool's own assembly of a frame, and none of that is reachable from a library.

use std::process::Command;

use tessella_capture_abi::envelope::{
    AttributeDesc, GeometryAdd, TextureRef, TextureUpdate, WireRecord as _,
};
use tessella_capture_abi::{BuiltIn, EnvelopeKind};

/// A style with a pattern, a sprite to resolve it against, and a plain layer beside it.
///
/// The `sprite` URL is never fetched -- this tool has no transport and takes the files instead --
/// but it has to be there: a style that names no sprite is one `--sprite` is refused for, since a
/// sheet the style never asked for would resolve patterns no real host could.
///
/// The water layer is the fixture's own, as `tests/a_ground.rs` uses it, and the pattern is
/// `grass_pattern` because `tests/sprite-fixtures/emerald.json` holds one of that name.
///
/// Twice over, because a pattern resolves in two places and a constant one reaches only the
/// second. `meadow` names it outright, which the *frame* resolves into the layer's tile properties;
/// `varied` names an expression over a feature's own field, which makes it data-driven -- and a
/// data-driven pattern is a pair of rectangles per *feature*, resolved while the bucket is built.
/// A tool that passed its sheet to the frame and not to the bucket build would write a capture that
/// looks right until something reads the vertices.
const STYLE: &str = r##"{
  "version": 8,
  "sprite": "https://example.invalid/sprites/emerald",
  "sources": {
    "world": { "type": "vector", "url": "http://127.0.0.1:1/world.json" }
  },
  "layers": [
    { "id": "bg", "type": "background", "paint": { "background-color": "#eae6dd" } },
    {
      "id": "meadow",
      "type": "fill",
      "source": "world",
      "source-layer": "water",
      "paint": { "fill-pattern": "grass_pattern", "fill-opacity": 0.9 }
    },
    {
      "id": "varied",
      "type": "fill",
      "source": "world",
      "source-layer": "water",
      "paint": {
        "fill-pattern": ["coalesce", ["get", "pattern"], "grass_pattern"],
        "fill-opacity": 0.5
      }
    }
  ]
}"##;

/// The same style with its `sprite` removed, for the refusal.
const NO_SPRITE: &str = r##"{
  "version": 8,
  "sources": {
    "world": { "type": "vector", "url": "http://127.0.0.1:1/world.json" }
  },
  "layers": [
    { "id": "bg", "type": "background", "paint": { "background-color": "#eae6dd" } }
  ]
}"##;

/// The address the fixture tile is placed at.
const AT: &str = "0/0/0";

/// What one run of the tool put in its capture.
struct Written {
    /// Shaders the drawables name, by `builtin_shader`.
    shaders: Vec<i32>,
    /// Every texture declared, as `(id, width, height)`.
    textures: Vec<(u64, u32, u32)>,
    /// Texture ids the drawables name.
    bound: Vec<u64>,
    /// Attribute ids the drawables bind, which is where a data-driven pattern shows up.
    attributes: Vec<u32>,
}

fn root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Runs the tool, with the sheet or without, and reads back what its capture names.
fn written(with_sprite: bool) -> Written {
    let root = root();
    let out = std::env::temp_dir().join(format!("capture-render-sprite-{with_sprite}.tslcap"));
    let style = std::env::temp_dir().join(format!("capture-render-sprite-{with_sprite}.json"));
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
    if with_sprite {
        command
            .arg("--sprite")
            .arg(root.join("tests/sprite-fixtures/emerald"));
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
        shaders: Vec::new(),
        textures: Vec::new(),
        bound: Vec::new(),
        attributes: Vec::new(),
    };
    for record in read.records() {
        let kind = u16::try_from(record.kind)
            .ok()
            .and_then(EnvelopeKind::from_repr);
        match kind {
            Some(EnvelopeKind::GeometryAdd) => {
                let Some(add) = GeometryAdd::from_bytes(record.record) else {
                    continue;
                };
                written.shaders.push(add.builtin_shader);
                let stride = core::mem::size_of::<AttributeDesc>();
                let at = add.attrs.offset as usize;
                for index in 0..add.attrs.count as usize {
                    if let Some(desc) =
                        AttributeDesc::from_bytes(&record.payload[at + index * stride..])
                    {
                        written.attributes.push(desc.attr_id);
                    }
                }
                let size = core::mem::size_of::<TextureRef>();
                let start = add.texture_refs.offset as usize;
                for index in 0..add.texture_refs.count as usize {
                    if let Some(reference) =
                        TextureRef::from_bytes(&record.payload[start + index * size..])
                    {
                        written.bound.push(reference.texture.0);
                    }
                }
            }
            Some(EnvelopeKind::TextureUpdate) => {
                if let Some(update) = TextureUpdate::from_bytes(record.record) {
                    written.textures.push((
                        update.texture.0,
                        update.size.width,
                        update.size.height,
                    ));
                }
            }
            _ => {}
        }
    }
    written
}

/// The atlas the sheet is packed into, which is `sprite::ATLAS_SIZE` square.
const ATLAS: u32 = 1024;

#[test]
fn a_sheet_resolves_the_pattern_and_the_atlas_goes_up_with_it() {
    let flat = written(false);
    assert!(
        flat.shaders.contains(&(BuiltIn::FillShader as i32)),
        "the patterned layer did not draw at all without a sheet: {:?}",
        flat.shaders
    );
    assert!(
        !flat.shaders.contains(&(BuiltIn::FillPatternShader as i32)),
        "a pattern resolved with no sheet to resolve it against"
    );
    assert!(
        !flat
            .textures
            .iter()
            .any(|(_, width, height)| (*width, *height) == (ATLAS, ATLAS)),
        "an atlas went up with no sheet: {:?}",
        flat.textures
    );

    let sprited = written(true);
    assert!(
        sprited
            .shaders
            .contains(&(BuiltIn::FillPatternShader as i32)),
        "the sheet holds `grass_pattern` and the layer still drew plain: {:?}",
        sprited.shaders
    );

    // The atlas itself, at the id the producer publishes for it. Checked by id as well as by size,
    // because the id is the half a consumer has to agree with: the first capture written with a
    // sheet declared it as the `1x1` placeholder's, which a consumer refuses as one texture at two
    // sizes. See `frame::SPRITE_ATLAS_ID`.
    let atlas = tessella_orchestrate::frame::SPRITE_ATLAS_ID.0;
    assert!(
        sprited.textures.contains(&(atlas, ATLAS, ATLAS)),
        "no {ATLAS}x{ATLAS} atlas at {atlas:#x}: {:?}",
        sprited.textures
    );
    assert!(
        sprited.bound.contains(&atlas),
        "the atlas went up and no drawable names it: {:?}",
        sprited.bound
    );

    // And the data-driven layer's rectangles, which are the bucket build's half rather than the
    // frame's: `idFillPatternFromVertexAttribute` is a *per-feature* pair, bound only where the
    // lookup resolved one while the bucket was being built. A sheet handed to the frame alone
    // leaves this out and everything above still passes.
    const PATTERN_FROM: u32 = 4;
    assert!(
        sprited.attributes.contains(&PATTERN_FROM),
        "no drawable binds the per-feature pattern rectangles: {:?}",
        sprited.attributes
    );
    assert!(
        !flat.attributes.contains(&PATTERN_FROM),
        "a per-feature pattern resolved with no sheet: {:?}",
        flat.attributes
    );

    // And the placeholders are still there and still themselves. They are written before any style
    // content exists, so an atlas id landing on one is not a texture shared but a texture declared
    // twice -- which is what this flag found.
    for (id, width, height) in [(0, 0, 0), (1, 1, 1)] {
        assert!(
            sprited.textures.contains(&(id, width, height)),
            "the {width}x{height} placeholder at {id} is not in the capture: {:?}",
            sprited.textures
        );
    }
}

/// A sheet for a style that names none is refused rather than used.
///
/// `--dem`'s rule, for the same reason: the tool would otherwise read both files, pack an atlas,
/// and write a capture whose layers resolved patterns against a sheet no real host would have
/// fetched.
#[test]
fn a_sheet_without_a_sprite_in_the_style_is_refused() {
    let root = root();
    let style = std::env::temp_dir().join("capture-render-no-sprite.json");
    std::fs::write(&style, NO_SPRITE).expect("the style is written");

    let run = Command::new(env!("CARGO_BIN_EXE_capture-render"))
        .arg("--style")
        .arg(&style)
        .arg("--tile")
        .arg(root.join("tests/mvt-fixtures/real-world-0-0-0.mvt"))
        .arg("--sprite")
        .arg(root.join("tests/sprite-fixtures/emerald"))
        .arg("--capture")
        .arg(std::env::temp_dir().join("capture-render-no-sprite.tslcap"))
        .output()
        .expect("the tool runs");

    assert!(!run.status.success(), "the sheet was accepted");
    let said = String::from_utf8_lossy(&run.stderr);
    assert!(
        said.contains("names no sprite"),
        "refused for another reason: {said}"
    );
}
