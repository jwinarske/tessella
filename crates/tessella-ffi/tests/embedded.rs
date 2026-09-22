//! A map created, aimed, ticked and destroyed the way a consumer embeds one.
//!
//! # Why through the FFI rather than through `Map`
//!
//! Because the FFI is where the assumptions live that `Map`'s own tests cannot see: that a
//! handle survives being handed to C and back, that a tick with nothing to do is cheap and says
//! so, and — the one that matters — that the ranges `tessella_regions` reports actually resolve.
//! A producer whose slab table is packed at the wrong moment produces a frame that is perfectly
//! well-formed and whose every handle dangles, and nothing on the Rust side of the boundary
//! would notice.
//!
//! The style is inline and its source has no tiles, so nothing is fetched: this is about the
//! lifecycle, not about drawing a map.

use tessella_ffi::{Config, MapHandle, Regions, Status};

const STYLE: &str = r##"{
  "version": 8,
  "sources": {},
  "layers": [
    {"id": "bg", "type": "background", "paint": {"background-color": "#101418"}}
  ]
}"##;

fn create() -> MapHandle {
    // A byte range, not a C string: the ABI takes a pointer and a length, so a
    // terminator is one thing fewer to get right.
    let style = String::from(STYLE);
    let config = Config {
        style_json: style.as_ptr(),
        style_json_len: style.len(),
        width: 1024,
        height: 768,
        ring_capacity: 1 << 22,
        // The default, which is ample for a test cover.
        slab_capacity: 0,
    };
    let mut map: MapHandle = core::ptr::null_mut();
    // SAFETY: both pointers are valid and the style outlives the call.
    let status = unsafe { tessella_ffi::tessella_create(&config, 51.505, -0.11, 4.0, &mut map) };
    assert_eq!(status, Status::Ok, "the map did not create");
    assert!(!map.is_null(), "a successful create handed back null");
    map
}

/// The lifecycle: create, aim, tick, destroy.
#[test]
fn a_map_ticks_and_settles() {
    let map = create();

    // SAFETY: `map` is live for all of these.
    unsafe {
        assert_eq!(tessella_ffi::tessella_tick(map), Status::Ok);
        // A settled map. The tick still returns `Ok` — sending nothing is the ordinary case, not
        // a condition — and what distinguishes it is the absence of records rather than a status.
        assert_eq!(tessella_ffi::tessella_tick(map), Status::Ok);

        assert_eq!(
            tessella_ffi::tessella_set_camera(map, 51.51, -0.12, 5.0, 0.0, 0.0),
            Status::Ok
        );
        assert_eq!(tessella_ffi::tessella_tick(map), Status::Ok);

        tessella_ffi::tessella_destroy(map);
    }
}

/// The regions resolve: the ring carries records, and the slab table is complete.
///
/// This is the assertion the Rust side cannot make for itself. `tessella_regions` hands back two
/// raw ranges, and a slab table packed before the frame finished allocating against it produces
/// handles that index past its end — a frame that looks right from the producer and resolves to
/// nothing from the consumer.
#[test]
fn the_regions_a_consumer_reads_are_whole() {
    let map = create();

    // SAFETY: `map` is live.
    unsafe {
        assert_eq!(tessella_ffi::tessella_tick(map), Status::Ok);

        let mut regions = Regions {
            ring: core::ptr::null(),
            ring_len: 0,
            slabs: core::ptr::null(),
            slabs_len: 0,
        };
        assert_eq!(
            tessella_ffi::tessella_regions(map, &mut regions),
            Status::Ok
        );
        assert!(!regions.ring.is_null(), "no ring");
        assert!(regions.ring_len > 0, "an empty ring");

        // The ring's head, read where the header says it is. A frame that emitted nothing would
        // leave it at zero, and the first tick of a fresh map always emits.
        let control = core::slice::from_raw_parts(regions.ring, regions.ring_len);
        let head = u64::from_le_bytes(control[128..136].try_into().expect("eight bytes"));
        assert!(
            head > 0,
            "the first tick published nothing: head is still zero"
        );

        // The slab region describes itself: revision, count, and a total that covers what it
        // claims to hold. A table packed too early fails the last of these.
        assert!(regions.slabs_len >= 16, "no slab region header");
        let slabs = core::slice::from_raw_parts(regions.slabs, regions.slabs_len);
        let abi_rev = u32::from_le_bytes(slabs[0..4].try_into().expect("four bytes"));
        let count = u32::from_le_bytes(slabs[4..8].try_into().expect("four bytes"));
        let total = u64::from_le_bytes(slabs[8..16].try_into().expect("eight bytes"));
        assert_eq!(
            abi_rev,
            tessella_capture_abi::ABI_REV,
            "the slab region was packed at a different ABI revision"
        );
        // At most, not equal: the range handed over is the whole region the arena was built on,
        // and `total_len` is how far the bump cursor has reached inside it. They were equal when
        // the producer serialized a fresh buffer each frame; it writes in place now, so the
        // header is what bounds a consumer's reads and the capacity is what bounds the header.
        assert!(
            total as usize <= regions.slabs_len,
            "the region claims {total} bytes of a {} byte range",
            regions.slabs_len
        );
        assert!(total >= 16, "the region does not cover its own header");
        assert!(
            16 + (count as usize) * 16 <= regions.slabs_len,
            "the slab table of {count} entries does not fit in {} bytes",
            regions.slabs_len
        );

        tessella_ffi::tessella_destroy(map);
    }
}

/// A null handle is refused rather than dereferenced, and destroying null is harmless.
#[test]
fn the_boundary_refuses_what_it_cannot_use() {
    // SAFETY: a null handle is exactly what these are being asked about.
    unsafe {
        assert_eq!(
            tessella_ffi::tessella_tick(core::ptr::null_mut()),
            Status::NoSuchMap
        );
        assert_eq!(
            tessella_ffi::tessella_set_camera(core::ptr::null_mut(), 0.0, 0.0, 1.0, 0.0, 0.0),
            Status::NoSuchMap
        );
        // A caller that frees twice, or frees a handle it never got, must not take the process
        // with it.
        tessella_ffi::tessella_destroy(core::ptr::null_mut());
    }

    // A byte range, not a C string: the ABI takes a pointer and a length, so a
    // terminator is one thing fewer to get right.
    let style = String::from(STYLE);
    let config = Config {
        style_json: style.as_ptr(),
        style_json_len: style.len(),
        width: 1024,
        height: 768,
        ring_capacity: 1 << 22,
        // The default, which is ample for a test cover.
        slab_capacity: 0,
    };
    // SAFETY: the config is valid; the out pointer deliberately is not.
    let status =
        unsafe { tessella_ffi::tessella_create(&config, 0.0, 0.0, 1.0, core::ptr::null_mut()) };
    assert_eq!(status, Status::NullArgument);
}

/// A published camera reaches the strip whole, matrix and map camera together.
///
/// The two halves answer different questions and are written in one seqlock generation, so this
/// asserts both came back rather than just the scalars: sixteen doubles is most of the payload,
/// and a read that returned the map camera alone would look like a pass.
#[test]
fn a_published_camera_reaches_the_strip() {
    let map = create();
    let matrix: [f64; 16] = core::array::from_fn(|i| i as f64 + 0.5);

    // SAFETY: `map` is live, and `matrix` is sixteen readable doubles.
    unsafe {
        assert_eq!(
            tessella_ffi::tessella_publish_camera(
                map,
                matrix.as_ptr(),
                -0.11,
                51.505,
                4.0,
                30.0,
                45.0,
            ),
            Status::Ok
        );

        let camera = tessella_ffi::published_camera_for_test(map).expect("nothing was published");
        assert_eq!(camera.view_projection, matrix, "the matrix did not survive");
        assert_eq!(camera.zoom, 4.0);
        assert_eq!(camera.bearing, 30.0);
        assert_eq!(camera.pitch, 45.0);
        assert_eq!(
            camera.center_zoom0,
            tessella_tile::projection::center_zoom0(-0.11, 51.505),
            "the center is the scale-free one the strip is defined in"
        );

        tessella_ffi::tessella_destroy(map);
    }
}

/// Publishing through a null handle is refused rather than crashing, and a null matrix with it.
#[test]
fn publishing_without_a_map_or_a_matrix_is_refused() {
    let matrix = [0.0f64; 16];
    let map = create();

    // SAFETY: the first call is the null-handle path; the second has a live map.
    unsafe {
        assert_eq!(
            tessella_ffi::tessella_publish_camera(
                core::ptr::null_mut(),
                matrix.as_ptr(),
                0.0,
                0.0,
                1.0,
                0.0,
                0.0,
            ),
            Status::NoSuchMap
        );
        assert_eq!(
            tessella_ffi::tessella_publish_camera(map, core::ptr::null(), 0.0, 0.0, 1.0, 0.0, 0.0),
            Status::NullArgument
        );
        tessella_ffi::tessella_destroy(map);
    }
}

/// A consumer-camera map takes its camera from the strip, and a producer-camera one ignores it.
///
/// The assertion is that the map *moved*, not that the call returned Ok: publishing into a strip
/// nothing reads returns Ok all day. The cover is what moves, so the tiles the map wants are the
/// evidence -- London and Berlin do not want the same ones.
#[test]
fn a_consumer_camera_map_follows_what_was_published() {
    let map = create();
    let matrix = [0.0f64; 16];

    // SAFETY: `map` is live for all of these, and `matrix` is sixteen readable doubles.
    unsafe {
        // Created over London, and still there after a tick.
        assert_eq!(tessella_ffi::tessella_tick(map), Status::Ok);
        let london = tessella_ffi::wanted_tiles_for_test(map);

        // Berlin, published but not yet asked for: a producer-camera map reads nothing.
        assert_eq!(
            tessella_ffi::tessella_publish_camera(
                map,
                matrix.as_ptr(),
                13.405,
                52.52,
                4.0,
                0.0,
                0.0
            ),
            Status::Ok
        );
        assert_eq!(tessella_ffi::tessella_tick(map), Status::Ok);
        assert_eq!(
            tessella_ffi::wanted_tiles_for_test(map),
            london,
            "a producer-camera map moved for a camera it does not own"
        );

        // Asked for, and now it follows.
        assert_eq!(
            tessella_ffi::tessella_set_camera_owner(map, tessella_ffi::CameraOwner::Consumer),
            Status::Ok
        );
        assert_eq!(tessella_ffi::tessella_tick(map), Status::Ok);
        assert_ne!(
            tessella_ffi::wanted_tiles_for_test(map),
            london,
            "a consumer-camera map stayed where the producer left it"
        );

        tessella_ffi::tessella_destroy(map);
    }
}
