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

use tessella_capture_abi::envelope::{Extent, GeometryId, Rect16, TextureId, ViewId};
use tessella_capture_abi::generated::mbgl_enums::TexturePixelType;
use tessella_capture_abi::ring::{self, region_size};
use tessella_orchestrate::SlabArena;
use tessella_orchestrate::emit;
use tessella_orchestrate::frame::{self, Frame};
use tessella_orchestrate::texture;
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
            patterns: None,
        },
    )
    .expect("the frame emits");

    // A texture with a *rect list*, which the frame itself never produces here: the only
    // textures a styleless frame uploads are mbgl's two bootstraps, and both are whole-texture
    // uploads. The rect path is the one with a rule the header has to carry — rows strided by
    // `w * bytes-per-pixel`, and the pixel bytes accounting for exactly the rectangles named —
    // so leaving it unexercised would leave the interesting half of the record unproven.
    //
    // Two rectangles in opposite corners rather than one, because that is the case the list
    // exists for: a union over them uploads the whole atlas (§6.4).
    let damage = [
        Rect16 {
            x: 0,
            y: 0,
            w: 4,
            h: 2,
        },
        Rect16 {
            x: 60,
            y: 60,
            w: 2,
            h: 3,
        },
    ];
    let format = TexturePixelType::Alpha;
    let bytes: usize = damage
        .iter()
        .map(|rect| usize::from(rect.w) * usize::from(rect.h) * format.channels() as usize)
        .sum();
    let upload = texture::regions(
        TextureId(64),
        Extent {
            width: 64,
            height: 64,
        },
        format,
        &damage,
        &vec![0xA5; bytes],
    )
    .expect("two rectangles are within the cap");
    texture::write(&mut producer, &upload).expect("the upload writes");

    // A mesh, a retirement and a teardown, none of which a settled first frame produces. Each is
    // a record a mirror must act on and none of them draws anything, so a fixture without them
    // would leave the walks compiled and never run — the failure mode this whole test exists to
    // avoid, one level up.
    let mesh = GeometryId(4096);
    let encoded = emit::encode_mesh(&mut arena, mesh, b"glTF\x02\x00\x00\x00");
    emit::write_mesh(&mut producer, &encoded).expect("the mesh writes");
    emit::remove(&mut producer, mesh).expect("the retirement writes");

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

    // Every use names something that was added. The ABI's model is that a consumer looks an id
    // up and finds whichever kind of thing declared it; an id nothing declared is a use with no
    // answer, and the record itself calls that a protocol fault.
    //
    // The background is why this is asserted rather than assumed. It takes ids from the shared
    // space and emits a `ViewUse` for each, and §2.2 has the consumer synthesize its quad rather
    // than the producer send one -- so four uses named four ids no `GeometryAdd` ever declared,
    // and the earlier version of this consumer counted them without noticing.
    assert_eq!(
        counts.get("dangling_uses").copied(),
        Some(0),
        "a drawable names geometry that was never added: {counts:?}"
    );
}

/// The two records whose payloads are shaped unlike anything else's, read from C.
///
/// A texture's damage is a *fixed array with a count beside it*, and a stencil list is a span
/// whose count is in elements rather than in bytes. Both are places the header can be
/// insufficient by omission rather than by error: the struct is right and the rule for reading
/// it is written only in a Rust doc comment, which a C consumer never sees.
///
/// The failure each rule prevents is stated as an assertion rather than described. A consumer
/// reading the whole rectangle array takes whatever the tail of it holds as damage, and uploads
/// past the end of the surface — so every rectangle is checked to fall inside the texture it
/// damages. A consumer validating a stencil span as `offset + count` against `payload_len`
/// accepts a list running fifteen sixteenths past the end of the payload, because a
/// `tsl_stencil_tile` is sixty-eight bytes and not one.
///
/// These are the records a Fluorite mirror needs and the earlier consumer never touched: it
/// walked five of the twelve kinds, and the two it was furthest from proving were the two whose
/// payload rules are not expressible in a struct.
#[test]
fn c_reads_the_textures_and_the_stencil_tiles() {
    let dir = std::env::temp_dir().join(format!("tessella-c-payloads-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a working directory");
    let (ring, slabs, _) = emit_frame();
    let counts = run(&dir, &ring, &slabs);
    std::fs::remove_dir_all(&dir).ok();

    assert!(
        counts.get("textures").copied().unwrap_or(0) > 0,
        "the fixture emits no texture at all, so nothing here was exercised: {counts:?}"
    );
    assert!(
        counts.get("whole_texture_uploads").copied().unwrap_or(0) > 0,
        "the frame's textures are mbgl's two bootstraps, which are whole-texture uploads: \
         {counts:?}"
    );
    assert!(
        counts.get("texture_rects").copied().unwrap_or(0) >= 2,
        "the two-rectangle upload was not walked, so the strided path is unproven: {counts:?}"
    );
    assert_eq!(
        counts.get("texture_bad").copied(),
        Some(0),
        "a texture upload's byte count did not match the area it claims to cover, or a damage \
         rectangle fell outside the texture it damages: {counts:?}"
    );

    assert!(
        counts.get("stencils").copied().unwrap_or(0) > 0,
        "the fixture emits no stencil record, so nothing here was exercised: {counts:?}"
    );
    assert!(
        counts.get("stencil_tiles").copied().unwrap_or(0) > 0,
        "a stencil record naming no tiles masks nothing: {counts:?}"
    );
    assert_eq!(
        counts.get("stencil_bad").copied(),
        Some(0),
        "a stencil tile's matrix never came from a camera: {counts:?}"
    );
}

/// The lifecycle records, which a mirror must act on and a counter would not notice.
///
/// A `ViewDeclare` opens a scene and a `ViewUndeclare` tears it down; a `GeometryRemove` frees
/// GPU resources and a `ViewRelease` drops one view's claim on geometry another view may still
/// hold. None of them draws anything, which is why walking them is easy to leave out and
/// expensive to have left out: the symptom of ignoring a retirement is a leak, and the symptom
/// of ignoring a declaration is one view's drawables in another view's scene.
///
/// The consumer struck each id from its declared set as it was retired, so this also proves
/// every retirement names something that was added — the dangling-use fault seen from the other
/// end, and the one a consumer notices late because it frees nothing and then leaks whatever
/// really was added.
#[test]
fn c_reads_the_lifecycle_records() {
    let dir = std::env::temp_dir().join(format!("tessella-c-lifecycle-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a working directory");
    let (ring, slabs, _) = emit_frame();
    let counts = run(&dir, &ring, &slabs);
    std::fs::remove_dir_all(&dir).ok();

    assert!(
        counts.get("declares").copied().unwrap_or(0) > 0,
        "a frame declares the view its records belong to: {counts:?}"
    );
    assert_eq!(
        counts.get("view_bad").copied(),
        Some(0),
        "a view id outside the range the ABI reserves: {counts:?}"
    );
    assert_eq!(
        counts.get("remove_unknown").copied(),
        Some(0),
        "a retirement named geometry that was never added: {counts:?}"
    );
    assert_eq!(
        counts.get("mesh_unknown_format").copied(),
        Some(0),
        "a mesh arrived in a format the header does not name, which a consumer must skip: \
         {counts:?}"
    );

    // The frame under test is a single settled view, so it retires nothing and undeclares
    // nothing. Asserted rather than left unsaid: if these ever become non-zero here the frame
    // has changed shape, and the counters above stop meaning what they say.
    assert!(
        counts.get("meshes").copied().unwrap_or(0) > 0,
        "the mesh walk never ran: {counts:?}"
    );
    assert!(
        counts.get("mesh_bytes").copied().unwrap_or(0) > 0,
        "the mesh's slab reference resolved to nothing: {counts:?}"
    );
    assert!(
        counts.get("removes").copied().unwrap_or(0) > 0,
        "the retirement walk never ran: {counts:?}"
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
