//! A hosted style boots: `mapbox://` in the document, HTTPS with a key on the wire.
//!
//! The unit tests in `tessella-storage` check the rewriting against mbgl's own strings. This
//! checks the thing those rewrites exist for — that a style written the way a vendor writes it
//! reaches tiles at all — and that the layers above the transport never see the key.

use std::sync::{Arc, Mutex};

use tessella_orchestrate::boot::{ColdStart, Workers};
use tessella_orchestrate::cache::TileCache;
use tessella_orchestrate::pool::{Pool, Priority};
use tessella_storage::canonical::{Canonical, TileServer};
use tessella_storage::source::{Coalescing, FetchError, FileSource, Response};
use tessella_tile::cover::ViewTransform;

const MVT: &[u8] = include_bytes!("../../../tests/mvt-fixtures/real-world-0-0-0.mvt");

/// An origin that answers a TileJSON for one address and tiles for the rest, recording both.
#[derive(Default)]
struct Hosted {
    seen: Mutex<Vec<String>>,
}

impl Hosted {
    fn seen(&self) -> Vec<String> {
        self.seen.lock().expect("not poisoned").clone()
    }
}

impl FileSource for Hosted {
    fn fetch(&self, url: &str) -> Result<Response, FetchError> {
        self.seen
            .lock()
            .expect("not poisoned")
            .push(url.to_string());

        // The manifest a Mapbox source resolves to, whose own tile templates are the canonical
        // form — which is what a real `v4/*.json` carries.
        let body = if url.contains("/v4/user.map.json") {
            br#"{"tiles": ["mapbox://tiles/user.map/{z}/{x}/{y}.vector.pbf"],
                 "minzoom": 0, "maxzoom": 14}"#
                .to_vec()
        } else {
            MVT.to_vec()
        };
        Ok(Response {
            status: 200,
            body,
            ..Response::default()
        })
    }
}

const STYLE: &str = r#"{"version": 8,
    "sources": {"base": {"type": "vector", "url": "mapbox://user.map"}},
    "layers": [{"id": "roads", "type": "line", "source": "base", "source-layer": "road"}]}"#;

fn view() -> ViewTransform {
    ViewTransform {
        longitude: -0.11,
        latitude: 51.505,
        zoom: 2.0,
        width: 512.0,
        height: 512.0,
        bearing: 0.0,
        pitch: 0.0,
    }
}

/// A style whose source is `mapbox://user.map` boots and builds tiles.
///
/// Nothing in the document is an HTTP URL: the source is a canonical address, and the manifest
/// it resolves to hands back canonical tile templates. Without the rewriting the boot fetches
/// nothing at all, and the failure is not a 404 but a scheme no transport claims.
#[test]
fn a_hosted_style_boots_through_canonical_urls() {
    let origin = Arc::new(Coalescing::new(Canonical::new(
        Hosted::default(),
        TileServer::mapbox(),
        "secret-token",
    )));
    let pool = Pool::new(Workers::serial());
    let booted = tessella_orchestrate::boot::cold_start(&ColdStart {
        style: STYLE,
        view: &view(),
        files: Arc::clone(&origin),
        cache: Arc::new(TileCache::new(64)),
        pool: &pool,
        priority: Priority::Foreground,
        style_rev: 1,
    })
    .expect("the style boots");

    assert!(!booted.tiles.is_empty(), "no tiles were built");
    assert!(
        booted.tiles.iter().any(|tile| !tile.buckets.is_empty()),
        "tiles built no buckets"
    );

    // Every address the transport used is the vendor's, and every one carries the key.
    let seen = origin.inner().inner().seen();
    assert!(!seen.is_empty());
    for url in &seen {
        assert!(url.starts_with("https://api.mapbox.com/v4/"), "{url}");
        assert!(url.contains("access_token=secret-token"), "{url}");
    }
    assert!(
        seen.iter().any(|url| url.contains("/v4/user.map.json")),
        "the manifest was never fetched: {seen:?}"
    );
}

/// A style with no key fails at the source, naming the parameter.
///
/// One error naming the missing token is more use than a hundred 401s, and it arrives before a
/// socket is opened rather than after every tile of the cover has failed.
#[test]
fn a_hosted_style_without_a_key_says_so() {
    let origin = Arc::new(Coalescing::new(Canonical::new(
        Hosted::default(),
        TileServer::mapbox(),
        "",
    )));
    let pool = Pool::new(Workers::serial());
    let failure = tessella_orchestrate::boot::cold_start(&ColdStart {
        style: STYLE,
        view: &view(),
        files: Arc::clone(&origin),
        cache: Arc::new(TileCache::new(64)),
        pool: &pool,
        priority: Priority::Foreground,
        style_rev: 1,
    })
    .expect_err("a mapbox source needs a token");

    assert!(failure.to_string().contains("access_token"), "{failure}");
    assert!(
        origin.inner().inner().seen().is_empty(),
        "it opened a connection anyway"
    );
}
