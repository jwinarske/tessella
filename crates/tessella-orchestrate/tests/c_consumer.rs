//! A frame, read back by C that knows only the generated header.
//!
//! # What this is for
//!
//! Every other consumer in this repository is Rust, in process, sharing the arena and the type
//! definitions with the producer. Such a consumer cannot fail in the ways a real one does: it
//! never notices a field the header does not describe, a layout rule that lives only in a Rust
//! doc comment, or a handle that resolves against a table nothing says how to index. `probe.c`
//! did not close that gap -- it takes three `sizeof`s, and CI compiles it with `-fsyntax-only`,
//! so no C had ever run against this ABI at all.
//!
//! So this hands `tools/abi-header/consumer.c` two byte buffers -- a ring region and a packed
//! slab region -- and checks that what it counts agrees with what the producer said it wrote.
//! The consumer includes the header and nothing else of ours.
//!
//! # Why the numbers are the assertion
//!
//! Agreement on counts is what proves the header sufficient. To arrive at the same number of
//! geometries the C has to walk records by `total_len`, skip the wrap records, find each
//! record's fixed part and its payload across the `PAYLOAD_ALIGN` gap, and read a span whose
//! offset is relative to a payload it had to locate itself. To resolve a slab reference it has
//! to index the table by the handle. Any of those rules being absent from the header shows up
//! here as a wrong count or a failed resolve, not as a compile error.

use std::io::Write as _;
use std::process::Command;

use tessella_capture_abi::envelope::ViewId;
use tessella_capture_abi::ring::{self, region_size};
use tessella_orchestrate::SlabArena;
use tessella_orchestrate::frame::{self, Frame};
use tessella_orchestrate::tile::{TileId, build_mvt_tile, build_sourceless};
use tessella_source::mvt::Tile;
use tessella_style::Style;
use tessella_style::light::Light;
use tessella_tile::camera;
use tessella_tile::cover::{self, ViewTransform};

const REAL_TILE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/real-world-0-0-0.mvt");

const STYLE: &str = r##"{
  "version": 8,
  "sources": {"src": {"type": "vector", "tiles": []}},
  "layers": [
    {"id": "bg", "type": "background", "paint": {"background-color": "#101418"}},
    {"id": "sea", "type": "fill", "source": "src", "source-layer": "water",
     "paint": {"fill-color": "#20344c"}},
    {"id": "banks", "type": "line", "source": "src", "source-layer": "water",
     "paint": {"line-color": "#88a", "line-width": 1.5}},
    {"id": "blocks", "type": "fill-extrusion", "source": "src", "source-layer": "water",
     "paint": {"fill-extrusion-height": 20, "fill-extrusion-opacity": 0.8}}
  ]
}"##;

/// Ring capacity. A power of two, as the control block requires.
const CAPACITY: usize = 1 << 24;

/// Emits one frame into a freestanding region and returns it with the packed slabs.
///
/// `ring::init` over a buffer of this test's own, rather than `Ring::new`, because what a C
/// consumer is handed is a region -- and the point here is to produce exactly the bytes that
/// would cross a mapping, not a Rust object that happens to contain them.
fn emit_frame() -> (Vec<u8>, Vec<u8>, frame::Emitted) {
    let style = Style::parse(STYLE).expect("the style parses");
    let view = camera::settled(&ViewTransform {
        longitude: 0.0,
        latitude: 0.0,
        zoom: 3.0,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 45.0,
    });
    let tiles = cover::cover(&view).expect("covers");
    let decoded = Tile::decode(REAL_TILE).expect("the fixture decodes");

    let mut buckets = Vec::new();
    for tile in &tiles {
        let id = TileId::new(tile.z, tile.x, tile.y);
        let mut built = build_mvt_tile(&style, "src", id, &decoded).expect("the tile builds");
        built.extend(build_sourceless(&style, id).expect("the background builds"));
        built.sort_by_key(|bucket| bucket.layer_index);
        buckets.push((id, built));
    }

    // Eight-aligned by construction, which `init` requires.
    let mut region = vec![0u64; region_size(CAPACITY).div_ceil(8)];
    // SAFETY: the buffer is `region_size(CAPACITY)` bytes, eight-aligned because it is a
    // `Vec<u64>`, outlives both halves, and nothing else touches it.
    let (mut producer, _consumer) =
        unsafe { ring::init(region.as_mut_ptr().cast::<u8>(), CAPACITY) };

    let mut arena = SlabArena::new();
    let emitted = frame::emit(
        &mut producer,
        &mut arena,
        &Frame {
            style: &style,
            view: &view,
            view_id: ViewId(0),
            tiles: &tiles,
            buckets: &buckets,
            light: &Light::default(),
            fonts: None,
        },
    )
    .expect("the frame emits");
    arena.seal();

    let bytes = region
        .iter()
        .flat_map(|word| word.to_ne_bytes())
        .take(region_size(CAPACITY))
        .collect();
    (bytes, arena.pack(), emitted)
}

/// Builds the consumer, returning the executable's path.
fn build_consumer(dir: &std::path::Path) -> std::path::PathBuf {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("the workspace root");
    let out = dir.join("consumer");

    let compiler = ["cc", "gcc", "clang"]
        .into_iter()
        .find(|name| {
            Command::new(name)
                .arg("--version")
                .output()
                .is_ok_and(|done| done.status.success())
        })
        .expect("a C compiler: this test exists to run C against the header");

    let done = Command::new(compiler)
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror", "-O1"])
        .arg("-I")
        .arg(root.join("include"))
        .arg("-o")
        .arg(&out)
        .arg(root.join("tools/abi-header/consumer.c"))
        .output()
        .expect("the compiler runs");
    assert!(
        done.status.success(),
        "the consumer did not compile:\n{}",
        String::from_utf8_lossy(&done.stderr)
    );
    out
}

/// Runs the consumer and returns its printed counters.
fn run(
    dir: &std::path::Path,
    ring: &[u8],
    slabs: &[u8],
) -> std::collections::BTreeMap<String, u64> {
    let ring_path = dir.join("ring.bin");
    let slab_path = dir.join("slabs.bin");
    std::fs::File::create(&ring_path)
        .and_then(|mut file| file.write_all(ring))
        .expect("the ring writes");
    std::fs::File::create(&slab_path)
        .and_then(|mut file| file.write_all(slabs))
        .expect("the slabs write");

    let consumer = build_consumer(dir);
    let done = Command::new(&consumer)
        .arg(&ring_path)
        .arg(&slab_path)
        .output()
        .expect("the consumer runs");
    assert!(
        done.status.success(),
        "the consumer failed:\n{}{}",
        String::from_utf8_lossy(&done.stdout),
        String::from_utf8_lossy(&done.stderr)
    );

    String::from_utf8_lossy(&done.stdout)
        .lines()
        .filter_map(|line| {
            let (name, value) = line.split_once(' ')?;
            Some((name.to_owned(), value.parse().ok()?))
        })
        .collect()
}

/// A C consumer, given only the header, counts the frame the producer says it wrote.
#[test]
fn c_reads_the_frame() {
    let dir = std::env::temp_dir().join(format!("tessella-c-consumer-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a working directory");
    let (ring, slabs, emitted) = emit_frame();
    let counts = run(&dir, &ring, &slabs);
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(
        counts.get("geometries").copied(),
        Some(emitted.geometries as u64),
        "C counted a different number of geometries than the producer emitted: {counts:?}"
    );
    assert_eq!(
        counts.get("drawables").copied(),
        Some(emitted.drawables as u64),
        "C counted a different number of drawables than the producer emitted: {counts:?}"
    );
    assert_eq!(
        counts.get("unresolved").copied(),
        Some(0),
        "a slab reference did not resolve against the packed region: {counts:?}"
    );
    assert!(
        counts.get("resolved_bytes").copied().unwrap_or(0) > 0,
        "nothing resolved, so the region was never really read: {counts:?}"
    );
    assert!(
        counts.get("order_entries").copied().unwrap_or(0) >= emitted.drawables as u64,
        "the order has an entry per drawable at least: {counts:?}"
    );
}

/// The uniform buffers and the camera, read from C and checked against the producer.
///
/// # Why these two and not the other nine
///
/// Counting geometry proves the record walk. It does not prove that the *contents* of a record
/// are reachable, and for a mirror the contents that matter are these: DR-16 consolidates one
/// uniform buffer per (view, layer) and indexes it by the order entry's `ubo_index`, and the
/// camera carries the projection every drawable is transformed by. A consumer that could read
/// geometry and not these would register a whole scene and draw none of it.
///
/// The camera is also the only record whose fields are almost all `double`. A misread offset
/// there does not fail loudly — it lands in the next field or in padding and yields a plausible
/// number, which is why the check is against values the producer computed rather than against
/// the record merely being present.
#[test]
fn c_reads_the_uniforms_and_the_camera() {
    let dir = std::env::temp_dir().join(format!("tessella-c-uniforms-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a working directory");
    let (ring, slabs, _) = emit_frame();
    let counts = run(&dir, &ring, &slabs);
    std::fs::remove_dir_all(&dir).ok();

    assert!(
        counts.get("ubos").copied().unwrap_or(0) > 0,
        "no uniform buffers were read: {counts:?}"
    );
    assert!(
        counts.get("ubo_bytes").copied().unwrap_or(0) > 0,
        "the buffers were found but their spans resolved to nothing: {counts:?}"
    );
    assert_eq!(
        counts.get("ubo_truncated").copied(),
        Some(0),
        "a buffer's span ran past the payload that carries it: {counts:?}"
    );
    // `GlobalPaintParams` is written frame-wide, at layer -1. A consumer that read the layer
    // index as unsigned would see 4294967295 and never find one.
    assert!(
        counts.get("ubo_frame_wide").copied().unwrap_or(0) > 0,
        "no frame-wide buffer was seen, so layer_index was read unsigned: {counts:?}"
    );

    assert_eq!(
        counts.get("cameras").copied(),
        Some(1),
        "exactly one camera closes a frame: {counts:?}"
    );
    assert_eq!(
        counts.get("camera_bad").copied(),
        Some(0),
        "the projection read back as zeroes, which is a misread offset: {counts:?}"
    );

    // Against what the producer put there, not merely against being non-zero. The view is
    // pitched at forty-five degrees and the light is the style default.
    assert_eq!(
        counts.get("camera_pitch_milli").copied(),
        Some(45_000),
        "C read a different pitch than the view was built with: {counts:?}"
    );
    assert_eq!(
        counts.get("camera_light_milli").copied(),
        Some(500),
        "the light's intensity is mbgl's default of one half: {counts:?}"
    );
    // The epoch ties the camera to the order it was computed against, which is the rule a
    // consumer needs to avoid drawing one frame's order against another's camera.
    assert!(
        counts.get("camera_epoch").copied().unwrap_or(0) > 0,
        "the camera names no order epoch: {counts:?}"
    );
    assert!(
        counts.get("camera_proj0_micro").copied().unwrap_or(0) != 0,
        "the projection's first element is zero: {counts:?}"
    );
}

/// The same walk across the buffer's wrap, where the protocol is subtlest.
///
/// A record never straddles the end of the data region: one that would not fit is preceded by a
/// skip record covering the remainder, carrying no envelope and no payload. A consumer that
/// missed that rule would read a record header out of the tail of one record and the head of
/// another, and the numbers would be nonsense rather than an error.
///
/// The header states the rule in a sentence and defines `TSL_RECORD_FLAG_SKIP`. Whether that is
/// enough to implement it is what this asks. The frame above never wraps -- its ring is sized so
/// it does not -- so without this the flag would be a constant nothing had ever set.
#[test]
fn c_reads_across_the_wrap() {
    // Small enough that a few hundred records go round several times.
    const SMALL: usize = 1 << 12;
    // Chosen so records do not divide the capacity evenly: a whole number of records per lap
    // would put every wrap exactly on a record boundary and never need a skip at all.
    const PAYLOAD: [u8; 8] = [7; 8];

    let mut region = vec![0u64; region_size(SMALL).div_ceil(8)];
    // SAFETY: as above -- sized, eight-aligned, outlives the halves, untouched by anything else.
    let (mut producer, mut consumer) =
        unsafe { ring::init(region.as_mut_ptr().cast::<u8>(), SMALL) };

    let release = tessella_capture_abi::envelope::ViewRelease {
        geometry: tessella_capture_abi::envelope::GeometryId(1),
        view: ViewId(0),
        _pad: 0,
    };
    use tessella_capture_abi::envelope::WireRecord as _;

    // Round the buffer several times, draining as we go so there is always room.
    let mut written = 0u64;
    for round in 0..400 {
        if producer
            .write(
                tessella_capture_abi::EnvelopeKind::ViewRelease,
                release.as_bytes(),
                &PAYLOAD,
            )
            .is_ok()
        {
            written += 1;
        }
        if round % 3 == 2 {
            while let Some(record) = consumer.peek() {
                let consumed = record.consumed();
                consumer.advance(consumed);
            }
        }
    }
    assert!(written > 300, "the ring took {written} records");

    // Drain, then leave a known number in flight for C to find.
    while let Some(record) = consumer.peek() {
        let consumed = record.consumed();
        consumer.advance(consumed);
    }
    let mut live = 0u64;
    for _ in 0..40 {
        producer
            .write(
                tessella_capture_abi::EnvelopeKind::ViewRelease,
                release.as_bytes(),
                &PAYLOAD,
            )
            .expect("room for the last batch");
        live += 1;
    }

    let bytes: Vec<u8> = region
        .iter()
        .flat_map(|word| word.to_ne_bytes())
        .take(region_size(SMALL))
        .collect();

    let dir = std::env::temp_dir().join(format!("tessella-c-wrap-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a working directory");
    let counts = run(&dir, &bytes, &SlabArena::new().pack());
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(
        counts.get("records").copied(),
        Some(live),
        "C did not find the records left in flight: {counts:?}"
    );
    assert!(
        counts.get("skips").copied().unwrap_or(0) > 0,
        "the window never crossed the wrap, so the skip rule went untested: {counts:?}"
    );
}
