//! `mapbox://`, `maptiler://` and `maplibre://` rewriting, against mbgl's own expectations.
//!
//! The strings are `test/util/mapbox.test.cpp`'s, character for character. They are worth having
//! exactly rather than in spirit: every one of these URLs is fetched from a real server that
//! either has the resource at that address or does not, and a rewrite that is nearly right
//! produces a 404 with nothing in it to say which of a dozen rules was wrong.
//!
//! The pass-through cases matter as much as the rewrites. A style mixes hosted sources with
//! self-hosted ones, and a URL nothing claims must reach the transport exactly as written.

use tessella_storage::canonical::{CanonicalError, Kind, TileServer, normalize};

fn mapbox(kind: Kind, url: &str, key: &str) -> String {
    normalize(&TileServer::mapbox(), kind, url, key)
        .unwrap_or_else(|error| panic!("{url}: {error}"))
        .into_owned()
}

fn maptiler(kind: Kind, url: &str, key: &str) -> String {
    normalize(&TileServer::maptiler(), kind, url, key)
        .unwrap_or_else(|error| panic!("{url}: {error}"))
        .into_owned()
}

fn maplibre(kind: Kind, url: &str, key: &str) -> String {
    normalize(&TileServer::maplibre(), kind, url, key)
        .unwrap_or_else(|error| panic!("{url}: {error}"))
        .into_owned()
}

/// mbgl `Mapbox.SourceURL`.
///
/// `mapbox://user.map` is the whole address of a TileJSON, and everything after it — the `/v4`
/// version prefix, the `.json` suffix, the access token, the `&secure` — is supplied here. A
/// build without this rewriting fetches nothing at all: the failure is not a 404 but a scheme no
/// transport claims.
#[test]
fn a_mapbox_source_url_normalizes() {
    assert_eq!(
        mapbox(Kind::Source, "mapbox://user.map", "key"),
        "https://api.mapbox.com/v4/user.map.json?access_token=key&secure"
    );
}

/// A source URL's own query survives, and its `?` becomes an `&`.
///
/// mbgl `Mapbox.SourceURL`'s third case. The template already contributed a `?` for the access
/// token, so the original's has to become an ampersand — two question marks make every server
/// read the second as a literal inside the first parameter's value, which for a signed URL means
/// the signature silently stops matching.
#[test]
fn a_source_urls_query_is_carried_across() {
    assert_eq!(
        mapbox(
            Kind::Source,
            "mapbox://user.map?style=mapbox://styles/mapbox/streets-v9@0",
            "key"
        ),
        "https://api.mapbox.com/v4/user.map.json?access_token=key&secure&style=mapbox://styles/mapbox/streets-v9@0"
    );
}

/// A bare `?` carries nothing and is dropped.
///
/// mbgl `Mapbox.SourceURL`'s fourth case, and the reason the test is `query.second > 1` rather
/// than a non-empty check: a trailing question mark is a span of one, and appending it would put
/// `&` on the end of every such URL.
#[test]
fn a_bare_question_mark_is_dropped() {
    assert_eq!(
        mapbox(Kind::Source, "mapbox://user.map?", "key"),
        "https://api.mapbox.com/v4/user.map.json?access_token=key&secure"
    );
}

/// A URL this server does not claim passes through untouched.
///
/// The common case, and the one a rewriting layer must not get wrong: a style mixes hosted
/// sources with self-hosted ones, and the second kind has to reach the transport exactly as
/// written — query string, port, credentials and all.
#[test]
fn an_unclaimed_url_passes_through() {
    for kind in [
        Kind::Source,
        Kind::Style,
        Kind::Sprite,
        Kind::Glyphs,
        Kind::Tile,
    ] {
        assert_eq!(mapbox(kind, "http://path", "key"), "http://path");
        assert_eq!(
            maptiler(kind, "https://api.tileserver.com/map?key=1234", ""),
            "https://api.tileserver.com/map?key=1234"
        );
        assert_eq!(maptiler(kind, "", ""), "");
    }

    // And it is borrowed rather than rebuilt: this runs per resource of every style, and the
    // overwhelming majority of URLs in a self-hosted style are in this branch.
    let url = "https://tiles.example.com/{z}/{x}/{y}.pbf";
    let passed = normalize(&TileServer::mapbox(), Kind::Tile, url, "key").expect("passes through");
    assert!(matches!(passed, std::borrow::Cow::Borrowed(_)));
}

/// mbgl `Mapbox.SourceURL`'s last case: a source with no key is refused outright.
///
/// mbgl throws here and only logs for the other four kinds, and the asymmetry is deliberate on
/// its part. A source with no key produces a TileJSON fetch that fails and takes every tile of
/// that source with it; a sprite with no key is one missing picture on a map that otherwise
/// works. One error naming the parameter is more use than a hundred 401s.
#[test]
fn a_source_without_an_api_key_is_refused() {
    let failure = normalize(&TileServer::mapbox(), Kind::Source, "mapbox://user.map", "")
        .expect_err("a mapbox source needs a token");
    let CanonicalError::MissingApiKey { parameter, .. } = &failure else {
        panic!("expected a missing key, got {failure:?}");
    };
    assert_eq!(*parameter, "access_token");

    // A server that needs no key does not refuse.
    assert_eq!(
        maplibre(Kind::Source, "maplibre://tiles/tiles", ""),
        "https://demotiles.maplibre.org/tiles/tiles.json"
    );
}

/// mbgl `Mapbox.GlyphsURL`.
///
/// The percent-encoded font name survives the rewrite unchanged, and so do the `{fontstack}` and
/// `{range}` tokens — they are the glyph manager's to fill in later, and a rewriter that tried
/// to resolve them here would produce a URL for a font stack nobody asked for.
#[test]
fn a_mapbox_glyphs_url_normalizes() {
    assert_eq!(
        mapbox(
            Kind::Glyphs,
            "mapbox://fonts/boxmap/Comic%20Sans/0-255.pbf",
            "key"
        ),
        "https://api.mapbox.com/fonts/v1/boxmap/Comic%20Sans/0-255.pbf?access_token=key"
    );
    assert_eq!(
        mapbox(
            Kind::Glyphs,
            "mapbox://fonts/boxmap/{fontstack}/{range}.pbf",
            "key"
        ),
        "https://api.mapbox.com/fonts/v1/boxmap/{fontstack}/{range}.pbf?access_token=key"
    );
}

/// mbgl `Mapbox.StyleURL`, including the draft variant.
#[test]
fn a_mapbox_style_url_normalizes() {
    assert_eq!(
        mapbox(Kind::Style, "mapbox://styles/user/style", "key"),
        "https://api.mapbox.com/styles/v1/user/style?access_token=key"
    );
    assert_eq!(
        mapbox(Kind::Style, "mapbox://styles/user/style/draft", "key"),
        "https://api.mapbox.com/styles/v1/user/style/draft?access_token=key"
    );
    assert_eq!(
        mapbox(Kind::Style, "mapbox://styles/user/style?shave=true", "key"),
        "https://api.mapbox.com/styles/v1/user/style?access_token=key&shave=true"
    );
    assert_eq!(
        mapbox(Kind::Style, "mapbox://styles/user/style?", "key"),
        "https://api.mapbox.com/styles/v1/user/style?access_token=key"
    );
}

/// mbgl `Mapbox.SpriteURL`, which is where the `@2x` rule earns its place.
///
/// A sprite's scale suffix is part of the *extension*, not the filename: `streets-v8@2x.png`
/// splits as `streets-v8` and `@2x.png`, so the template's `{filename}/sprite{extension}` puts
/// the scale back on the far side of the rewrite. Reading `@2x` as part of the filename produces
/// `streets-v8@2x/sprite.png` — a directory that does not exist.
#[test]
fn a_sprite_urls_scale_is_part_of_its_extension() {
    assert_eq!(
        mapbox(
            Kind::Sprite,
            "mapbox://sprites/mapbox/streets-v8.json",
            "key"
        ),
        "https://api.mapbox.com/styles/v1/mapbox/streets-v8/sprite.json?access_token=key"
    );
    assert_eq!(
        mapbox(
            Kind::Sprite,
            "mapbox://sprites/mapbox/streets-v8@2x.png",
            "key"
        ),
        "https://api.mapbox.com/styles/v1/mapbox/streets-v8/sprite@2x.png?access_token=key"
    );
    assert_eq!(
        mapbox(
            Kind::Sprite,
            "mapbox://sprites/mapbox/streets-v8/draft@2x.png",
            "key"
        ),
        "https://api.mapbox.com/styles/v1/mapbox/streets-v8/draft/sprite@2x.png?access_token=key"
    );
}

/// mbgl `Mapbox.SpriteURL`'s oddest case, kept because mbgl keeps it.
///
/// `mapbox://sprites/mapbox/streets-v11?fresh=true.png` has its `.png` *inside the query*, so the
/// path has no extension at all and the query is appended whole. The result is not a URL anyone
/// meant to write, and reproducing it is the point: an implementation that "fixed" it here would
/// disagree with the oracle on an input a style can contain.
#[test]
fn an_extension_inside_the_query_stays_there() {
    assert_eq!(
        mapbox(
            Kind::Sprite,
            "mapbox://sprites/mapbox/streets-v11?fresh=true.png",
            "key"
        ),
        "https://api.mapbox.com/styles/v1/mapbox/streets-v11/sprite?access_token=key&fresh=true.png"
    );
}

/// mbgl `Mapbox.TileURL`, including the multi-source composite form.
///
/// `a.b,c.d` is Mapbox's way of asking one request for two tilesets composited server-side. It
/// is a single path segment containing a comma, and a rewriter that split on punctuation would
/// turn it into two requests for tilesets that do not exist.
#[test]
fn a_mapbox_tile_url_normalizes() {
    assert_eq!(
        mapbox(Kind::Tile, "mapbox://tiles/a.b/0/0/0.pbf", "key"),
        "https://api.mapbox.com/v4/a.b/0/0/0.pbf?access_token=key"
    );
    assert_eq!(
        mapbox(Kind::Tile, "mapbox://tiles/a.b/0/0/0@2x.png", "key"),
        "https://api.mapbox.com/v4/a.b/0/0/0@2x.png?access_token=key"
    );
    assert_eq!(
        mapbox(Kind::Tile, "mapbox://tiles/a.b,c.d/0/0/0.pbf", "key"),
        "https://api.mapbox.com/v4/a.b,c.d/0/0/0.pbf?access_token=key"
    );
}

/// A URL offered as the wrong resource kind is refused rather than rewritten.
///
/// `mapbox://fonts/...` is a glyph URL and `mapbox://styles/...` is a style URL. mbgl checks the
/// domain against the kind and logs when it disagrees; rewriting anyway would produce a
/// well-formed address for a resource that does not exist, which arrives as a 404 with nothing
/// in it to say the caller asked the wrong question.
#[test]
fn a_url_of_the_wrong_kind_is_refused() {
    let failure = normalize(
        &TileServer::mapbox(),
        Kind::Style,
        "mapbox://fonts/boxmap/Comic%20Sans/0-255.pbf",
        "key",
    )
    .expect_err("a glyph url is not a style url");
    assert!(
        matches!(failure, CanonicalError::WrongDomain { .. }),
        "{failure:?}"
    );

    // And the source rule, whose domain is empty, accepts anything — which is what makes
    // `mapbox://user.map` a source at all: there is no `sources` segment in front of it.
    assert_eq!(
        mapbox(Kind::Source, "mapbox://anything.at.all", "key"),
        "https://api.mapbox.com/v4/anything.at.all.json?access_token=key&secure"
    );
}

/// mbgl `MapTiler.StyleURL`, `SourceURL`, `GlyphsURL`, `Sprites` and `Tiles`.
///
/// A different vendor with a different key parameter, different templates and no `&secure`, and
/// not a line of vendor-specific code between them: the differences are all data. Checking a
/// second vendor is what says so — a build with Mapbox's rules hard-coded passes every test
/// above and none of these.
#[test]
fn maptilers_rules_are_the_same_machinery() {
    assert_eq!(
        maptiler(Kind::Style, "maptiler://maps/basic", "abcdef"),
        "https://api.maptiler.com/maps/basic/style.json?key=abcdef"
    );
    assert_eq!(
        maptiler(Kind::Source, "maptiler://sources/v3", "abcdef"),
        "https://api.maptiler.com/tiles/v3/tiles.json?key=abcdef"
    );
    assert_eq!(
        maptiler(
            Kind::Source,
            "maptiler://sources/7ac429c7-c96e-46dd-8c3e-13d48988986a",
            "abcdef"
        ),
        "https://api.maptiler.com/tiles/7ac429c7-c96e-46dd-8c3e-13d48988986a/tiles.json?key=abcdef"
    );
    assert_eq!(
        maptiler(
            Kind::Glyphs,
            "maptiler://fonts/{fontstack}/{range}.pbf",
            "abcdef"
        ),
        "https://api.maptiler.com/fonts/{fontstack}/{range}.pbf?key=abcdef"
    );
    assert_eq!(
        maptiler(Kind::Sprite, "maptiler://sprites/streets/sprite", ""),
        "https://api.maptiler.com/maps/streets/sprite"
    );
    assert_eq!(
        maptiler(
            Kind::Tile,
            "maptiler://tiles/tiles/contours/{z}/{x}/{y}.pbf",
            "abcdef"
        ),
        "https://api.maptiler.com/tiles/contours/{z}/{x}/{y}.pbf?key=abcdef"
    );
}

/// A key that is not supplied is simply absent, for the kinds that do not require one.
///
/// mbgl `MapTiler.StyleURL`'s first case. The style still resolves to a real address; the server
/// answers 403 rather than the client refusing to ask. That is the right split — a public
/// MapTiler style genuinely works without a key.
#[test]
fn a_missing_key_leaves_the_parameter_off() {
    assert_eq!(
        maptiler(Kind::Style, "maptiler://maps/basic", ""),
        "https://api.maptiler.com/maps/basic/style.json"
    );
    assert_eq!(
        maptiler(Kind::Glyphs, "maptiler://fonts/{fontstack}/{range}.pbf", ""),
        "https://api.maptiler.com/fonts/{fontstack}/{range}.pbf"
    );
}

/// mbgl `MapLibre.CanonicalURL`: the demo server, which needs no key at all.
#[test]
fn maplibres_demo_server_needs_no_key() {
    assert_eq!(
        maplibre(Kind::Style, "maplibre://maps/style", ""),
        "https://demotiles.maplibre.org/style.json"
    );
    assert_eq!(
        maplibre(Kind::Source, "maplibre://tiles/tiles", ""),
        "https://demotiles.maplibre.org/tiles/tiles.json"
    );
    assert_eq!(
        maplibre(
            Kind::Glyphs,
            "maplibre://fonts/{fontstack}/{start}-{end}.pbf",
            ""
        ),
        "https://demotiles.maplibre.org/font/{fontstack}/{start}-{end}.pbf"
    );
}

/// `mapbox://////` parses the way mbgl parses it, and is answered differently on purpose.
///
/// The parse agrees: skipping the run of `:` and `/` leaves the domain *empty*, which is neither
/// `sprites` nor anything else, so the URL names no resource kind. mbgl logs that and returns the
/// input — which hands the transport a `mapbox://` URL no transport claims, and the caller learns
/// about it as an unknown scheme several layers away from the style that wrote it.
///
/// This reports instead, and it is a deliberate divergence rather than an oversight. The
/// information is available exactly here and nowhere later: the rewriter knows which kind was
/// asked for, which domain the URL carried, and that they disagree. Passing the URL through
/// throws all three away and replaces them with a fetch failure.
///
/// The divergence is safe because no well-formed style reaches it. A real `mapbox://styles/...`
/// carries `styles`, a real `mapbox://sprites/...` carries `sprites`, and the source rule accepts
/// any domain because a source URL has no kind segment in front of it at all.
#[test]
fn a_degenerate_canonical_url_is_reported_rather_than_passed_on() {
    use tessella_storage::canonical::Url;

    // The parse itself is mbgl's, which is what makes the divergence about the *answer* rather
    // than about reading the URL.
    let parsed = Url::parse("mapbox://////");
    assert_eq!(Url::slice("mapbox://////", parsed.scheme), "mapbox");
    assert_eq!(
        Url::slice("mapbox://////", parsed.domain),
        "",
        "mbgl's empty domain"
    );

    let failure = normalize(&TileServer::mapbox(), Kind::Sprite, "mapbox://////", "key")
        .expect_err("it names no resource kind");
    let CanonicalError::WrongDomain { url, kind } = &failure else {
        panic!("expected a wrong domain, got {failure:?}");
    };
    assert_eq!(url, "mapbox://////");
    assert_eq!(*kind, "sprite");
}

/// The URL parser splits where mbgl splits, including where that is surprising.
///
/// Checked directly rather than only through the rewrites, because the spans are what every
/// template reads and a template can be right while the span it fills is wrong. The `@2x` rule is
/// the one worth stating twice: it belongs to the extension, so a scale suffix survives a
/// filename-and-extension rewrite instead of being carried into a directory name.
#[test]
fn the_url_and_path_split_where_the_oracle_splits() {
    use tessella_storage::canonical::{PathParts, Url};

    let text = "https://api.mapbox.com/styles/v1/mapbox/streets-v8/sprite@2x.png?access_token=k";
    let url = Url::parse(text);
    assert_eq!(Url::slice(text, url.scheme), "https");
    assert_eq!(Url::slice(text, url.domain), "api.mapbox.com");
    assert_eq!(
        Url::slice(text, url.path),
        "/styles/v1/mapbox/streets-v8/sprite@2x.png"
    );
    assert_eq!(
        Url::slice(text, url.query),
        "?access_token=k",
        "the query keeps its `?`"
    );

    let path = PathParts::parse(text, url.path.0, url.path.1);
    assert_eq!(
        Url::slice(text, path.directory),
        "/styles/v1/mapbox/streets-v8/"
    );
    assert_eq!(Url::slice(text, path.filename), "sprite");
    assert_eq!(
        Url::slice(text, path.extension),
        "@2x.png",
        "the scale is the extension"
    );

    // A path with no dot has no extension and an empty-suffixed filename, rather than treating
    // the last segment as one.
    let plain = "https://host/a/b/c";
    let url = Url::parse(plain);
    let path = PathParts::parse(plain, url.path.0, url.path.1);
    assert_eq!(Url::slice(plain, path.filename), "c");
    assert_eq!(Url::slice(plain, path.extension), "");

    // A `#` before the `?` means the fragment swallowed it: there is no query at all.
    let fragment = "https://host/a#frag?notaquery";
    let url = Url::parse(fragment);
    assert_eq!(Url::slice(fragment, url.query), "");
    assert_eq!(Url::slice(fragment, url.path), "/a");
}

/// The resource kind is read off the URL, which is what lets a transport rewrite one.
///
/// A file source is handed a URL and no context. The kind is written in the URL's own domain
/// segment, so nothing has to be passed alongside it — and anything the server does not
/// recognise is a source, because a source URL has no kind segment at all.
#[test]
fn a_canonical_urls_kind_is_written_in_it() {
    let mapbox = TileServer::mapbox();
    assert_eq!(Kind::of(&mapbox, "mapbox://styles/u/s"), Some(Kind::Style));
    assert_eq!(
        Kind::of(&mapbox, "mapbox://sprites/u/s"),
        Some(Kind::Sprite)
    );
    assert_eq!(Kind::of(&mapbox, "mapbox://fonts/u/f"), Some(Kind::Glyphs));
    assert_eq!(
        Kind::of(&mapbox, "mapbox://tiles/a.b/0/0/0.pbf"),
        Some(Kind::Tile)
    );
    assert_eq!(Kind::of(&mapbox, "mapbox://user.map"), Some(Kind::Source));
    assert_eq!(
        Kind::of(&mapbox, "https://tiles.example.com/0/0/0.pbf"),
        None
    );

    // The same URL under a different vendor is not claimed at all.
    assert_eq!(
        Kind::of(&TileServer::maptiler(), "mapbox://styles/u/s"),
        None
    );
}

/// The wrapper rewrites on the way to the transport, and leaves everything above it canonical.
mod through_the_transport {
    use std::sync::Mutex;

    use tessella_storage::canonical::{Canonical, TileServer};
    use tessella_storage::source::{FetchError, FileSource, Response};

    /// A transport that records what it was asked for.
    ///
    /// A `Mutex` rather than a `RefCell` because `FileSource` is `Sync`: a real one is shared
    /// between workers, and the trait says so.
    #[derive(Default)]
    struct Recorder {
        seen: Mutex<Vec<String>>,
    }

    impl Recorder {
        fn seen(&self) -> Vec<String> {
            self.seen.lock().expect("not poisoned").clone()
        }
    }

    impl FileSource for Recorder {
        fn fetch(&self, url: &str) -> Result<Response, FetchError> {
            self.seen
                .lock()
                .expect("not poisoned")
                .push(url.to_string());
            Ok(Response {
                status: 200,
                ..Response::default()
            })
        }
    }

    /// The transport sees the address with the key; nothing above it does.
    ///
    /// Which is the whole reason the wrapper sits at the bottom of the stack: the coalescing
    /// table and the byte cache key on the URL they were given, so they key on the canonical
    /// form and never on a credential.
    #[test]
    fn the_key_appears_only_at_the_transport() {
        let source = Canonical::new(Recorder::default(), TileServer::mapbox(), "secret");
        source
            .fetch("mapbox://tiles/a.b/0/0/0.pbf")
            .expect("fetches");

        let seen = source.inner().seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            seen[0],
            "https://api.mapbox.com/v4/a.b/0/0/0.pbf?access_token=secret"
        );
    }

    /// Two keys produce the same canonical URL and two different requests.
    ///
    /// An API key is a credential with a lifetime of its own — rotated, refreshed, scoped per
    /// user — and a cache keyed on the address it appears in would treat every rotation as a
    /// cold start for the whole downloaded region. The key never reaches the layer that
    /// remembers.
    #[test]
    fn a_rotated_key_does_not_change_the_canonical_url() {
        let canonical = "mapbox://tiles/a.b/0/0/0.pbf";
        let mut fetched = Vec::new();
        for key in ["old", "new"] {
            let source = Canonical::new(Recorder::default(), TileServer::mapbox(), key);
            source.fetch(canonical).expect("fetches");
            fetched.push(source.inner().seen()[0].clone());
        }

        assert_ne!(
            fetched[0], fetched[1],
            "the transport saw the same address twice"
        );
        assert!(fetched[0].ends_with("access_token=old"));
        assert!(fetched[1].ends_with("access_token=new"));
    }

    /// A URL the server does not claim reaches the transport untouched.
    #[test]
    fn an_unclaimed_url_is_not_rewritten() {
        let source = Canonical::new(Recorder::default(), TileServer::mapbox(), "secret");
        let url = "https://tiles.example.com/0/0/0.pbf?token=mine";
        source.fetch(url).expect("fetches");
        assert_eq!(source.inner().seen()[0], url);
    }

    /// A URL that cannot be rewritten fails with the reason rather than being passed on.
    ///
    /// Passing it on would hand the transport a `mapbox://` address no transport claims, and the
    /// caller would learn about it as an unknown scheme several layers from the style that wrote
    /// it. Nothing reaches the socket.
    #[test]
    fn a_source_with_no_key_never_reaches_the_socket() {
        let source = Canonical::new(Recorder::default(), TileServer::mapbox(), "");
        let failure = source
            .fetch("mapbox://user.map")
            .expect_err("a mapbox source needs a token");
        assert!(failure.to_string().contains("access_token"), "{failure}");
        assert!(source.inner().seen().is_empty(), "it fetched anyway");
    }
}

/// A cached body outlives the credential that fetched it.
///
/// The claim the wrapper's position in the stack exists to make, checked where it is observable:
/// the cache keys on the URL it was handed, the wrapper adds the key below it, so rotating the
/// key is a cache *hit* rather than a cold start. An API key has a lifetime of its own — rotated,
/// refreshed, scoped per user — and a cache keyed on the address it appears in would throw away
/// a downloaded region every time one changed.
#[cfg(feature = "cache")]
mod across_a_rotation {
    use std::sync::Mutex;

    use tessella_storage::cache::{CachingFileSource, SqliteCache};
    use tessella_storage::canonical::{Canonical, TileServer};
    use tessella_storage::source::{FetchError, FileSource, Response};

    #[derive(Default)]
    struct Counting {
        calls: Mutex<Vec<String>>,
    }

    impl FileSource for Counting {
        fn fetch(&self, url: &str) -> Result<Response, FetchError> {
            self.calls
                .lock()
                .expect("not poisoned")
                .push(url.to_string());
            Ok(Response {
                status: 200,
                body: b"a tile".to_vec(),
                // Fresh for an hour, so the second request is a hit rather than a revalidation.
                max_age: Some(3600),
                ..Response::default()
            })
        }
    }

    fn stack(path: &std::path::Path, key: &str) -> CachingFileSource<Canonical<Counting>> {
        let cache = SqliteCache::open(path).expect("opens");
        CachingFileSource::with_clock(
            Canonical::new(Counting::default(), TileServer::mapbox(), key),
            cache,
            || 1_700_000_000,
        )
    }

    #[test]
    fn a_rotated_key_hits_the_cache() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("cache.db");
        let canonical = "mapbox://tiles/a.b/3/2/1.vector.pbf";

        let first = stack(&path, "the-old-token");
        assert_eq!(first.fetch(canonical).expect("fetches").body, b"a tile");
        assert_eq!(first.stats().fetched(), 1);
        assert_eq!(first.stats().hits(), 0);
        assert_eq!(
            first.inner().inner().calls.lock().expect("not poisoned")[0],
            "https://api.mapbox.com/v4/a.b/3/2/1.vector.pbf?access_token=the-old-token"
        );
        drop(first);

        // A new process, a new token, the same cache file.
        let second = stack(&path, "a-freshly-rotated-token");
        assert_eq!(second.fetch(canonical).expect("fetches").body, b"a tile");
        assert_eq!(second.stats().hits(), 1, "the rotation was a cold start");
        assert_eq!(second.stats().fetched(), 0);
        assert!(
            second
                .inner()
                .inner()
                .calls
                .lock()
                .expect("not poisoned")
                .is_empty(),
            "it went to the network anyway"
        );
    }

    /// And a *different* tile under the same key is still a miss, so the hit above is not the
    /// cache answering everything.
    #[test]
    fn a_different_tile_is_still_fetched() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("cache.db");

        let source = stack(&path, "token");
        source
            .fetch("mapbox://tiles/a.b/3/2/1.vector.pbf")
            .expect("fetches");
        source
            .fetch("mapbox://tiles/a.b/3/2/2.vector.pbf")
            .expect("fetches");
        assert_eq!(source.stats().fetched(), 2);
        assert_eq!(source.stats().hits(), 0);
    }
}
