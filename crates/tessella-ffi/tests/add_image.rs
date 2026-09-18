// SPDX-License-Identifier: BSD-2-Clause
//! An image the style did not bring, added by the consumer.
//!
//! GL JS's `map.addImage(id, image)`: eleven of the documentation examples name an icon the
//! sprite sheet does not have -- a canvas the page drew, a PNG it fetched, a pulsing dot it
//! animates -- and a build without the call can draw none of them.
//!
//! # Why this is asked through the FFI
//!
//! Because what it has to get right is not the packing, which `Sprites::insert_image` has done
//! since annotations needed it. It is that the map *redraws* with the image in hand: an icon is
//! laid out against the sheet per frame, a symbol laid out while its image was missing has no
//! rectangle, and a call that packed the image and left the map settled would draw the frame
//! without it for ever.

#![cfg(feature = "image")]

use tessella_ffi::{Config, MapHandle, Regions, Status};

/// Where the ring's head sits in the control block, which is how much has been published.
const HEAD_AT: usize = 128;

/// A real picture, because a hand-written one is a test of the decoder's patience.
const PIXEL: &[u8] = include_bytes!("../../../tools/parity/scenes/marker.png");

/// A style with no sprite of its own, which is the case `addImage` has to work in: mbgl's does,
/// and a style that names no sheet is most of the demo styles.
const STYLE: &str = r##"{
  "version": 8,
  "sources": {"points": {"type": "geojson", "data": {"type": "Feature", "properties": {},
      "geometry": {"type": "Point", "coordinates": [0, 0]}}}},
  "layers": [
    {"id": "bg", "type": "background", "paint": {"background-color": "#101418"}},
    {"id": "marks", "type": "symbol", "source": "points",
     "layout": {"icon-image": "dot", "icon-allow-overlap": true}}
  ]
}"##;

fn create() -> MapHandle {
    let config = Config {
        style_json: STYLE.as_ptr(),
        style_json_len: STYLE.len(),
        width: 256,
        height: 256,
        // Sixteen megabytes, because the sheet's atlas is 1024 by 1024 and reaches the consumer
        // whole: four megabytes of texture in the frame that hands it over, which a four-megabyte
        // ring cannot take. That is the same upload a style with its own sprite makes.
        ring_capacity: 1 << 24,
        slab_capacity: 0,
    };
    let mut map: MapHandle = core::ptr::null_mut();
    // SAFETY: both pointers are valid and the style outlives the call.
    let status = unsafe { tessella_ffi::tessella_create(&config, 0.0, 0.0, 2.0, &mut map) };
    assert_eq!(status, Status::Ok, "the map did not create");
    map
}

/// How far the ring has been written, which is what "the map emitted" means from outside.
fn published(map: MapHandle) -> u64 {
    let mut regions = Regions {
        ring: core::ptr::null(),
        ring_len: 0,
        slabs: core::ptr::null(),
        slabs_len: 0,
    };
    // SAFETY: `map` is live and `regions` is a valid out-parameter.
    unsafe {
        assert_eq!(
            tessella_ffi::tessella_regions(map, &mut regions),
            Status::Ok
        );
        let control = core::slice::from_raw_parts(regions.ring, regions.ring_len);
        u64::from_le_bytes(
            control[HEAD_AT..HEAD_AT + 8]
                .try_into()
                .expect("eight bytes"),
        )
    }
}

fn add(map: MapHandle, id: &str, image: &[u8], pixel_ratio: f64) -> Status {
    // SAFETY: both byte ranges are valid for the lengths given and outlive the call.
    unsafe {
        tessella_ffi::tessella_add_image(
            map,
            id.as_ptr(),
            id.len(),
            image.as_ptr(),
            image.len(),
            pixel_ratio,
            false,
        )
    }
}

/// Ticks until the style has resolved, which is when there is a sheet to add to.
fn until_ready(map: MapHandle) {
    for _ in 0..2000 {
        // SAFETY: `map` is live.
        unsafe {
            assert_eq!(tessella_ffi::tessella_tick(map), Status::Ok);
            let mut readiness: i32 = -1;
            assert_eq!(
                tessella_ffi::tessella_status(
                    map,
                    &mut readiness,
                    core::ptr::null_mut::<std::ffi::c_char>(),
                    0
                ),
                Status::Ok
            );
            if readiness == 2 {
                return;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    panic!("the style never resolved");
}

/// The image is taken, and the map draws again because of it.
///
/// The second half is the one worth having. The camera has not moved and no tile has landed, so
/// a settled map emits nothing; the frame after the call has to be caused by the call.
#[test]
fn an_added_image_makes_the_map_draw_again() {
    let map = create();
    until_ready(map);

    // Settle first, so what follows is the image's doing and not the cold start's.
    for _ in 0..200 {
        // SAFETY: `map` is live.
        unsafe { assert_eq!(tessella_ffi::tessella_tick(map), Status::Ok) };
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let settled = published(map);
    // SAFETY: `map` is live.
    unsafe { assert_eq!(tessella_ffi::tessella_tick(map), Status::Ok) };
    assert_eq!(
        published(map),
        settled,
        "the map was not settled, so a later frame proves nothing"
    );

    assert_eq!(
        add(map, "dot", PIXEL, 1.0),
        Status::Ok,
        "the call was refused"
    );

    let mut drew = false;
    for _ in 0..2000 {
        // SAFETY: `map` is live.
        unsafe { assert_eq!(tessella_ffi::tessella_tick(map), Status::Ok) };
        if published(map) > settled {
            drew = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(drew, "the map never drew the image it was handed");

    // SAFETY: `map` is live and is not used again.
    unsafe { tessella_ffi::tessella_destroy(map) };
}

/// A name may be replaced, which repacks the atlas rather than refusing.
#[test]
fn an_image_may_be_replaced() {
    let map = create();
    until_ready(map);
    assert_eq!(add(map, "dot", PIXEL, 1.0), Status::Ok);
    assert_eq!(
        add(map, "dot", PIXEL, 2.0),
        Status::Ok,
        "a second image under one name"
    );
    // SAFETY: `map` is live and is not used again.
    unsafe { tessella_ffi::tessella_destroy(map) };
}

/// What the boundary refuses, and how it says so.
#[test]
fn the_boundary_reports_what_it_cannot_take() {
    // Before the style resolves there is no sheet to add to.
    let fresh = create();
    assert_eq!(add(fresh, "dot", PIXEL, 1.0), Status::NotResolved);

    until_ready(fresh);
    assert_eq!(
        add(fresh, "dot", b"not a picture", 1.0),
        Status::BadImage,
        "bytes that are not a picture"
    );
    assert_eq!(
        add(fresh, "dot", PIXEL, 0.0),
        Status::BadImage,
        "a pixel ratio that is not positive"
    );

    // SAFETY: the handle is live; the null cases are what is being asked about.
    unsafe {
        assert_eq!(
            tessella_ffi::tessella_add_image(
                fresh,
                core::ptr::null(),
                0,
                PIXEL.as_ptr(),
                PIXEL.len(),
                1.0,
                false
            ),
            Status::NullArgument
        );
        assert_eq!(
            tessella_ffi::tessella_add_image(
                core::ptr::null_mut(),
                "dot".as_ptr(),
                3,
                PIXEL.as_ptr(),
                PIXEL.len(),
                1.0,
                false
            ),
            Status::NoSuchMap
        );
        tessella_ffi::tessella_destroy(fresh);
    }
}
