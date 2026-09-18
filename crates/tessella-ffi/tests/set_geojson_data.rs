// SPDX-License-Identifier: BSD-2-Clause
//! Replacing a source's data through the boundary a consumer calls across.
//!
//! # Why through the FFI rather than through `TileSource`
//!
//! `tessella-orchestrate`'s own test proves the replacement reaches the tiles. What it cannot
//! prove is what a consumer actually needs: that the call is reachable from C with a source name
//! and a document as bytes, and that the map *redraws* afterwards. The redraw is not implied by
//! the rebuild -- a tick emits only when its damage gate says something changed, and the camera
//! has not moved -- so a replacement that landed new tiles and left the gate shut would be a map
//! that draws the old data for ever.

use std::ffi::c_char;

use tessella_ffi::{Config, MapHandle, Regions, Status};

/// Where the ring's head sits in the control block, which is how much has been published.
const HEAD_AT: usize = 128;

fn style(points: &str) -> String {
    format!(
        r##"{{"version": 8,
             "sources": {{"g": {{"type": "geojson",
                                 "data": {{"type": "FeatureCollection", "features": [{points}]}}}}}},
             "layers": [
               {{"id": "bg", "type": "background", "paint": {{"background-color": "#101418"}}}},
               {{"id": "dots", "type": "circle", "source": "g", "paint": {{"circle-radius": 4}}}}
             ]}}"##
    )
}

/// `count` points near the middle of the zoom-zero tile.
fn points(count: usize) -> String {
    (0..count)
        .map(|index| {
            #[allow(clippy::cast_precision_loss)]
            let offset = index as f64 * 0.5;
            format!(
                r#"{{"type":"Feature","properties":{{}},
                   "geometry":{{"type":"Point","coordinates":[{},{}]}}}}"#,
                0.1 + offset,
                51.5 - offset
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn create(style_json: &str) -> MapHandle {
    let config = Config {
        style_json: style_json.as_ptr(),
        style_json_len: style_json.len(),
        width: 512,
        height: 512,
        ring_capacity: 1 << 22,
        slab_capacity: 0,
    };
    let mut map: MapHandle = core::ptr::null_mut();
    // SAFETY: both pointers are valid and the style outlives the call.
    let status = unsafe { tessella_ffi::tessella_create(&config, 51.5, 0.1, 0.0, &mut map) };
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

/// Ticks until the style has resolved, which is when a source can be named.
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
                    core::ptr::null_mut::<c_char>(),
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

/// Hands `map` a document for source `name`.
fn set(map: MapHandle, name: &str, document: &str) -> Status {
    // SAFETY: both byte ranges are valid for the length given and outlive the call.
    unsafe {
        tessella_ffi::tessella_set_geojson_data(
            map,
            name.as_ptr(),
            name.len(),
            document.as_ptr(),
            document.len(),
        )
    }
}

/// A replacement is taken, and the map draws again because of it.
///
/// The second half is the one worth having: the camera has not moved, so a settled map emits
/// nothing, and the frames after the replacement have to be *caused* by it.
#[test]
fn a_replacement_makes_the_map_draw_again() {
    let map = create(&style(&points(3)));
    until_ready(map);

    // Settle first, so what follows is the replacement's doing and not the cold start's.
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

    let document = format!(
        r#"{{"type":"FeatureCollection","features":[{}]}}"#,
        points(9)
    );
    assert_eq!(set(map, "g", &document), Status::Ok, "the call was refused");
    std::eprintln!("TMP settled={settled}");

    let mut drew = false;
    for round in 0..2000 {
        if round < 6 {
            std::eprintln!(
                "TMP round {round} head={} pending={}",
                published(map),
                unsafe {
                    let mut p = 0u64;
                    tessella_ffi::tessella_pending(map, &mut p);
                    p
                }
            );
        }
        // SAFETY: `map` is live.
        unsafe { assert_eq!(tessella_ffi::tessella_tick(map), Status::Ok) };
        if published(map) > settled {
            drew = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(drew, "the map never drew the data it was handed");

    // SAFETY: `map` is live and is not used again.
    unsafe { tessella_ffi::tessella_destroy(map) };
}

/// What the boundary refuses, and how it says so.
#[test]
fn the_boundary_reports_what_it_cannot_take() {
    let document = format!(
        r#"{{"type":"FeatureCollection","features":[{}]}}"#,
        points(1)
    );

    // Before the style resolves there is no source list to name.
    let fresh = create(&style(&points(3)));
    assert_eq!(set(fresh, "g", &document), Status::NotResolved);

    until_ready(fresh);
    assert_eq!(
        set(fresh, "nowhere", &document),
        Status::NoSuchSource,
        "a source the style does not have"
    );
    assert_eq!(
        set(fresh, "g", "{\"type\": \"Nonsense\"}"),
        Status::BadGeojson,
        "a document that is not GeoJSON"
    );
    assert_eq!(
        set(fresh, "g", "not json at all"),
        Status::BadGeojson,
        "bytes that are not JSON"
    );

    // SAFETY: the handle is live; the null cases are what is being asked about.
    unsafe {
        assert_eq!(
            tessella_ffi::tessella_set_geojson_data(
                fresh,
                core::ptr::null(),
                0,
                document.as_ptr(),
                document.len()
            ),
            Status::NullArgument
        );
        assert_eq!(
            tessella_ffi::tessella_set_geojson_data(
                core::ptr::null_mut(),
                "g".as_ptr(),
                1,
                document.as_ptr(),
                document.len()
            ),
            Status::NoSuchMap
        );
        tessella_ffi::tessella_destroy(fresh);
    }
}
