//! A map created through the C API draws its labels.
//!
//! # Why this needs a server
//!
//! `boot` does not fetch glyphs and cannot: which glyphs a style needs is not a property of the
//! style but of the *data* — `text-field` evaluated against each tile's own features — so nothing
//! can be asked for until the tiles are built, which is the last thing boot does. `tessella_create`
//! therefore collects the dependencies from the built cover and fetches them itself.
//!
//! It fetches over the same coalescing store the tiles came through, and that store is HTTP: a
//! `file://` glyph URL, which every fixture in this repository uses, does not resolve. So the
//! test serves the range over HTTP, which is also the arrangement a real style has.
//!
//! # What it would catch
//!
//! A map that draws everything except its labels, silently. `Content::is_encodable` withholds a
//! symbol bucket whose glyphs have not arrived — deliberately, so it stays fresh for the frame
//! that can draw it — and the consequence is that forgetting to fetch them produces a map that
//! is correct in every other respect and has no text on it. Which is what the C API did until
//! now.

#![cfg(feature = "image")]

use std::ffi::CString;

use tessella_ffi::{Config, MapHandle, Regions, Status};

const GLYPHS: &[u8] = include_bytes!("../../../tests/glyph-fixtures/TestFont/0-255.pbf");

/// The symbol style, pointed at a local server for its glyphs.
fn style_at(origin: &str) -> String {
    let raw = include_str!("../../tessella-style/tests/symbol_style.json");
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    raw.replace(
        "file://TESSELLA/tests/glyph-fixtures/{fontstack}/{range}.pbf",
        &format!("{origin}/glyphs/{{fontstack}}/{{range}}.pbf"),
    )
    .replace("TESSELLA", root)
}

/// Labels reach the wire through the C API.
#[test]
fn a_map_created_through_c_draws_its_labels() {
    let server = tile_server::Server::start(tile_server::Routes::new().at(
        "/glyphs/TestFont/0-255.pbf",
        "application/x-protobuf",
        GLYPHS.to_vec(),
    ))
    .expect("the server starts");

    let style = CString::new(style_at(&server.origin())).expect("no interior NUL");
    let config = Config {
        style_json: style.as_ptr(),
        width: 1024,
        height: 768,
        ring_capacity: 1 << 22,
    };
    let mut map: MapHandle = core::ptr::null_mut();
    // SAFETY: both pointers are valid and the style outlives the call.
    let status = unsafe { tessella_ffi::tessella_create(&config, 51.505, -0.11, 13.0, &mut map) };
    assert_eq!(status, Status::Ok, "the map did not create");

    // SAFETY: `map` is live.
    let drawn = unsafe {
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
        // The symbol drawable's vertices live in a slab, so a frame with labels packs far more
        // than one without them.
        let packed = regions.slabs_len;
        tessella_ffi::tessella_destroy(map);
        packed
    };

    // Measured both ways rather than guessed: 3168 bytes with the glyphs fetched and 224
    // without. The first threshold tried here was 64, which both states clear — the test passed
    // with the fetch disabled, which is the only reason its emptiness was noticed.
    assert!(
        drawn > 1024,
        "the frame packed {drawn} bytes of geometry, against 3168 when the labels reach it and \
         224 when they do not — the glyphs are not getting in"
    );
}
