//! The transport, against a real server over a real socket.
//!
//! Everything above the transport is tested against an in-memory source in `coalescing.rs`.
//! This is the part that only a socket can check: that a templated URL is one a server actually
//! answers, that a body arrives whole, that a 404 becomes a response rather than an error, and
//! that coalescing still holds when the thing being coalesced is a syscall rather than a
//! function call.
//!
//! The server binds an ephemeral port on loopback, so this needs no network and several of
//! these can run at once.

#![cfg(feature = "http")]

use std::sync::{Arc, Barrier};

use tessella_storage::http::HttpFileSource;
use tessella_storage::source::{Coalescing, FetchError, FileSource};
use tessella_storage::url::{Scheme, expand};

const FIXTURE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/real-world-0-0-0.mvt");

fn server() -> tile_server::Server {
    tile_server::Server::start(tile_server::Routes::new().tiles(FIXTURE.to_vec(), Some((0, 14))))
        .expect("binds a loopback port")
}

/// A templated URL fetches the bytes the server has.
#[test]
fn a_templated_url_fetches_a_tile() {
    let server = server();
    let template = format!("{}/{{z}}/{{x}}/{{y}}.pbf", server.origin());
    let url = expand(&template, 13, 4093, 2724, Scheme::Xyz, 1.0);
    assert!(url.ends_with("/13/4093/2724.pbf"), "{url}");

    let response = HttpFileSource::default().fetch(&url).expect("fetches");
    assert_eq!(response.status, 200);
    assert!(response.is_ok());
    assert_eq!(
        response.body.len(),
        FIXTURE.len(),
        "the whole body, not a short read"
    );
    assert_eq!(response.body, FIXTURE);
    assert!(response.etag.is_some(), "the server sends one");
    assert_eq!(server.paths(), ["/13/4093/2724.pbf"]);
}

/// The bytes that come off the socket decode as the tile they are.
///
/// The transport is only interesting if what it delivers is usable, and a body truncated at a
/// buffer boundary would still have a plausible length.
#[test]
fn a_fetched_tile_decodes() {
    let server = server();
    let url = expand(
        &format!("{}/{{z}}/{{x}}/{{y}}.pbf", server.origin()),
        0,
        0,
        0,
        Scheme::Xyz,
        1.0,
    );
    let response = HttpFileSource::default().fetch(&url).expect("fetches");

    let tile = tessella_source::mvt::Tile::decode(&response.body).expect("decodes");
    assert!(!tile.layers.is_empty(), "a real tile has layers");
    assert!(tile.layer("water").is_some(), "including this one");
}

/// A tile the source does not have is a response, not an error.
///
/// A source's coverage is not a rectangle. Treating the edge of it as a transport failure would
/// make every map with a bounded source log errors while working correctly.
#[test]
fn an_absent_tile_is_a_response() {
    let server = server();
    // Outside the server's zoom range.
    let url = expand(
        &format!("{}/{{z}}/{{x}}/{{y}}.pbf", server.origin()),
        20,
        0,
        0,
        Scheme::Xyz,
        1.0,
    );
    let response = HttpFileSource::default().fetch(&url).expect("no error");
    assert_eq!(response.status, 404);
    assert!(response.is_absent());
    assert!(!response.is_ok());
}

/// A refused connection is a transport error, and names the URL.
#[test]
fn a_dead_origin_is_a_transport_error() {
    // Bind and drop, so the port is one nothing is listening on.
    let addr = {
        let server = server();
        server.origin()
    };
    let error = HttpFileSource::default()
        .fetch(&format!("{addr}/0/0/0.pbf"))
        .expect_err("nothing is listening");
    match error {
        FetchError::Transport { url, .. } => assert!(url.contains("/0/0/0.pbf"), "{url}"),
        other => panic!("{other:?}"),
    }
}

/// Coalescing holds over the socket: four views, one request reaching the server.
///
/// The count is the server's own, so this is not measuring the client's bookkeeping against
/// itself. Without coalescing the server sees four connections.
#[test]
fn concurrent_views_produce_one_request() {
    const VIEWS: usize = 4;
    let server = server();
    let url = expand(
        &format!("{}/{{z}}/{{x}}/{{y}}.pbf", server.origin()),
        13,
        4093,
        2724,
        Scheme::Xyz,
        1.0,
    );

    let coalescing = Arc::new(Coalescing::new(HttpFileSource::default()));
    let start = Arc::new(Barrier::new(VIEWS));
    let handles: Vec<_> = (0..VIEWS)
        .map(|_| {
            let coalescing = Arc::clone(&coalescing);
            let start = Arc::clone(&start);
            let url = url.clone();
            std::thread::spawn(move || {
                start.wait();
                coalescing.fetch(&url)
            })
        })
        .collect();

    let bodies: Vec<usize> = handles
        .into_iter()
        .map(|handle| {
            handle
                .join()
                .expect("no panic")
                .expect("fetched")
                .body
                .len()
        })
        .collect();

    assert!(bodies.iter().all(|len| *len == FIXTURE.len()), "{bodies:?}");
    // A race is possible in principle — a thread could finish before another starts — so the
    // assertion is that work was saved, not that exactly one request happened. In practice the
    // barrier makes it one; asserting that would make this flaky rather than strict.
    assert!(
        server.requests() < VIEWS as u64,
        "{} requests for {VIEWS} views",
        server.requests()
    );
    assert_eq!(
        coalescing.stats().fetches() + coalescing.stats().waits(),
        VIEWS as u64,
        "every caller is accounted for"
    );
    assert_eq!(coalescing.stats().fetches(), server.requests());
}

/// A gzipped tile is inflated before it reaches the caller.
///
/// Every real vector-tile origin serves gzip — `pmtiles serve`, and every hosted basemap — so
/// a client without decompression gets `1f 8b 08` and a decoder that correctly refuses it. The
/// failure is far from its cause: the bytes arrive, the length is plausible, and the error is
/// about protobuf wire types.
///
/// The transport is where this belongs, not the decoder. `Content-Encoding` is a property of
/// the transfer, and a decoder that sniffed for gzip magic would also have to decide what to do
/// about a tile that is genuinely gzip-in-protobuf.
#[test]
fn a_gzipped_tile_is_inflated() {
    use std::io::Write as _;

    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(FIXTURE).expect("compresses");
    let gzipped = encoder.finish().expect("finishes");
    assert_eq!(&gzipped[..3], &[0x1f, 0x8b, 0x08], "it really is gzip");
    assert!(gzipped.len() < FIXTURE.len(), "and it really is smaller");

    let server = tile_server::Server::start(tile_server::Routes::new().tiles_encoded(
        gzipped,
        Some("gzip"),
        Some((0, 14)),
    ))
    .expect("binds");

    let url = expand(
        &format!("{}/{{z}}/{{x}}/{{y}}.pbf", server.origin()),
        0,
        0,
        0,
        Scheme::Xyz,
        1.0,
    );
    let response = HttpFileSource::default().fetch(&url).expect("fetches");

    assert_eq!(response.body, FIXTURE, "inflated back to the tile");
    tessella_source::mvt::Tile::decode(&response.body).expect("and it decodes");
}
