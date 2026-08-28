//! The Fluorite mirror's own reader, run over a real frame.
//!
//! # Why this lives here
//!
//! The reader is C++ and lives in the `maplibre_fluorite` checkout, because that is where the
//! Filament half is. But it is only worth anything against a stream a *producer* wrote, and the
//! producer is here. So the test is here and the code under test is over there, the same way
//! `live_parity`'s tiles live in a server this repository does not contain.
//!
//! # What it proves that `c_consumer` does not
//!
//! `c_consumer` walks the stream and counts. This one is the code the mirror will actually run,
//! and it does the two things a counter never has to:
//!
//! It **joins**. tessella splits mbgl's one `DrawableAdd` into a shared `GeometryAdd` and a
//! per-view `ViewUse`, because four views over one tile send one add and four uses (§5.1). The
//! mirror wants the joined record back, and getting the join wrong is not a crash — it is one
//! view's geometry drawn with another view's layer index.
//!
//! And it **pairs the camera with its order**. §11.7 obliges a consumer to hold a camera until
//! its order epoch is held. Those are two records here and were one in mbgl, so the pairing is
//! the reader's to do, and a consumer that applied a camera against the wrong order draws one
//! frame of the previous painter order on every restyle.
//!
//! # Skipped rather than failed when the checkout is absent
//!
//! `TESSELLA_MIRROR_DIR` points at the `maplibre_fluorite` checkout; without it this reports and
//! returns. A missing sibling checkout is not a defect in this repository, and a test that
//! failed for it would be turned off and then stay off.

#![allow(clippy::print_stdout)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

mod fixture;

/// Where the mirror checkout is, if this machine has one.
fn mirror_dir() -> Option<PathBuf> {
    if let Ok(from_env) = std::env::var("TESSELLA_MIRROR_DIR") {
        let at = PathBuf::from(from_env);
        return at.is_dir().then_some(at);
    }
    // The layout on the machine this was developed on. Tried rather than required, so the
    // common case needs no configuration and the uncommon one is one variable.
    let guess = Path::new(concat!(env!("CARGO_MANIFEST_DIR")))
        .join("../../../maplibre-frontend/maplibre_fluorite");
    guess.is_dir().then(|| guess.clone())
}

/// Builds the probe, returning its path.
fn build_probe(mirror: &Path, dir: &Path) -> Option<PathBuf> {
    let source = mirror.join("native/test/tessella_reader_probe.cc");
    let reader = mirror.join("native/src/tessella_reader.cc");
    if !source.is_file() || !reader.is_file() {
        println!("no reader in {}; skipping", mirror.display());
        return None;
    }
    let out = dir.join("reader-probe");
    let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"))).join("../..");
    let compiler = std::env::var("CXX").unwrap_or_else(|_| "g++".to_string());
    let done = Command::new(compiler)
        .args(["-std=c++20", "-Wall", "-Wextra", "-Werror", "-O1"])
        .arg("-I")
        .arg(mirror.join("native/include"))
        .arg("-I")
        .arg(root.join("include"))
        .arg("-o")
        .arg(&out)
        .arg(&source)
        .arg(&reader)
        .output()
        .ok()?;
    assert!(
        done.status.success(),
        "the mirror's reader did not compile against this header:\n{}",
        String::from_utf8_lossy(&done.stderr)
    );
    Some(out)
}

/// The mirror's reader agrees with the producer about the frame it wrote.
#[test]
fn the_mirror_reads_a_real_frame() {
    let Some(mirror) = mirror_dir() else {
        println!("no maplibre_fluorite checkout; set TESSELLA_MIRROR_DIR to run this");
        return;
    };

    let dir = std::env::temp_dir().join(format!("tessella-mirror-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a working directory");
    let Some(probe) = build_probe(&mirror, &dir) else {
        std::fs::remove_dir_all(&dir).ok();
        return;
    };

    let (ring, slabs, emitted) = fixture::emit_frame();
    let ring_path = dir.join("ring.bin");
    let slab_path = dir.join("slabs.bin");
    std::fs::File::create(&ring_path)
        .and_then(|mut file| file.write_all(&ring))
        .expect("the ring writes");
    std::fs::File::create(&slab_path)
        .and_then(|mut file| file.write_all(&slabs))
        .expect("the slabs write");

    let done = Command::new(&probe)
        .arg(&ring_path)
        .arg(&slab_path)
        .output()
        .expect("the probe runs");
    std::fs::remove_dir_all(&dir).ok();
    assert!(
        done.status.success(),
        "the probe failed:\n{}",
        String::from_utf8_lossy(&done.stderr)
    );

    let counts: std::collections::BTreeMap<String, u64> = String::from_utf8_lossy(&done.stdout)
        .lines()
        .filter_map(|line| {
            let (name, value) = line.split_once(' ')?;
            Some((name.to_owned(), value.parse().ok()?))
        })
        .collect();

    // The join. One `GeometryAdd` and one `ViewUse` per drawable in this frame, so the count the
    // mirror sees is the count the producer emitted — and every joined record carries both
    // halves, which `joined_badly` is what checks.
    assert_eq!(
        counts.get("drawables").copied(),
        Some(emitted.drawables as u64),
        "the mirror joined a different number of drawables than the producer emitted: {counts:?}"
    );
    assert_eq!(
        counts.get("joined_badly").copied(),
        Some(0),
        "a drawable arrived with a shader but no geometry behind it: {counts:?}"
    );
    assert_eq!(
        counts.get("unresolved_indexes").copied(),
        Some(0),
        "an index buffer did not resolve against the slab region: {counts:?}"
    );
    assert!(
        counts.get("indices").copied().unwrap_or(0) > 0,
        "no indices resolved, so the geometry was never really read: {counts:?}"
    );

    // The camera and its order arrive as one thing at the sink, which is what lets a consumer
    // honour §11.7 at all.
    assert_eq!(
        counts.get("cameras").copied(),
        Some(1),
        "the frame's camera did not reach the sink: {counts:?}"
    );
    assert_eq!(
        counts.get("epoch_mismatch").copied(),
        Some(0),
        "a camera was paired with an order it does not name: {counts:?}"
    );
    assert_eq!(
        counts.get("orphan_orders").copied(),
        Some(0),
        "an order was delivered with no camera to commit it: {counts:?}"
    );

    // The frame is bracketed, and the camera is what closes it. Two opened and one closed, which
    // is right rather than a discrepancy: the fixture appends a texture, a mesh and a retirement
    // *after* `frame::emit` has written the camera, so from the stream's point of view those are
    // the start of a second frame that no camera has committed yet.
    //
    // That is a real situation and not an artefact — a consumer draining while the producer is
    // mid-frame sees exactly this — and it is worth asserting, because the failure it rules out
    // is a reader that closes a frame it never saw a camera for. Such a reader would hand the
    // mirror a half-built frame to draw.
    assert_eq!(
        counts.get("frames_begun").copied(),
        Some(2),
        "the records after the camera open a second frame: {counts:?}"
    );
    assert_eq!(
        counts.get("frames_ended").copied(),
        Some(1),
        "only the frame with a camera is committed: {counts:?}"
    );

    // Everything else the mirror must act on, and the payload rules it must read correctly.
    assert_eq!(
        counts.get("texture_bad").copied(),
        Some(0),
        "a texture upload's bytes did not account for the area it covers: {counts:?}"
    );
    assert!(
        counts.get("whole_texture").copied().unwrap_or(0) > 0
            && counts.get("rects").copied().unwrap_or(0) > 0,
        "both texture shapes should be exercised by this frame: {counts:?}"
    );
    assert!(
        counts.get("stencil_tiles").copied().unwrap_or(0) > 0,
        "the stencil tiles did not resolve: {counts:?}"
    );
    assert!(
        counts.get("meshes").copied().unwrap_or(0) > 0
            && counts.get("mesh_bytes").copied().unwrap_or(0) > 0,
        "the mesh did not reach the sink with its bytes: {counts:?}"
    );
    assert!(
        counts.get("removes").copied().unwrap_or(0) > 0,
        "the retirement did not reach the sink: {counts:?}"
    );
    assert_eq!(
        counts.get("unknown_records").copied(),
        Some(0),
        "the mirror met a record kind it does not know: {counts:?}"
    );
}
