//! Serving a `.pmtiles` archive through the same [`FileSource`] everything else fetches from.
//!
//! The archive reader next door answers "give me this tile". This turns it into a source a
//! style can name, so `"url": "pmtiles:///data/planet.pmtiles"` resolves and covers exactly the
//! way an XYZ origin does, and nothing above [`crate::tileset::resolve`] learns a second shape.
//!
//! # The manifest is synthesised, because the archive already knows
//!
//! A style asking for a source by `url` expects TileJSON back: the zoom range, the templates,
//! the bounds. An archive carries all of that in its header and its metadata document, so this
//! answers the manifest request itself rather than requiring a `.json` alongside the file.
//! mbgl's `PMTilesFileSource::request_tilejson` does the same, and the merge order here is
//! transcribed from it: the metadata document wins where it says something, the header fills in
//! where it does not, and `scheme` is forced to `xyz` because a v3 archive has no other.
//!
//! # Where the tile coordinate lives, and why it is not where mbgl puts it
//!
//! mbgl's tile template is bare — `pmtiles://<archive>` with no tokens — because its `Resource`
//! carries the canonical tile id alongside the URL, so the coordinate rides in a second field.
//! [`FileSource::fetch`] takes a URL and nothing else, deliberately: §5.1's coalescing and the
//! §12.6 cache both key on that string, and a request whose identity is only partly in its key
//! is one that dedupes two different tiles into one. So the template synthesised here ends
//! `/{z}/{x}/{y}` and the coordinate is parsed back out. The bytes returned are identical; what
//! differs is that the URL is a whole key.
//!
//! # An archive is already a cache
//!
//! Nothing here reports an etag or a `max-age`, and this is not meant to be wrapped in the
//! `cache` feature's `CachingFileSource`. Copying tiles out of a local archive into a local
//! SQLite cache spends disk to make reads slower.

use std::collections::HashMap;
use std::fs::File;
use std::sync::{Arc, Mutex};

use crate::pmtiles::{Archive, PmtilesError, TileType};
use crate::source::{FetchError, FileSource, Response};

/// The scheme an archive is named by.
pub const PROTOCOL: &str = "pmtiles://";

/// Whether this source is the one that should answer for `url`.
///
/// mbgl's `PMTilesFileSource::canRequest`, and the predicate a router dispatches on.
#[must_use]
pub fn accepts(url: &str) -> bool {
    url.starts_with(PROTOCOL)
}

/// How many archives stay open at once.
///
/// A style names one or two. The bound exists so that a long-lived process handed a stream of
/// distinct paths cannot accumulate file descriptors without limit; past it, archives are
/// opened per request, which is slower and never wrong.
const MAX_OPEN: usize = 16;

/// What a `pmtiles://` URL is asking for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Request<'a> {
    /// The archive's TileJSON.
    Manifest(&'a str),
    /// One tile.
    Tile(&'a str, u8, u32, u32),
}

/// Splits a trailing `/{z}/{x}/{y}`, with an optional extension on the last component.
///
/// The one ambiguity is an archive whose path itself ends in three numeric components, which
/// would read as a tile request. Every `{z}/{x}/{y}` template has had that ambiguity since the
/// scheme was invented, and it resolves the same way here as everywhere: a path that addresses
/// tiles is a tile request.
fn split_tile(inner: &str) -> Option<(&str, u8, u32, u32)> {
    let (rest, y) = inner.rsplit_once('/')?;
    let (rest, x) = rest.rsplit_once('/')?;
    let (path, z) = rest.rsplit_once('/')?;
    if path.is_empty() {
        return None;
    }
    let y = y.split_once('.').map_or(y, |(stem, _)| stem);
    Some((path, z.parse().ok()?, x.parse().ok()?, y.parse().ok()?))
}

/// Parses a `pmtiles://` URL into the archive path and what is wanted from it.
fn parse(url: &str) -> Result<Request<'_>, FetchError> {
    let refuse = |message: &str| FetchError::Transport {
        url: url.to_owned(),
        message: message.to_owned(),
    };
    let inner = url
        .strip_prefix(PROTOCOL)
        .ok_or_else(|| refuse("not a `pmtiles://` url"))?;
    // mbgl writes the inner url with a scheme of its own — `pmtiles://file:///path` — and a
    // bare path is the ordinary way to write it by hand. Both mean the same file.
    let inner = inner.strip_prefix("file://").unwrap_or(inner);
    if inner.starts_with("http://") || inner.starts_with("https://") {
        // Honest refusal rather than a confusing open failure. Reading a remote archive is a
        // `RangeReader` over §12.6's range requests, which is the same shape as the file reader
        // and is not built yet; treating the url as a path would fail with `no such file`.
        return Err(refuse(
            "a remote archive needs range requests, which are not built yet",
        ));
    }
    if inner.is_empty() {
        return Err(refuse("no archive path"));
    }
    Ok(match split_tile(inner) {
        Some((path, z, x, y)) => Request::Tile(path, z, x, y),
        None => Request::Manifest(inner),
    })
}

/// Reads tiles and manifests out of `.pmtiles` archives on local storage.
#[derive(Debug, Default)]
pub struct PmtilesFileSource {
    /// Archives held open by path, so that the header and root directory are read once rather
    /// than per tile.
    open: Mutex<HashMap<String, Arc<Archive<File>>>>,
}

impl PmtilesFileSource {
    /// A source with nothing open yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The archive at `path`, opening it if it is not already held.
    fn archive(&self, path: &str) -> Result<Arc<Archive<File>>, PmtilesError> {
        // Two locks rather than one held across the open: a slow open — a cold page cache, a
        // network filesystem — would otherwise block every other archive's tiles behind it.
        // The cost is that two threads racing on the same new path both open it and one's work
        // is discarded, which is a wasted read and not a wrong answer.
        if let Some(held) = self.open.lock().expect("archive table").get(path) {
            return Ok(Arc::clone(held));
        }
        let file = File::open(path).map_err(|error| PmtilesError::Io(error.to_string()))?;
        let archive = Arc::new(Archive::open(file)?);
        let mut table = self.open.lock().expect("archive table");
        if let Some(held) = table.get(path) {
            return Ok(Arc::clone(held));
        }
        if table.len() < MAX_OPEN {
            table.insert(path.to_owned(), Arc::clone(&archive));
        }
        Ok(archive)
    }

    /// Builds the TileJSON for an archive: its metadata document, with the header filling in
    /// whatever the document left out.
    fn manifest(&self, url: &str, path: &str) -> Result<Vec<u8>, PmtilesError> {
        let archive = self.archive(path)?;
        let header = *archive.header();
        let raw = archive.metadata()?;

        let mut doc = match serde_json::from_slice::<serde_json::Value>(&raw) {
            Ok(serde_json::Value::Object(map)) => map,
            // A metadata document that is absent, empty, or not an object is not a fault: the
            // header alone addresses every tile in the archive. mbgl does the same.
            _ => serde_json::Map::new(),
        };

        doc.insert("tilejson".to_owned(), "3.0.0".into());
        doc.insert("scheme".to_owned(), "xyz".into());
        if !doc.get("tiles").is_some_and(serde_json::Value::is_array) {
            doc.insert(
                "tiles".to_owned(),
                serde_json::Value::Array(vec![format!("{url}/{{z}}/{{x}}/{{y}}").into()]),
            );
        }
        if header.tile_type == TileType::Mlt {
            doc.insert("encoding".to_owned(), "mlt".into());
        }
        if !doc.get("bounds").is_some_and(serde_json::Value::is_array) {
            doc.insert("bounds".to_owned(), header.bounds.as_slice().into());
        }
        if !doc.get("center").is_some_and(serde_json::Value::is_array) {
            let center = vec![
                header.center[0].into(),
                header.center[1].into(),
                header.center_zoom.into(),
            ];
            doc.insert("center".to_owned(), serde_json::Value::Array(center));
        }
        // A zoom written as a string is common enough in the wild that mbgl parses one, and a
        // TileJSON reader that does not will fall back to the spec's 0..22 and fetch a hundred
        // tiles the archive does not hold.
        for (key, fallback) in [("minzoom", header.min_zoom), ("maxzoom", header.max_zoom)] {
            let stated = doc.get(key).and_then(|value| match value {
                serde_json::Value::Number(_) => Some(value.clone()),
                serde_json::Value::String(text) => text.parse::<u8>().ok().map(Into::into),
                _ => None,
            });
            doc.insert(key.to_owned(), stated.unwrap_or_else(|| fallback.into()));
        }

        Ok(serde_json::to_vec(&serde_json::Value::Object(doc))
            .expect("a map of json values serializes"))
    }
}

impl FileSource for PmtilesFileSource {
    fn fetch(&self, url: &str) -> Result<Response, FetchError> {
        let failed = |error: PmtilesError| FetchError::Transport {
            url: url.to_owned(),
            message: error.to_string(),
        };
        match parse(url)? {
            Request::Manifest(path) => Ok(Response {
                status: 200,
                body: self.manifest(url, path).map_err(failed)?,
                ..Response::default()
            }),
            Request::Tile(path, z, x, y) => {
                let archive = self.archive(path).map_err(failed)?;
                // Absent is a 404 and not an error, for the reason `Response::is_absent` gives:
                // a tileset's coverage is not a rectangle, and asking past its edge is how the
                // edge is found. An archive answers that question locally instead of over a
                // round trip, which is most of the point of using one.
                Ok(match archive.tile(z, x, y).map_err(failed)? {
                    Some(body) => Response {
                        status: 200,
                        body,
                        ..Response::default()
                    },
                    None => Response {
                        status: 404,
                        ..Response::default()
                    },
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_its_own_scheme() {
        assert!(accepts("pmtiles:///data/x.pmtiles"));
        assert!(!accepts("https://example.com/x.pmtiles"));
        assert!(!accepts("/data/x.pmtiles"));
    }

    #[test]
    fn reads_a_manifest_request() {
        assert_eq!(
            parse("pmtiles:///data/x.pmtiles"),
            Ok(Request::Manifest("/data/x.pmtiles"))
        );
        // mbgl writes the inner url with its own scheme; both name the same file.
        assert_eq!(
            parse("pmtiles://file:///data/x.pmtiles"),
            Ok(Request::Manifest("/data/x.pmtiles"))
        );
    }

    #[test]
    fn reads_a_tile_request() {
        assert_eq!(
            parse("pmtiles:///data/x.pmtiles/5/17/10"),
            Ok(Request::Tile("/data/x.pmtiles", 5, 17, 10))
        );
        // An extension on the last component is how most templates are written.
        assert_eq!(
            parse("pmtiles:///data/x.pmtiles/5/17/10.mvt"),
            Ok(Request::Tile("/data/x.pmtiles", 5, 17, 10))
        );
    }

    /// A path with fewer than three trailing numbers is the archive itself, not a tile of it.
    #[test]
    fn a_partial_coordinate_is_not_a_tile() {
        assert_eq!(
            parse("pmtiles:///data/x.pmtiles/5/17"),
            Ok(Request::Manifest("/data/x.pmtiles/5/17"))
        );
        assert_eq!(
            parse("pmtiles:///data/2020/07/01/x.pmtiles"),
            Ok(Request::Manifest("/data/2020/07/01/x.pmtiles"))
        );
    }

    /// A remote archive is a range reader that is not built. Saying so beats failing later with
    /// `no such file or directory: https:`.
    #[test]
    fn refuses_a_remote_archive_clearly() {
        let error = parse("pmtiles://https://example.com/x.pmtiles").expect_err("refused");
        assert!(format!("{error}").contains("range requests"), "{error}");
    }
}
