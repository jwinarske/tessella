//! Resolving a GeoJSON source to its document.
//!
//! # A GeoJSON source is fetched once, not once per tile
//!
//! That is the whole difference from a vector source, and it follows from where the tiling
//! happens. A vector source is cut into tiles by the server, so a cover of nine tiles is nine
//! requests. A GeoJSON source is one document that the *client* cuts up — geojson-vt's job —
//! so it is one request however many tiles the cover has, and the cover cannot start until it
//! lands. mbgl models this as a source that is not `loaded` until its description arrives.
//!
//! # `data` is overloaded, and the discrimination is by JSON type
//!
//! An object is the document; a string is a URL to fetch it from; anything else is an error.
//! mbgl checks in that order — `isObject` first, then `toString` — and so does this, because a
//! JSON string is not an object and the reverse test would be ambiguous for neither.
//!
//! # Where this deliberately differs from mbgl
//!
//! A document that does not parse makes mbgl log and substitute an *empty* source, with the
//! comment that it is "to make sure we're not infinitely waiting for tiles to load". That is
//! forced by its architecture: tiles are waiting on an observer callback that has to fire.
//! Nothing here is waiting on a callback, so a failure is returned. A caller that wants mbgl's
//! behaviour can log it and carry on with no features; a caller given an empty source cannot
//! recover the fact that the style is broken.

use std::borrow::Cow;

use tessella_style::{GeojsonSource, Value};

use crate::source::FileSource;

/// Where a GeoJSON source's document comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin<'a> {
    /// The document is written into the style.
    Inline,
    /// The document is fetched from here.
    Url(&'a str),
}

/// Why a GeoJSON source could not be resolved.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GeoJsonSourceError {
    /// `data` was neither an object nor a string.
    #[error("GeoJSON data must be a URL or an object")]
    NotUrlOrObject,
    /// The document could not be fetched.
    #[error("fetching `{url}`: {message}")]
    Fetch {
        /// What was asked for.
        url: String,
        /// What went wrong.
        message: String,
    },
    /// The origin answered, but not with a document.
    #[error("the GeoJSON at `{url}` returned {status}")]
    Status {
        /// What was asked for.
        url: String,
        /// What came back.
        status: u16,
    },
    /// The origin answered with nothing.
    ///
    /// Distinct from a status failure and from a parse failure, because it is neither: a tile
    /// that is empty is an ordinary tile, and a *source* that is empty is a style that will
    /// draw nothing and never say why. mbgl calls this "unexpectedly empty GeoJSON" and treats
    /// it as an error for the same reason.
    #[error("the GeoJSON at `{url}` was empty")]
    Empty {
        /// What was asked for.
        url: String,
    },
    /// The bytes were not JSON.
    #[error("parsing the GeoJSON at `{url}`: {message}")]
    Malformed {
        /// What was asked for.
        url: String,
        /// What went wrong.
        message: String,
    },
}

/// Where a source's document lives, without fetching it.
///
/// # Errors
///
/// [`GeoJsonSourceError::NotUrlOrObject`] when `data` is neither.
pub fn origin(source: &GeojsonSource) -> Result<Origin<'_>, GeoJsonSourceError> {
    match &source.data {
        Value::Object(_) => Ok(Origin::Inline),
        Value::String(url) => Ok(Origin::Url(url)),
        _ => Err(GeoJsonSourceError::NotUrlOrObject),
    }
}

/// The source's document, fetched if it is not inline.
///
/// Returns a borrowed value for the inline case and an owned one for the fetched case, so an
/// inline document — which is already in the style — is not copied to be read.
///
/// # Relative URLs are not resolved
///
/// A style parsed from a string has no notion of where it came from, so a `data` of
/// `"features.geojson"` has no base to resolve against and is passed to the file source as
/// written. Absolute URLs work; relative ones need the style's own URL threaded through, which
/// is a change to how styles are loaded rather than to this.
///
/// # Errors
///
/// [`GeoJsonSourceError`] when `data` is neither a URL nor an object, or the fetch, the status,
/// the emptiness or the parse says otherwise.
pub fn resolve<'a>(
    source: &'a GeojsonSource,
    files: &dyn FileSource,
) -> Result<Cow<'a, Value>, GeoJsonSourceError> {
    let url = match origin(source)? {
        Origin::Inline => return Ok(Cow::Borrowed(&source.data)),
        Origin::Url(url) => url,
    };

    let response = files
        .fetch(url)
        .map_err(|error| GeoJsonSourceError::Fetch {
            url: url.to_string(),
            message: error.to_string(),
        })?;

    if !response.is_ok() {
        return Err(GeoJsonSourceError::Status {
            url: url.to_string(),
            status: response.status,
        });
    }
    if response.body.is_empty() {
        return Err(GeoJsonSourceError::Empty {
            url: url.to_string(),
        });
    }

    let document: Value =
        serde_json::from_slice(&response.body).map_err(|error| GeoJsonSourceError::Malformed {
            url: url.to_string(),
            message: error.to_string(),
        })?;
    Ok(Cow::Owned(document))
}
