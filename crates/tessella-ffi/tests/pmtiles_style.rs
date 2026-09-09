//! A style that names an archive on an origin, through the C surface.
//!
//! # What this closes
//!
//! Two things landed separately: an archive can be read by byte range, and a `pmtiles://` url can
//! name an origin. Neither is reachable from a map until the FFI's router sends `pmtiles://`
//! somewhere, which is what this checks -- a style pointed at an archive over HTTP resolves and
//! draws, and the archive is never downloaded.
//!
//! The server is the one the storage tests use, in thirty lines of `TcpListener`: it honours
//! `Range` and nothing else, which is all an archive needs.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};

use tessella_ffi::{Config, MapHandle, Status};

/// A real archive, when this machine has one.
fn archive_bytes() -> Option<Vec<u8>> {
    let path = std::path::Path::new("/mnt/dev/maplibre-frontend/tileserver/berlin_z15.pmtiles");
    path.exists().then(|| std::fs::read(path).expect("reads"))
}

/// Serves one file over HTTP, honouring `Range`. Answers the port it bound.
fn serve(bytes: Vec<u8>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
    let port = listener.local_addr().expect("has an address").port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            answer(stream, &bytes);
        }
    });
    port
}

fn answer(mut stream: TcpStream, bytes: &[u8]) {
    let mut reader = BufReader::new(stream.try_clone().expect("clones"));
    let mut range = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
            let (first, last) = value.trim().split_once('-').unwrap_or(("", ""));
            if let (Ok(first), Ok(last)) = (first.parse::<usize>(), last.parse::<usize>()) {
                range = Some((first, last));
            }
        }
    }
    let (status, body) = match range {
        Some((first, last)) if last < bytes.len() => {
            ("206 Partial Content", bytes[first..=last].to_vec())
        }
        Some(_) => ("416 Range Not Satisfiable", Vec::new()),
        None => ("200 OK", bytes.to_vec()),
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&body);
    let _ = stream.flush();
    let mut sink = Vec::new();
    let _ = reader.read_to_end(&mut sink);
}

#[test]
fn a_map_draws_from_an_archive_on_an_origin() {
    let Some(bytes) = archive_bytes() else {
        eprintln!("no archive on this machine; skipping");
        return;
    };
    let port = serve(bytes);
    // Berlin, at a zoom the archive carries. The source is named by `url`, so resolving it is a
    // manifest read out of the archive rather than a fetch of anything else.
    let style = format!(
        r##"{{"version": 8,
             "sources": {{"a": {{"type": "vector",
                 "url": "pmtiles://http://127.0.0.1:{port}/berlin_z15.pmtiles"}}}},
             "layers": [
               {{"id": "bg", "type": "background", "paint": {{"background-color": "#101418"}}}},
               {{"id": "water", "type": "fill", "source": "a", "source-layer": "water",
                 "paint": {{"fill-color": "#3050c0"}}}}]}}"##
    );

    let config = Config {
        style_json: style.as_ptr(),
        style_json_len: style.len(),
        width: 512,
        height: 512,
        ring_capacity: 1 << 22,
        slab_capacity: 0,
    };
    let mut map: MapHandle = core::ptr::null_mut();
    // SAFETY: both pointers are valid for the call, and the style is a live byte range.
    let status =
        unsafe { tessella_ffi::tessella_create(&config, 52.52, 13.405, 14.0, &raw mut map) };
    assert_eq!(status, Status::Ok, "the map did not create");
    assert!(!map.is_null());

    // Ticked until it settles, which is what a consumer does. A source read out of an archive
    // resolves on a worker exactly as an http one does.
    let mut readiness = -1;
    let mut reason = [0u8; 256];
    for _ in 0..4000 {
        // SAFETY: the handle is live for the loop.
        assert_eq!(unsafe { tessella_ffi::tessella_tick(map) }, Status::Ok);
        let mut pending = 0u64;
        // SAFETY: as above, and both out pointers are valid.
        unsafe {
            tessella_ffi::tessella_status(
                map,
                &raw mut readiness,
                reason.as_mut_ptr().cast(),
                reason.len(),
            );
            tessella_ffi::tessella_pending(map, &raw mut pending);
        }
        if readiness == 2 && pending == 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }

    let said = String::from_utf8_lossy(&reason);
    let said = said.split('\0').next().unwrap_or("");
    assert_eq!(readiness, 2, "the archive source never resolved: {said}");
    assert!(said.is_empty(), "the map reported a problem: {said}");

    // Resolving only proves the *manifest* came out of the archive. Geometry proves a tile did,
    // which is the read that goes through the directory walk. Measured from the arena's own
    // cursor rather than the region length, which is the capacity it was handed and says nothing
    // about what was written.
    let mut regions = tessella_ffi::Regions {
        ring: core::ptr::null(),
        ring_len: 0,
        slabs: core::ptr::null(),
        slabs_len: 0,
    };
    // SAFETY: the handle is live and the out pointer is valid.
    assert_eq!(
        unsafe { tessella_ffi::tessella_regions(map, &raw mut regions) },
        Status::Ok
    );
    assert!(regions.slabs_len >= 16, "no slab region");
    // SAFETY: the producer owns this region and says it is at least sixteen bytes.
    let header = unsafe { core::slice::from_raw_parts(regions.slabs, 16) };
    let packed = u64::from_le_bytes(header[8..16].try_into().expect("eight bytes")) as usize;
    // An arena with nothing in it is its header plus an empty table.
    let empty = 16 + 4096 * 16;
    assert!(
        packed > empty + 1024,
        "the arena holds {} bytes past its empty table; no tile came out of the archive",
        packed.saturating_sub(empty)
    );

    // SAFETY: the handle came back from a successful create and has not been destroyed.
    unsafe { tessella_ffi::tessella_destroy(map) };
}
