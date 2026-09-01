//! The whole chain, joined: a style in, batched draws out.
//!
//! # Why this is the test that matters now
//!
//! Every link had a test and none of them was a map. `Reader` turns records into drawables,
//! `DrawList` groups an order into batches, the FFI runs a map — each correct on its own, and
//! together still nothing, because nothing ran them as one thing. `tsf::Host` is that piece, and
//! this is what says it works.
//!
//! What it deliberately does not need is a GPU. The renderer in the probe counts instead of
//! uploading, which is the whole reason the join was kept free of Filament: the part that can be
//! wrong without a device is exactly the part a device would make untestable.

use std::path::PathBuf;
use std::process::Command;

/// A background over an inline GeoJSON polygon.
///
/// Inline rather than served, so this measures the join and not a tile server: a GeoJSON document
/// written into the style is resolved without fetching anything, and tiled on this side. The
/// polygon is what makes the frame real — it tessellates, so there are vertices, indices, an
/// order and something to batch.
///
/// A background *alone* would not do. It draws per tile of the cover and the cover is filled from
/// tiles that have buckets, so a style with no sources has nothing in its drawn set and paints
/// nothing at all — see the note in the assertions below, which is a finding rather than a
/// premise of this test.
const STYLE: &str = r##"{
  "version": 8,
  "sources": {
    "shapes": {
      "type": "geojson",
      "data": {
        "type": "FeatureCollection",
        "features": [
          {
            "type": "Feature",
            "properties": {"kind": "block"},
            "geometry": {
              "type": "Polygon",
              "coordinates": [[[-4.0, 49.0], [4.0, 49.0], [4.0, 54.0], [-4.0, 54.0], [-4.0, 49.0]]]
            }
          },
          {
            "type": "Feature",
            "properties": {"kind": "dot"},
            "geometry": {"type": "Point", "coordinates": [-0.11, 51.505]}
          }
        ]
      }
    }
  },
  "layers": [
    {"id": "bg", "type": "background", "paint": {"background-color": "#101418"}},
    {"id": "block", "type": "fill", "source": "shapes",
     "paint": {"fill-color": "#3050c0"}},
    {"id": "block-outline", "type": "fill", "source": "shapes",
     "paint": {"fill-color": "#3050c0", "fill-outline-color": "#ffffff"}},
    {"id": "edge", "type": "line", "source": "shapes",
     "paint": {"line-color": "#88aacc", "line-width": 2.0}},
    {"id": "dot", "type": "circle", "source": "shapes",
     "paint": {"circle-color": "#ffcc00", "circle-radius": 4.0}}
  ]
}"##;

fn profile_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("the test binary has a path");
    exe.parent()
        .and_then(std::path::Path::parent)
        .expect("target/<profile>/deps/<test>")
        .to_path_buf()
}

/// Builds the staticlib this links against, and returns it.
///
/// `cargo test` builds the rlib the harness needs; the staticlib is a *separate artefact of the
/// same crate* and is not rebuilt by a test run. Linking whatever happens to be on disk means a
/// test that silently exercises a library from an earlier edit -- which cost real time here
/// before it was understood, because every run reported the same numbers however the Rust
/// changed. So it is built rather than found.
///
/// Nested cargo is safe at this point: the outer invocation has finished building and released
/// its lock before any test runs.
fn staticlib() -> Option<PathBuf> {
    let built = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .arg("build")
        .arg("-p")
        .arg("tessella-ffi")
        .arg("--all-features")
        .output()
        .ok()?;
    if !built.status.success() {
        eprintln!(
            "skipping: the staticlib did not build:\n{}",
            String::from_utf8_lossy(&built.stderr)
        );
        return None;
    }
    let path = profile_dir().join("libtessella_ffi.a");
    path.exists().then_some(path)
}

#[test]
fn a_style_reaches_a_backend_as_batched_draws() {
    let root = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
    let consumer = PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../tessella_fluorite"
    ));
    if !consumer.join("native/src/host.cc").exists() {
        eprintln!("skipping: no consumer checkout at {}", consumer.display());
        return;
    }
    let Some(staticlib) = staticlib() else {
        return;
    };

    let dir = std::env::temp_dir().join(format!("tessella-host-join-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let style_path = dir.join("style.json");
    std::fs::write(&style_path, STYLE).expect("the style is written");
    let out = dir.join("host_probe");

    let compiler = std::env::var("CXX").unwrap_or_else(|_| "g++".to_string());
    let done = Command::new(compiler)
        .arg("-std=c++17")
        .arg("-Wall")
        .arg("-Wextra")
        .arg("-I")
        .arg(consumer.join("native/include"))
        .arg("-I")
        .arg(root.join("include"))
        .arg("-o")
        .arg(&out)
        .arg(consumer.join("native/test/host_probe.cc"))
        .arg(consumer.join("native/src/host.cc"))
        .arg(consumer.join("native/src/reader.cc"))
        .arg(consumer.join("native/src/drawlist.cc"))
        .arg(&staticlib)
        .arg("-lpthread")
        .arg("-ldl")
        .arg("-lm")
        .output()
        .expect("the compiler runs");

    assert!(
        done.status.success(),
        "the host did not build:\n{}",
        String::from_utf8_lossy(&done.stderr)
    );

    let run = Command::new(&out)
        .arg(&style_path)
        .output()
        .expect("the probe runs");
    let stderr = String::from_utf8_lossy(&run.stderr).into_owned();
    std::fs::remove_dir_all(&dir).ok();
    assert!(
        run.status.success(),
        "the probe exited {}:\n{stderr}",
        run.status
    );

    let said: std::collections::BTreeMap<String, i64> = String::from_utf8_lossy(&run.stdout)
        .lines()
        .filter_map(|line| {
            let (name, value) = line.split_once(' ')?;
            Some((name.to_owned(), value.parse().ok()?))
        })
        .collect();

    let got = |name: &str| said.get(name).copied().unwrap_or(-1);

    assert_eq!(
        got("created"),
        1,
        "the host did not create a map: {said:?} "
    );
    assert_eq!(
        got("readiness"),
        2,
        "a sourceless map did not reach TESSELLA_READY: {said:?} {stderr}"
    );
    assert_eq!(got("last_result"), 0, "a call failed: {said:?}");

    // The chain carried something. Records were read, geometry reached the backend, and the
    // frames were bracketed -- a backend that is never told a frame began cannot begin one.
    assert!(got("records") > 0, "no records were read: {said:?}");
    assert!(got("frames") > 0, "no frame was begun: {said:?}");
    assert!(
        got("geometries") > 0,
        "no geometry reached the backend: {said:?}"
    );
    assert!(got("vertices") > 0, "nothing was tessellated: {said:?}");

    // And it arrived as *batches*, which is what the backend actually draws.
    assert!(got("batches") > 0, "nothing was batched: {said:?}");
    assert!(
        got("drawn") >= got("batches"),
        "a batch drew fewer geometries than one: {said:?}"
    );

    // The three ways the join could be silently wrong.
    assert_eq!(
        got("unknown_in_batch"),
        0,
        "a batch named geometry the backend was never given, which is a dangling buffer: {said:?}"
    );
    assert_eq!(
        got("malformed"),
        0,
        "a batch's geometry and slot lists disagree: {said:?}"
    );
    assert_eq!(
        got("unresolved_indexes"),
        0,
        "an index buffer did not resolve against the slab region: {said:?}"
    );

    // The material inventory: which shader families a real frame actually asks for.
    //
    // This is what scopes the Filament backend. The ABI declares thirty-five families and this
    // style needs five, one per kind of layer it draws -- so the question "how many materials
    // must be authored" has a measured answer per style rather than a worst case. Asserted by id
    // because a family appearing or disappearing is a change in what the backend must support,
    // and that should not happen quietly.
    //
    //   3  background        the layer that draws behind everything
    //   5  circle            the point feature
    //   11 fill              the polygon
    //   12 fill outline      the second fill layer's `fill-outline-color`
    //   25 line              the polygon's edge
    for (id, what) in [
        (3, "background"),
        (5, "circle"),
        (11, "fill"),
        (12, "fill outline"),
        (25, "line"),
    ] {
        assert!(
            got(&format!("shader_{id}")) >= 1,
            "no batch used the {what} shader ({id}), which this style draws: {said:?}"
        );
    }
    assert_eq!(
        got("shader_families"),
        5,
        "the set of shader families this style needs changed, which changes what a backend must \
         author: {said:?}"
    );

    // The background is here only because something else is. A style whose layers draw from no
    // source paints nothing at all -- the draw loop reaches sourceless layers through tiles that
    // have buckets -- which is recorded in plan.md §16 as a §13.2 question rather than fixed.
    assert!(
        got("shader_3") >= 1,
        "the background did not draw even beside a source that did: {said:?}"
    );

    // The tail moved, so the producer can reuse what the backend is done with. A host that never
    // retires stalls the producer, and nothing else here would notice.
    assert_eq!(
        got("cursor_advanced"),
        1,
        "the reader never advanced, so nothing was ever released: {said:?}"
    );
}
