// SPDX-License-Identifier: BSD-2-Clause
//! A map with a store on disk draws when the origin is gone.
//!
//! `tessella_config.cache_path` is the whole of what a host has to do to get a warm start, and it
//! is also what makes a downloaded region readable -- a region's tiles are rows in this file, so a
//! map pointed at it draws them with no network at all. The point of the field is therefore the
//! case with nothing listening, which is what this drives.
//!
//! # The control is the same map without the store
//!
//! A test that only showed the cached map drawing would not say where the tiles came from: a built
//! tile outlives the map that built it (the cache is per style, held weakly), so a second map on
//! the same style can draw without fetching anything. Two things keep that from being the answer
//! here. Each phase uses a style with a different `name`, which is a different key in that cache
//! and so a cold start for it; and the third phase is the second with no `cache_path` at all,
//! which has to draw nothing.

#![cfg(feature = "cache")]

use tessella_ffi::{Config, MapHandle, Regions, Status};

const TILE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/protomaps-berlin-14-8802-5373.mvt");

/// What an arena with nothing in it reports: its header and its empty table.
const EMPTY: usize = 16 + 4096 * 16;

/// One fill layer over a vector source, told apart from the other phases' by `name`.
fn style(name: &str, origin: &str) -> String {
    format!(
        r##"{{
  "version": 8,
  "name": "{name}",
  "sources": {{
    "fixture": {{"type": "vector", "tiles": ["{origin}/{{z}}/{{x}}/{{y}}.pbf"], "maxzoom": 14}}
  }},
  "layers": [
    {{"id": "earth", "type": "fill", "source": "fixture", "source-layer": "earth",
      "paint": {{"fill-color": "#204060"}}}}
  ]
}}"##
    )
}

/// Runs one map to its deadline and answers how many bytes its arena packed.
///
/// The same measurement `labels.rs` takes, and for the same reason: the region's header carries the
/// cursor, where `slabs_len` is the capacity it was given and says nothing about what was written.
fn drawn(style: &str, cache_path: Option<&std::path::Path>, seconds: u64) -> usize {
    let path = cache_path.map(|path| path.to_string_lossy().into_owned());
    let config = Config {
        style_json: style.as_ptr(),
        style_json_len: style.len(),
        width: 512,
        height: 512,
        ring_capacity: 1 << 22,
        slab_capacity: 0,
        cache_path: path
            .as_ref()
            .map_or(core::ptr::null(), |path| path.as_ptr()),
        cache_path_len: path.as_ref().map_or(0, std::string::String::len),
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

/// A store outlives the origin it was filled from, and nothing else accounts for the picture.
#[test]
fn a_store_outlives_the_origin() {
    let dir = std::env::temp_dir().join(format!("tessella-offline-cache-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let path = dir.join("cache.sqlite");

    // With a server, which is the run that pays for the tiles.
    let origin;
    let warm = {
        let server =
            tile_server::Server::start(tile_server::Routes::new().tiles(TILE.to_vec(), None))
                .expect("the server starts");
        origin = server.origin();
        let drew = drawn(&style("warm", &origin), Some(&path), 20);
        assert!(
            server.requests() > 0,
            "the first map drew without asking the origin for anything"
        );
        drew
    };
    assert!(
        warm > EMPTY + 1024,
        "the first map drew nothing: {} bytes past an empty arena",
        warm.saturating_sub(EMPTY)
    );
    assert!(path.exists(), "the store was not written");

    // The server is gone. Same tile urls, a style this process has never built, and the store.
    let offline = drawn(&style("offline", &origin), Some(&path), 20);

    // And the control: the same again with no store, which is the one thing that differs.
    let blind = drawn(&style("control", &origin), None, 10);

    std::fs::remove_dir_all(&dir).ok();

    assert!(
        offline > EMPTY + 1024,
        "a map with a store drew nothing with the origin gone: {} bytes past an empty arena",
        offline.saturating_sub(EMPTY)
    );
    assert!(
        blind <= EMPTY,
        "a map with no store drew {} bytes past an empty arena with nothing listening, so the \
         store is not what the phase above was reading",
        blind.saturating_sub(EMPTY)
    );
}
