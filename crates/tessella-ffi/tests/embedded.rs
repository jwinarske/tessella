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

use std::ffi::CString;

use tessella_ffi::{Config, MapHandle, Regions, Status};

const STYLE: &str = r##"{
  "version": 8,
  "sources": {},
  "layers": [
    {"id": "bg", "type": "background", "paint": {"background-color": "#101418"}}
  ]
}"##;

fn create() -> MapHandle {
    let style = CString::new(STYLE).expect("no interior NUL");
    let config = Config {
        style_json: style.as_ptr(),
        width: 1024,
        height: 768,
        ring_capacity: 1 << 22,
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
        assert_eq!(
            total as usize, regions.slabs_len,
            "the region's declared length disagrees with the range handed over"
        );
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

    let style = CString::new(STYLE).expect("no interior NUL");
    let config = Config {
        style_json: style.as_ptr(),
        width: 1024,
        height: 768,
        ring_capacity: 1 << 22,
    };
    // SAFETY: the config is valid; the out pointer deliberately is not.
    let status =
        unsafe { tessella_ffi::tessella_create(&config, 0.0, 0.0, 1.0, core::ptr::null_mut()) };
    assert_eq!(status, Status::NullArgument);
}
