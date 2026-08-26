//! `mapbox://`, `maptiler://` and `maplibre://` URLs, and the API key that goes with them.
//!
//! Transcribed from mbgl's `util::mapbox` and `util::URL`/`util::Path`
//! (`util/mapbox.cpp`, `util/url.cpp`, `util/tile_server_options.cpp`).
//!
//! # Why a style needs this before it can load at all
//!
//! A hosted style does not write out its own URLs. `mapbox://styles/mapbox/streets-v11` is the
//! *whole* address of a Mapbox style, and every source, sprite and glyph range inside it is
//! written the same way. There is no HTTP in any of it. A build with no rewriting fetches
//! nothing, and the failure is not a 404 — it is a scheme no transport claims.
//!
//! The rewrite is also where the API key goes. It is a query parameter whose *name* differs by
//! vendor — `access_token` for Mapbox, `key` for MapTiler — and which is appended to every
//! request derived from the style. Putting it anywhere else means either hard-coding one
//! vendor's spelling or asking the caller to paste it into a template by hand.
//!
//! # One shape, three vendors, and no vendor branch in the code
//!
//! mbgl expresses all of this as data — a [`TileServer`] of templates and domain names — so
//! that a self-hosted server is configured rather than special-cased. This does the same. The
//! only place a vendor is named is in the three constructors, and even Mapbox's one oddity
//! (`&secure` on a source URL) is a field rather than an `if`.
//!
//! # Everything that is not canonical passes through untouched
//!
//! A style is free to mix `mapbox://` sources with plain HTTPS ones, and the plain ones must
//! arrive at the transport exactly as written — query string, port, credentials and all. So the
//! first line of every function here is a test for the scheme, and the identity case is the
//! common one.

use std::borrow::Cow;

use crate::url::replace_tokens;

/// One resource kind's rewriting rule.
///
/// Three parts, because mbgl's `withXTemplate(template, domainName, versionPrefix)` has three.
/// The domain name is what the canonical URL's *host* position has to say — `mapbox://tiles/...`
/// has a domain of `tiles` — and a URL naming something else is rejected rather than rewritten,
/// because a rewrite would produce a plausible address for a resource nobody asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rule {
    /// Path template, with `{path}`, `{domain}`, `{directory}`, `{filename}` and `{extension}`.
    pub template: &'static str,
    /// The domain the canonical URL must carry, or empty to accept any.
    pub domain: &'static str,
    /// Version segment inserted between the base URL and the template, if the vendor has one.
    pub version_prefix: Option<&'static str>,
}

/// A tile server's URL scheme, as data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileServer {
    /// Where the vendor's API lives, with no trailing slash.
    pub base_url: &'static str,
    /// The URI scheme a canonical URL uses, without `://`.
    pub scheme_alias: &'static str,
    /// Query parameter the API key travels in, or empty for a server that needs none.
    pub api_key_parameter: &'static str,
    /// Whether a request without a key is refused rather than merely unauthenticated.
    pub requires_api_key: bool,
    /// Appended verbatim to a normalized *source* URL.
    ///
    /// Mapbox's `&secure`, and nothing else's. It is a field rather than a branch so that the
    /// vendor list stays data — mbgl writes it as `if (uriSchemeAlias == "mapbox")`, which is
    /// the one place its own configuration leaks into its code.
    pub source_suffix: &'static str,
    /// TileJSON and inline source documents.
    pub source: Rule,
    /// Style documents.
    pub style: Rule,
    /// Sprite indexes and sheets.
    pub sprites: Rule,
    /// Glyph ranges.
    pub glyphs: Rule,
    /// Tiles.
    pub tile: Rule,
}

impl TileServer {
    /// Mapbox.
    ///
    /// The `/v4` version prefix on sources and tiles is Mapbox's alone, and so is `&secure`.
    #[must_use]
    pub const fn mapbox() -> Self {
        Self {
            base_url: "https://api.mapbox.com",
            scheme_alias: "mapbox",
            api_key_parameter: "access_token",
            requires_api_key: true,
            source_suffix: "&secure",
            source: Rule {
                template: "/{domain}.json",
                domain: "",
                version_prefix: Some("/v4"),
            },
            style: Rule {
                template: "/styles/v1{path}",
                domain: "styles",
                version_prefix: None,
            },
            sprites: Rule {
                template: "/styles/v1{directory}{filename}/sprite{extension}",
                domain: "sprites",
                version_prefix: None,
            },
            glyphs: Rule {
                template: "/fonts/v1{path}",
                domain: "fonts",
                version_prefix: None,
            },
            tile: Rule {
                template: "{path}",
                domain: "tiles",
                version_prefix: Some("/v4"),
            },
        }
    }

    /// MapTiler.
    #[must_use]
    pub const fn maptiler() -> Self {
        Self {
            base_url: "https://api.maptiler.com",
            scheme_alias: "maptiler",
            api_key_parameter: "key",
            requires_api_key: true,
            source_suffix: "",
            source: Rule {
                template: "/tiles{path}/tiles.json",
                domain: "sources",
                version_prefix: None,
            },
            style: Rule {
                template: "/maps{path}/style.json",
                domain: "maps",
                version_prefix: None,
            },
            sprites: Rule {
                template: "/maps{path}",
                domain: "sprites",
                version_prefix: None,
            },
            glyphs: Rule {
                template: "/fonts{path}",
                domain: "fonts",
                version_prefix: None,
            },
            tile: Rule {
                template: "{path}",
                domain: "tiles",
                version_prefix: None,
            },
        }
    }

    /// MapLibre's demo tiles.
    ///
    /// The one server here that needs no key, which is why `requires_api_key` is a field and not
    /// an assumption: a normalize that demanded a key would make the demo style unloadable.
    #[must_use]
    pub const fn maplibre() -> Self {
        Self {
            base_url: "https://demotiles.maplibre.org",
            scheme_alias: "maplibre",
            api_key_parameter: "",
            requires_api_key: false,
            source_suffix: "",
            source: Rule {
                template: "/tiles/{domain}.json",
                domain: "",
                version_prefix: None,
            },
            style: Rule {
                template: "{path}.json",
                domain: "maps",
                version_prefix: None,
            },
            sprites: Rule {
                template: "/{path}/sprite{scale}.{format}",
                domain: "",
                version_prefix: None,
            },
            glyphs: Rule {
                template: "/font/{fontstack}/{start}-{end}.pbf",
                domain: "fonts",
                version_prefix: None,
            },
            tile: Rule {
                template: "/{path}",
                domain: "tiles",
                version_prefix: None,
            },
        }
    }

    /// Whether this URL is one of ours to rewrite.
    #[must_use]
    pub fn claims(&self, url: &str) -> bool {
        if self.scheme_alias.is_empty() || self.base_url.is_empty() {
            return false;
        }
        url.len() > self.scheme_alias.len() + 2
            && url.starts_with(self.scheme_alias)
            && url[self.scheme_alias.len()..].starts_with("://")
    }
}

/// Why a canonical URL could not be rewritten.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CanonicalError {
    /// The server requires an API key and none was given.
    ///
    /// Reported rather than fetched-and-401'd. Every request derived from the style would fail
    /// the same way, and one error naming the missing key is more use than a hundred 401s.
    #[error("`{url}` needs an API key: pass the `{parameter}` value for this tile server")]
    MissingApiKey {
        /// The URL that could not be rewritten.
        url: String,
        /// What the vendor calls its key.
        parameter: &'static str,
    },
    /// The URL's domain is not the one this resource kind uses.
    ///
    /// `mapbox://fonts/...` is a glyph URL and `mapbox://styles/...` is a style URL; asking to
    /// normalize one as the other would produce a well-formed address for a resource that does
    /// not exist. mbgl logs and returns the input unchanged; this reports, because a style that
    /// names the wrong resource kind is a fault the caller should see.
    #[error("`{url}` is not a {kind} URL for this tile server")]
    WrongDomain {
        /// The URL that could not be rewritten.
        url: String,
        /// Which resource kind it was offered as.
        kind: &'static str,
    },
}

/// A parsed URL, as offsets into the original.
///
/// Deliberately not a general-purpose URL type. It is mbgl's `util::URL`, transcribed for one
/// job: to give `{domain}`, `{path}` and the rest the exact spans mbgl's templates expect. A
/// stricter parser would reject `mapbox://////`, which mbgl carries through unchanged and whose
/// behaviour is asserted in its own tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Url {
    /// `scheme` without the colon.
    pub scheme: (usize, usize),
    /// Host, between the scheme and the first `/`.
    pub domain: (usize, usize),
    /// Everything from the domain to the query.
    pub path: (usize, usize),
    /// The query *including* its leading `?`, or an empty span at the fragment or the end.
    pub query: (usize, usize),
}

impl Url {
    /// Parses a URL into spans.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let bytes = text.as_bytes();
        let is_scheme_char =
            |c: u8| c.is_ascii_alphanumeric() || c == b'-' || c == b'+' || c == b'.';

        // A `#` before the `?` means there is no query at all: the fragment swallowed it.
        let hash = text.find('#');
        let question = text.find('?');
        let query = match (question, hash) {
            (Some(q), None) => (q, text.len() - q),
            (Some(q), Some(h)) if q < h => (q, h - q),
            (_, Some(h)) => (h, 0),
            (None, None) => (text.len(), 0),
        };

        let scheme = if bytes.first().is_some_and(u8::is_ascii_alphabetic) {
            let mut end = 0;
            while end < query.0 && bytes.get(end).is_some_and(|c| is_scheme_char(*c)) {
                end += 1;
            }
            // Past the end reads as not-a-colon, which is what a C++ `string[size()]` gives.
            if bytes.get(end) == Some(&b':') {
                (0, end)
            } else {
                (0, 0)
            }
        } else {
            (0, 0)
        };

        let is_data = &text[scheme.0..scheme.0 + scheme.1] == "data";
        let mut domain_start = scheme.0 + scheme.1;
        while domain_start < query.0 && matches!(bytes.get(domain_start), Some(b':') | Some(b'/')) {
            domain_start += 1;
        }
        let separator = if is_data { ',' } else { '/' };
        let domain_end = text[domain_start..]
            .find(separator)
            .map_or(text.len(), |index| domain_start + index)
            .min(query.0);
        let domain = (domain_start, domain_end.saturating_sub(domain_start));

        let mut path_start = domain.0 + domain.1;
        if is_data {
            path_start += 1;
        }
        let path = (path_start, query.0.saturating_sub(path_start));

        Self {
            scheme,
            domain,
            path,
            query,
        }
    }

    /// The slice a span names.
    #[must_use]
    pub fn slice(text: &str, span: (usize, usize)) -> &str {
        let end = (span.0 + span.1).min(text.len());
        text.get(span.0.min(end)..end).unwrap_or("")
    }
}

/// A path split into directory, filename and extension.
///
/// mbgl's `util::Path`, and its one surprise is transcribed with it: a `@2x` immediately before
/// the dot counts as *part of the extension*. `sprite@2x.png` has a filename of `sprite` and an
/// extension of `@2x.png`, which is what lets the sprite template put the scale back on the far
/// side of the rewrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathParts {
    /// Everything up to and including the last `/`.
    pub directory: (usize, usize),
    /// Between the directory and the extension.
    pub filename: (usize, usize),
    /// From the dot — or from `@2x` — to the end.
    pub extension: (usize, usize),
}

impl PathParts {
    /// Splits the span `(pos, count)` of `text`.
    #[must_use]
    pub fn parse(text: &str, pos: usize, count: usize) -> Self {
        let end = (pos + count).min(text.len());
        let head = &text[..end];

        let directory = match head.rfind('/') {
            Some(slash) if slash >= pos => (pos, slash + 1 - pos),
            _ => (pos, 0),
        };

        let mut dot = head.rfind('.');
        if let Some(index) = dot
            && index >= 3
            && head.get(index - 3..index) == Some("@2x")
        {
            dot = Some(index - 3);
        }
        let after_directory = directory.0 + directory.1;
        let extension = match dot {
            Some(index) if index >= after_directory => (index, end - index),
            _ => (end, 0),
        };

        let filename = (after_directory, extension.0.saturating_sub(after_directory));

        Self {
            directory,
            filename,
            extension,
        }
    }
}

/// Fills a template's tokens from a parsed URL.
///
/// mbgl's `util::transformURL`. The query handling at the end is the part that is easy to get
/// wrong: the original URL's query is carried across, and its `?` becomes an `&` when the
/// template already contributed one. Dropping it instead loses a signed URL's signature; keeping
/// the `?` produces two of them, which every server reads as a literal in the first parameter.
#[must_use]
fn transform(template: &str, text: &str, url: &Url) -> String {
    let mut result = replace_tokens(template, |token| {
        let span = match token {
            "path" => url.path,
            "domain" => url.domain,
            "scheme" => url.scheme,
            "directory" => PathParts::parse(text, url.path.0, url.path.1).directory,
            "filename" => PathParts::parse(text, url.path.0, url.path.1).filename,
            "extension" => PathParts::parse(text, url.path.0, url.path.1).extension,
            _ => return None,
        };
        Some(Url::slice(text, span).to_string())
    });

    // A span of one is a bare `?`, which carries nothing and is dropped.
    if url.query.1 > 1 {
        let amp = result.contains('?').then_some(result.len());
        result.push_str(Url::slice(text, url.query));
        if let Some(index) = amp
            && index < result.len()
        {
            result.replace_range(index..index + 1, "&");
        }
    }
    result
}

/// The `?key=…` a request carries, or empty.
fn query_string(server: &TileServer, api_key: &str) -> String {
    if !server.requires_api_key || server.api_key_parameter.is_empty() || api_key.is_empty() {
        return String::new();
    }
    alloc_format(server.api_key_parameter, api_key)
}

fn alloc_format(parameter: &str, api_key: &str) -> String {
    let mut out = String::with_capacity(parameter.len() + api_key.len() + 2);
    out.push('?');
    out.push_str(parameter);
    out.push('=');
    out.push_str(api_key);
    out
}

/// Which resource a URL is being normalized as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A TileJSON or inline source document.
    Source,
    /// A style document.
    Style,
    /// A sprite index or sheet.
    Sprite,
    /// A glyph range.
    Glyphs,
    /// A tile.
    Tile,
}

impl Kind {
    const fn rule(self, server: &TileServer) -> Rule {
        match self {
            Self::Source => server.source,
            Self::Style => server.style,
            Self::Sprite => server.sprites,
            Self::Glyphs => server.glyphs,
            Self::Tile => server.tile,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Style => "style",
            Self::Sprite => "sprite",
            Self::Glyphs => "glyphs",
            Self::Tile => "tile",
        }
    }
}

/// Rewrites a canonical URL into the one that fetches it.
///
/// A URL this server does not claim is returned borrowed and unchanged, which is the common
/// case: a style may mix `mapbox://` sources with plain HTTPS ones, and the plain ones must
/// reach the transport exactly as written.
///
/// # Errors
///
/// [`CanonicalError::MissingApiKey`] when the server requires a key and none was given, and
/// [`CanonicalError::WrongDomain`] when the URL names a different resource kind than the one
/// asked for.
pub fn normalize<'a>(
    server: &TileServer,
    kind: Kind,
    url: &'a str,
    api_key: &str,
) -> Result<Cow<'a, str>, CanonicalError> {
    if !server.claims(url) {
        return Ok(Cow::Borrowed(url));
    }

    // Only a *source* refuses outright for a missing key. mbgl throws there and logs elsewhere,
    // and the asymmetry is deliberate on its part: a source with no key produces a TileJSON
    // fetch that fails and takes every tile with it, while a sprite with no key is one missing
    // picture on an otherwise working map.
    if kind == Kind::Source && server.requires_api_key && api_key.is_empty() {
        return Err(CanonicalError::MissingApiKey {
            url: url.to_string(),
            parameter: server.api_key_parameter,
        });
    }

    let rule = kind.rule(server);
    let parsed = Url::parse(url);
    if !rule.domain.is_empty() && Url::slice(url, parsed.domain) != rule.domain {
        return Err(CanonicalError::WrongDomain {
            url: url.to_string(),
            kind: kind.name(),
        });
    }

    let mut template = String::from(server.base_url);
    template.push_str(rule.version_prefix.unwrap_or(""));
    template.push_str(rule.template);
    template.push_str(&query_string(server, api_key));
    if kind == Kind::Source {
        template.push_str(server.source_suffix);
    }

    Ok(Cow::Owned(transform(&template, url, &parsed)))
}

impl Kind {
    /// Which resource kind a canonical URL names, from its domain segment.
    ///
    /// `mapbox://sprites/…` is a sprite and `mapbox://fonts/…` is a glyph range: the segment
    /// after the scheme *is* the kind, which is what lets a transport wrapper rewrite a URL
    /// without being told what it is for. Anything the server does not recognise is a source,
    /// because a source URL has no kind segment at all — `mapbox://user.map` is the tileset
    /// `user.map` and nothing else.
    ///
    /// `None` for a URL this server does not claim.
    #[must_use]
    pub fn of(server: &TileServer, url: &str) -> Option<Self> {
        if !server.claims(url) {
            return None;
        }
        let domain = Url::slice(url, Url::parse(url).domain);
        for kind in [Self::Style, Self::Sprite, Self::Glyphs, Self::Tile] {
            let rule = kind.rule(server);
            if !rule.domain.is_empty() && rule.domain == domain {
                return Some(kind);
            }
        }
        Some(Self::Source)
    }
}

/// Wraps a file source so canonical URLs are rewritten on their way to the transport.
///
/// # Why this belongs at the bottom of the stack
///
/// Everything above it — the in-flight coalescing table of §5.1, the byte cache of §12.6 —
/// keys on the URL. Rewriting *below* them means those layers see the canonical
/// `mapbox://tiles/a.b/0/0/0.pbf` and only the transport ever sees the address with the API key
/// in it. Two consequences follow, and both are the point:
///
/// A cached tile survives a change of API key. The key is a credential with a lifetime of its
/// own — rotated, refreshed, scoped per user — and a cache keyed on the normalized URL would
/// treat every rotation as a cold start for the whole region. mbgl solves the same problem from
/// the other end, canonicalizing a normalized URL back before storing it.
///
/// And two views sharing a tile share it whatever key each was configured with, because they
/// agree on the canonical form before either reaches a socket.
///
/// # The kind is inferred, and that is not a weakening
///
/// A transport is handed a URL and no context, so the resource kind comes from the URL's own
/// domain segment — which is exactly where it is written. What is given up is the cross-check a
/// caller who *knows* the kind can make, and [`normalize`] is still there for those callers.
#[derive(Debug)]
pub struct Canonical<S> {
    inner: S,
    server: TileServer,
    api_key: String,
}

impl<S> Canonical<S> {
    /// Wraps `inner`, rewriting URLs this server claims.
    pub fn new(inner: S, server: TileServer, api_key: impl Into<String>) -> Self {
        Self {
            inner,
            server,
            api_key: api_key.into(),
        }
    }

    /// The wrapped source.
    pub const fn inner(&self) -> &S {
        &self.inner
    }

    /// The address `url` is actually fetched from.
    ///
    /// # Errors
    ///
    /// [`CanonicalError`] as [`normalize`] reports it.
    pub fn resolve<'a>(&self, url: &'a str) -> Result<Cow<'a, str>, CanonicalError> {
        match Kind::of(&self.server, url) {
            Some(kind) => normalize(&self.server, kind, url, &self.api_key),
            None => Ok(Cow::Borrowed(url)),
        }
    }
}

impl<S: crate::source::FileSource> crate::source::FileSource for Canonical<S> {
    fn fetch(&self, url: &str) -> Result<crate::source::Response, crate::source::FetchError> {
        // A URL that cannot be rewritten is a transport failure naming the reason, rather than a
        // fetch of an address no transport claims. The distinction reaches the caller as the
        // difference between "this style asks for something that does not exist" and "the
        // network is broken".
        let resolved = self
            .resolve(url)
            .map_err(|error| crate::source::FetchError::Transport {
                url: url.to_string(),
                message: error.to_string(),
            })?;
        self.inner.fetch(&resolved)
    }

    fn fetch_conditional(
        &self,
        url: &str,
        etag: Option<&str>,
    ) -> Result<crate::source::Response, crate::source::FetchError> {
        let resolved = self
            .resolve(url)
            .map_err(|error| crate::source::FetchError::Transport {
                url: url.to_string(),
                message: error.to_string(),
            })?;
        self.inner.fetch_conditional(&resolved, etag)
    }
}
