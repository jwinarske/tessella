//! The producer in one process, the consumer in another, coupled by nothing but the ring.
//!
//! # The claim being tested
//!
//! §3.5 records process isolation as a latent option: "the frontend's only process coupling is
//! the ring, so promoting staticlib-in-mirror to its own process (ring over shm) is a linker
//! change, not a redesign", and it is precluded by nothing in the ABI because "slab handles are
//! offsets" rather than pointers. Every word of that was reasoning about the design. Nothing had
//! ever run it.
//!
//! `slab_region` closed half the gap — a handle resolves against a packed region rather than a
//! `Vec` of `Arc`s — and `c_consumer` closed another half, walking a stream with nothing but the
//! generated header. Both hand the far side a finished buffer. Neither has a second process in
//! it, and so neither touches the part of the claim that is actually load-bearing: that the two
//! halves stay coupled while both are *running*.
//!
//! # What only two processes can show
//!
//! A ring is a fixed number of bytes. The producer may not write past what the consumer has
//! consumed, and it learns what that is from a counter the consumer publishes. In process, every
//! test drains the ring between frames on the same thread, so the counter is never contended and
//! the producer never actually waits for anything.
//!
//! Here the ring is deliberately smaller than the frames going through it. The producer makes
//! progress only because another process is publishing `tail` — the two counters are in shared
//! memory, one released by each side, and if either the ordering or the sharing were wrong this
//! test would hang rather than fail an assertion. That the producer is forced to retry, and the
//! test asserts it was, is the point: a run where nothing filled up would prove nothing.
//!
//! # What it found
//!
//! The C consumer never published `tail`. In process that was invisible — the harness handed it
//! a completed buffer and threw it away — and against a live producer it is a stall on the first
//! full ring. So half of this seam had never been exercised at all, which is what a spike is for.
//!
//! # The one thing it does not close
//!
//! Geometry bytes reach a consumer through a region packed by `SlabArena::pack`, and today that
//! runs *after* the frame that named them is on the ring. In process there is no window, because
//! the arena is the same object on both sides. Across a mapping there is: a consumer can hold a
//! `GeometryAdd` whose handle the region does not yet cover. The spike sequences it explicitly —
//! the region is written and mapped before anything resolves against it — and the durable answer
//! is §11.3's arena allocating out of the shared region, where there is no pack step to be late.
//! Recorded in §3.5 rather than worked around here.

use std::os::unix::io::AsRawFd as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

use tessella_capture_abi::envelope::ViewId;
use tessella_capture_abi::ring::{self, region_size};
use tessella_orchestrate::SlabArena;
use tessella_orchestrate::frame::{self, Frame};
use tessella_orchestrate::registry::Session;
use tessella_orchestrate::tile::{LayerBucket, TileId, build_mvt_tile, build_sourceless};
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
     "paint": {"line-color": "#88a", "line-width": 1.5}}
  ]
}"##;

/// Small enough that the frames below do not fit in it, which is the whole design of the test:
/// the producer has to wait for the other process, and a ring that never filled would prove
/// nothing about the coupling.
const CAPACITY: usize = 1 << 13;

/// How long the consumer waits without progress before calling the producer dead.
const TIMEOUT_MS: u64 = 30_000;

/// A file of exactly `len` bytes, mapped shared and writable.
///
/// The mapping outlives the `File` on purpose: a mapping keeps its own reference to the object,
/// and closing the descriptor does not unmap it. Both processes map the same file, which is what
/// makes the control block's two counters one pair of counters rather than two.
struct Shared {
    base: *mut u8,
    len: usize,
}

impl Shared {
    fn create(path: &Path, len: usize) -> Self {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .expect("the backing file opens");
        file.set_len(len as u64).expect("the file sizes");
        // SAFETY: the descriptor is open and the file is exactly `len` bytes.
        let base = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        assert!(base != libc::MAP_FAILED, "the mapping fails");
        Self {
            base: base.cast::<u8>(),
            len,
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // SAFETY: this is the mapping made above, and nothing else holds it.
        unsafe { libc::munmap(self.base.cast::<libc::c_void>(), self.len) };
    }
}

/// A style, the view over it, its cover and the buckets built for it.
struct Scene {
    style: Style,
    view: ViewTransform,
    tiles: Vec<cover::TileCoord>,
    buckets: Vec<(TileId, Vec<LayerBucket>)>,
}

fn scene(longitude: f64) -> Scene {
    let style = Style::parse(STYLE).expect("the style parses");
    let view = camera::settled(&ViewTransform {
        longitude,
        latitude: 0.0,
        zoom: 3.0,
        width: 512.0,
        height: 512.0,
        bearing: 0.0,
        pitch: 0.0,
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
    Scene {
        style,
        view,
        tiles,
        buckets,
    }
}

/// Builds the consumer, returning the executable's path.
fn build_consumer(dir: &Path) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the workspace root");
    let out = dir.join("live-consumer");
    let compiler = ["cc", "gcc", "clang"]
        .into_iter()
        .find(|name| {
            Command::new(name)
                .arg("--version")
                .output()
                .is_ok_and(|done| done.status.success())
        })
        .expect("a C compiler: this test exists to run C in another process");
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

/// Reads the consumer's printed counters.
fn counters(child: Child) -> std::collections::BTreeMap<String, u64> {
    let done = child.wait_with_output().expect("the consumer exits");
    assert!(
        done.status.success(),
        "the consumer failed ({}):\n{}{}",
        done.status,
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

/// Emits against a deadline, counting how often the ring was full.
///
/// The frame rolled back whole on every refusal, so a retry is the same frame again and not the
/// tail of a half-written one.
fn press<T>(waits: &mut usize, mut attempt: impl FnMut() -> Result<T, frame::FrameError>) -> T {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        match attempt() {
            Ok(done) => return done,
            Err(error) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the ring never became writable: {error}. A full ring clears when the other \
                     process publishes `tail`; one too small to hold a frame never does, and \
                     this is how that tells itself apart from a consumer that merely stalled"
                );
                *waits += 1;
                std::thread::sleep(std::time::Duration::from_micros(200));
            }
        }
    }
}

#[test]
fn a_consumer_in_another_process_reads_a_live_stream() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let consumer = build_consumer(dir.path());
    let ring_path = dir.path().join("ring.shm");
    let shared = Shared::create(&ring_path, region_size(CAPACITY));

    // SAFETY: the mapping is `region_size(CAPACITY)` bytes and freshly zeroed, and nothing else
    // has touched it. The consumer half is unused: this process only produces.
    let (mut producer, _) = unsafe { ring::init(shared.base, CAPACITY) };

    let child = Command::new(&consumer)
        .arg("--live")
        .arg(&ring_path)
        .arg(TIMEOUT_MS.to_string())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("the consumer starts");

    let mut arena = SlabArena::new();
    let mut session = Session::new();
    let mut geometries = 0;
    let mut uses = 0;
    let mut waits = 0;

    // Frames from moving viewpoints, so each one has real work in it rather than repeating a
    // cover the registry would answer with nothing.
    //
    // Retried against a deadline rather than forever. A full ring clears when the other process
    // publishes `tail`, and a ring too small to hold the frame at all never clears — the two are
    // the same `Full` at this end, and the second one is a configuration fault that a bare
    // `loop` would turn into a hang. `a_ring_too_small_for_a_frame_never_becomes_writable` is
    // the same distinction from the other side.
    for step in 0..8 {
        let scene = scene(f64::from(step) * 12.0);
        let emitted = press(&mut waits, || {
            frame::emit_incremental(
                &mut producer,
                &mut arena,
                &Frame {
                    style: &scene.style,
                    view: &scene.view,
                    view_id: ViewId(0),
                    tiles: &scene.tiles,
                    buckets: &scene.buckets,
                    light: &Light::default(),
                    fonts: None,
                    patterns: None,
                },
                &mut session,
            )
        });
        geometries += emitted.geometries;
        uses += emitted.uses;
    }

    // The teardown is the end of the stream, and the consumer stops on it. A view that goes away
    // without saying so leaves the far side holding geometry nothing will ever mention again,
    // which is exactly what the teardown protocol is for.
    press(&mut waits, || {
        frame::teardown_view(&mut producer, &mut arena, &mut session, ViewId(0))
    });

    let counted = counters(child);
    assert!(
        waits > 0,
        "the ring never filled, so nothing here waited on the other process: \
         raise CAPACITY or lower it, but do not let this pass silently"
    );
    assert_eq!(
        counted.get("geometries").copied(),
        Some(geometries as u64),
        "every geometry the producer announced arrived: {counted:?}"
    );
    assert_eq!(
        counted.get("drawables").copied(),
        Some(uses as u64),
        "and every use — the records written, not the drawables in the frames, which retention \
         is precisely what keeps apart: {counted:?}"
    );
    assert_eq!(
        counted.get("dangling_uses").copied(),
        Some(0),
        "no use named a geometry the other process never saw declared"
    );
    assert_eq!(
        counted.get("camera_bad").copied(),
        Some(0),
        "and every camera read as a camera"
    );
}

/// A ring smaller than one frame is a fault, not backpressure.
///
/// The distinction matters because the recovery differs: a consumer that has not caught up is
/// waited for, and a ring that cannot hold a frame however empty it is will never accept one. A
/// producer that treated the second as the first would spin forever, which across a process
/// boundary looks exactly like a consumer that died.
#[test]
fn a_ring_too_small_for_a_frame_never_becomes_writable() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let ring_path = dir.path().join("tiny.shm");
    let capacity = 1 << 10;
    let shared = Shared::create(&ring_path, region_size(capacity));
    // SAFETY: as above.
    let (mut producer, consumer) = unsafe { ring::init(shared.base, capacity) };

    let scene = scene(0.0);
    let mut arena = SlabArena::new();
    let mut session = Session::new();
    for _ in 0..3 {
        assert!(
            frame::emit_incremental(
                &mut producer,
                &mut arena,
                &Frame {
                    style: &scene.style,
                    view: &scene.view,
                    view_id: ViewId(0),
                    tiles: &scene.tiles,
                    buckets: &scene.buckets,
                    light: &Light::default(),
                    fonts: None,
                    patterns: None,
                },
                &mut session,
            )
            .is_err(),
            "a frame that cannot fit is refused every time, not eventually accepted"
        );
        assert_eq!(
            consumer.occupancy(),
            0,
            "and leaves nothing behind for a consumer to trip over"
        );
    }
}
