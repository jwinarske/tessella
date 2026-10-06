// SPDX-License-Identifier: BSD-2-Clause
//! A region downloaded through the C surface, and a map drawing it with the origin gone.
//!
//! Region download was complete in Rust and unreachable from `tessella.h` (tessella#336). What that
//! buys, and what this drives end to end: define a region, be told what it costs, fill it, watch it
//! fill, then draw it on a device with no network -- which is the only reason any of the rest of it
//! exists.
//!
//! The last phase is the one that could not be faked. A map is created on the same store with the
//! server **dropped**, and it draws; the control is the same map on a store with no region in it,
//! which draws nothing.

#![cfg(feature = "cache")]

use tessella_ffi::offline::{Cost, Counters, OfflineHandle, RegionSpec, State};
use tessella_ffi::{Config, MapHandle, Regions, Status};

const TILE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/protomaps-berlin-14-8802-5373.mvt");

/// What an arena with nothing in it reports: its header and its empty table.
const EMPTY: usize = 16 + 4096 * 16;

/// One fill layer over a vector source, told apart from another phase's by `name`.
fn style(name: &str, origin: &str) -> String {
    format!(
        r##"{{
  "version": 8,
  "name": "{name}",
  "sources": {{
    "fixture": {{"type": "vector", "tiles": ["{origin}/{{z}}/{{x}}/{{y}}.pbf"],
                 "minzoom": 12, "maxzoom": 14}}
  }},
  "layers": [
    {{"id": "earth", "type": "fill", "source": "fixture", "source-layer": "earth",
      "paint": {{"fill-color": "#204060"}}}}
  ]
}}"##
    )
}

/// A spec over central Berlin at the zooms the map below draws.
fn spec(style_url: &str, min_zoom: f64, max_zoom: f64) -> RegionSpec {
    RegionSpec {
        style_url: style_url.as_ptr(),
        style_url_len: style_url.len(),
        geojson: core::ptr::null(),
        geojson_len: 0,
        description: core::ptr::null(),
        description_len: 0,
        west: 13.3,
        south: 52.45,
        east: 13.5,
        north: 52.58,
        min_zoom,
        max_zoom,
        pixel_ratio: 1.0,
        include_ideographs: 0,
    }
}

fn progress(offline: OfflineHandle, region: u64) -> Counters {
    let mut counters = Counters::default();
    // SAFETY: a live handle and a writable out parameter.
    let status =
        unsafe { tessella_ffi::offline::tessella_offline_progress(offline, region, &mut counters) };
    assert_eq!(status, Status::Ok, "progress refused");
    counters
}

/// Runs one map to its deadline and answers how many bytes its arena packed.
fn drawn(style: &str, cache_path: &std::path::Path, seconds: u64) -> usize {
    let path = cache_path.to_string_lossy().into_owned();
    let config = Config {
        style_json: style.as_ptr(),
        style_json_len: style.len(),
        width: 512,
        height: 512,
        ring_capacity: 1 << 22,
        slab_capacity: 0,
        cache_path: path.as_ptr(),
        cache_path_len: path.len(),
    };
    let mut map: MapHandle = core::ptr::null_mut();
    // SAFETY: both pointers are valid and the style and the path outlive the call.
    let status = unsafe { tessella_ffi::tessella_create(&config, 52.52, 13.405, 13.0, &mut map) };
    assert_eq!(status, Status::Ok, "the map did not create");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    // SAFETY: `map` is live until it is destroyed below.
    unsafe {
        let mut packed = 0;
        while std::time::Instant::now() < deadline {
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
            if regions.slabs_len >= 16 {
                let header = core::slice::from_raw_parts(regions.slabs, 16);
                let total =
                    u64::from_le_bytes(header[8..16].try_into().expect("eight bytes")) as usize;
                packed = packed.max(total);
            }
            if packed > EMPTY + 1024 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        tessella_ffi::tessella_destroy(map);
        packed
    }
}

/// The whole lifecycle, and then the map that is the point of it.
#[test]
fn a_region_is_defined_sized_filled_and_drawn() {
    use tessella_ffi::offline::{
        tessella_offline_cancel, tessella_offline_close, tessella_offline_define,
        tessella_offline_download, tessella_offline_estimate, tessella_offline_list,
        tessella_offline_open,
    };

    let dir = std::env::temp_dir().join(format!("tessella-offline-regions-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let store = dir.join("regions.sqlite");
    let empty_store = dir.join("empty.sqlite");

    let origin;
    let style_text;
    {
        // The style is served as well as passed: a region fetches its own style, which is the
        // first resource of every download.
        let server = tile_server::Server::start(
            tile_server::Routes::new().tiles(TILE.to_vec(), Some((12, 14))),
        )
        .expect("the server starts");
        origin = server.origin();
        style_text = style("region", &origin);
        let style_url = format!("{origin}/style.json");
        server.set_routes(
            tile_server::Routes::new()
                .tiles(TILE.to_vec(), Some((12, 14)))
                .at("/style.json", "application/json", style_text.clone().into()),
        );

        let path = store.to_string_lossy().into_owned();
        let mut offline: OfflineHandle = core::ptr::null_mut();
        // SAFETY: a valid path and a writable out parameter.
        let status = unsafe { tessella_offline_open(path.as_ptr(), path.len(), &mut offline) };
        assert_eq!(status, Status::Ok, "the store did not open");
        assert!(!offline.is_null(), "open returned OK without a handle");

        // Sized before it is agreed to, which is the whole shape of the feature: a user picks a
        // box, is shown what it costs, and accepts or declines.
        let spec = spec(&style_url, 13.0, 14.0);
        let mut cost = Cost::default();
        // SAFETY: a live handle, a readable spec and style, and a writable out parameter.
        let status = unsafe {
            tessella_offline_estimate(
                offline,
                &spec,
                style_text.as_ptr(),
                style_text.len(),
                &mut cost,
            )
        };
        assert_eq!(status, Status::Ok, "the estimate refused");
        assert!(cost.tiles > 0, "a region over a city has tiles");
        assert!(
            cost.resources > cost.tiles,
            "the style itself is a resource too: {cost:?}"
        );
        assert_eq!(
            cost.precise, 1,
            "an inline source states its zooms, so nothing here is a lower bound"
        );

        // Defined. It exists at nothing percent, which is what makes a download resumable.
        let mut region = 0u64;
        // SAFETY: as above.
        let status = unsafe { tessella_offline_define(offline, &spec, &mut region) };
        assert_eq!(status, Status::Ok, "the region was not recorded");

        let mut ids = [0u64; 4];
        let mut count = 0usize;
        // SAFETY: `ids` is valid for four and `count` is writable.
        let status =
            unsafe { tessella_offline_list(offline, ids.as_mut_ptr(), ids.len(), &mut count) };
        assert_eq!(status, Status::Ok);
        assert_eq!(count, 1, "one region, listed");
        assert_eq!(ids[0], region);

        let before = progress(offline, region);
        assert_eq!(before.state, State::Idle as u32);
        assert_eq!(before.stored_resources, 0, "nothing is claimed yet");

        // An identifier the store does not have is answered rather than guessed.
        let mut nothing = Counters::default();
        // SAFETY: a live handle and a writable out parameter.
        let status = unsafe {
            tessella_ffi::offline::tessella_offline_progress(offline, region + 999, &mut nothing)
        };
        assert_eq!(status, Status::NoSuchRegion);

        // Filled.
        // SAFETY: a live handle and a readable style.
        let status = unsafe {
            tessella_offline_download(offline, region, style_text.as_ptr(), style_text.len())
        };
        assert_eq!(status, Status::Ok, "the download did not start");

        // A second start is refused while the first runs. Immediately, so the first cannot have
        // finished: it has an HTTP round trip to make before it has even planned.
        // SAFETY: as above.
        let again = unsafe {
            tessella_offline_download(offline, region, style_text.as_ptr(), style_text.len())
        };
        assert_eq!(again, Status::AlreadyRunning, "two threads for one region");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut last = progress(offline, region);
        while std::time::Instant::now() < deadline && last.state == State::Running as u32 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            last = progress(offline, region);
        }
        assert_eq!(
            last.state,
            State::Done as u32,
            "the download did not finish: {last:?}"
        );
        assert!(last.required > 0, "the plan named nothing: {last:?}");
        assert_eq!(
            last.completed, last.required,
            "it finished without dealing with everything: {last:?}"
        );
        assert!(
            last.stored_resources > 0,
            "nothing was claimed against the region: {last:?}"
        );
        assert!(last.stored_bytes > 0, "the claims have no bytes: {last:?}");

        // Cancelling a finished download is not an error, and neither is cancelling twice.
        // SAFETY: a live handle.
        assert_eq!(
            unsafe { tessella_offline_cancel(offline, region) },
            Status::Ok
        );

        // An empty store for the control below, opened and closed with no region in it.
        let other = empty_store.to_string_lossy().into_owned();
        let mut second: OfflineHandle = core::ptr::null_mut();
        // SAFETY: a valid path and a writable out parameter.
        assert_eq!(
            unsafe { tessella_offline_open(other.as_ptr(), other.len(), &mut second) },
            Status::Ok
        );
        // SAFETY: a handle from open, not used again.
        unsafe { tessella_offline_close(second) };

        // SAFETY: a handle from open, not used again.
        unsafe { tessella_offline_close(offline) };
    }

    // The server is gone. A map on the store draws the region; a map on the empty store does not.
    let region_drew = drawn(&style("drawn", &origin), &store, 20);
    let control = drawn(&style("control", &origin), &empty_store, 10);
    std::fs::remove_dir_all(&dir).ok();

    assert!(
        region_drew > EMPTY + 1024,
        "a map on a downloaded region drew nothing with the origin gone: {} bytes past empty",
        region_drew.saturating_sub(EMPTY)
    );
    assert!(
        control <= EMPTY,
        "a map on a store with no region drew {} bytes past empty with nothing listening, so the \
         region is not what the map above was reading",
        control.saturating_sub(EMPTY)
    );
}

/// A deleted region is gone from the list, and every call naming it says so.
#[test]
fn a_deleted_region_is_not_a_region_with_nothing_in_it() {
    use tessella_ffi::offline::{
        tessella_offline_cancel, tessella_offline_close, tessella_offline_define,
        tessella_offline_delete, tessella_offline_list, tessella_offline_open,
    };

    let dir = std::env::temp_dir().join(format!("tessella-offline-delete-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let path = dir.join("regions.sqlite").to_string_lossy().into_owned();

    let mut offline: OfflineHandle = core::ptr::null_mut();
    // SAFETY: a valid path and a writable out parameter.
    assert_eq!(
        unsafe { tessella_offline_open(path.as_ptr(), path.len(), &mut offline) },
        Status::Ok
    );

    let style_url = "https://host.invalid/style.json";
    let spec = spec(style_url, 10.0, 11.0);
    let mut region = 0u64;
    // SAFETY: a live handle, a readable spec and a writable out parameter.
    assert_eq!(
        unsafe { tessella_offline_define(offline, &spec, &mut region) },
        Status::Ok
    );

    // SAFETY: a live handle.
    assert_eq!(
        unsafe { tessella_offline_delete(offline, region) },
        Status::Ok
    );

    let mut count = 7usize;
    // SAFETY: a null buffer with a capacity of zero, which is how the count alone is asked for.
    assert_eq!(
        unsafe { tessella_offline_list(offline, core::ptr::null_mut(), 0, &mut count) },
        Status::Ok
    );
    assert_eq!(count, 0, "the region is still listed");

    // Every call that names it answers the same thing, rather than one of them inventing an
    // empty region.
    // SAFETY: a live handle.
    assert_eq!(
        unsafe { tessella_offline_delete(offline, region) },
        Status::NoSuchRegion
    );
    // SAFETY: a live handle.
    assert_eq!(
        unsafe { tessella_offline_cancel(offline, region) },
        Status::NoSuchRegion
    );
    assert_eq!(
        unsafe {
            let mut counters = Counters::default();
            tessella_ffi::offline::tessella_offline_progress(offline, region, &mut counters)
        },
        Status::NoSuchRegion
    );

    // SAFETY: a handle from open, not used again.
    unsafe { tessella_offline_close(offline) };
    std::fs::remove_dir_all(&dir).ok();
}
