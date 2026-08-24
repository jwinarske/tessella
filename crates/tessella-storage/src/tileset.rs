//! Resolving a style's source into the templates and zooms that address its tiles.
//!
//! A style says one of two things about a tiled source: here are the tile URLs, or here is a
//! TileJSON that will tell you. This turns either into the same [`TileSet`], so nothing above
//! has to care which was written.
//!
//! # The manifest is a fetch, and it is on the critical path
//!
//! A source given by `url` cannot be covered until its manifest arrives — the zoom range is in
//! there, and so are the templates. That is the serialization §12.5 wants to break: the
//! manifest fetch should be issued the moment the sources parse, before layer compilation
//! finishes, rather than after it. Resolution is kept separate from covering here so that
//! reordering is a scheduling change rather than a rewrite.
//!
//! # Defaults come from the spec, not from the manifest's silence
//!
//! A TileJSON that omits `minzoom` means zero and one that omits `maxzoom` means twenty-two,
//! because that is what the style spec says those fields default to. Treating absence as "no
//! limit" would fetch tiles at zoom 30 from a source that stops at 14, and every one would be a
//! 404 that looks like a server fault.

use crate::source::{FetchError, FileSource};
use crate::url::{Scheme, ZoomRange};

/// Everything needed to address a source's tiles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TileSet {
    /// Tile URL templates. More than one is a source sharded across hosts.
    pub templates: Vec<String>,
    /// The zooms the source provides.
    pub zooms: ZoomRange,
    /// Which way rows are numbered.
    pub scheme: Scheme,
}

/// Why a source could not be resolved.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    /// The source names neither templates nor a manifest.
    #[error("source has neither `tiles` nor `url`")]
    Unaddressable,
    /// The manifest could not be fetched.
    #[error("fetching the manifest: {0}")]
    Fetch(#[from] FetchError),
    /// The manifest came back with a status that is not a manifest.
    #[error("the manifest at `{url}` returned {status}")]
    Status {
        /// What was asked for.
        url: String,
        /// What came back.
        status: u16,
    },
    /// The manifest is not TileJSON.
    #[error("parsing the manifest at `{url}`: {message}")]
    Malformed {
        /// What was asked for.
        url: String,
        /// What went wrong.
        message: String,
    },
}

/// Reads the zoom range and scheme a source states, falling back to the spec's defaults.
fn describe(source: &tessella_style::TileSource) -> (ZoomRange, Scheme) {
    let default = ZoomRange::default();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let zoom = |value: Option<f64>, fallback: u8| {
        value.map_or(fallback, |zoom| zoom.clamp(0.0, 30.0) as u8)
    };
    let scheme = Scheme::parse(
        source
            .extra
            .get("scheme")
            .and_then(tessella_style::Value::as_str),
    );
    (
        ZoomRange {
            min: zoom(source.minzoom, default.min),
            max: zoom(source.maxzoom, default.max),
        },
        scheme,
    )
}

/// Resolves a tiled source, fetching its manifest if it has one.
///
/// Inline `tiles` win over `url` when a source somehow has both, which is what the spec says
/// and what avoids a needless fetch.
///
/// # Errors
///
/// [`ResolveError`] when the source is unaddressable, or its manifest cannot be fetched or
/// parsed.
pub fn resolve(
    source: &tessella_style::TileSource,
    files: &dyn FileSource,
) -> Result<TileSet, ResolveError> {
    let (zooms, scheme) = describe(source);

    if let Some(templates) = &source.tiles
        && !templates.is_empty()
    {
        return Ok(TileSet {
            templates: templates.clone(),
            zooms,
            scheme,
        });
    }

    let Some(url) = &source.url else {
        return Err(ResolveError::Unaddressable);
    };

    let response = files.fetch(url)?;
    if !response.is_ok() {
        return Err(ResolveError::Status {
            url: url.clone(),
            status: response.status,
        });
    }

    let manifest: tessella_style::TileSource =
        serde_json::from_slice(&response.body).map_err(|error| ResolveError::Malformed {
            url: url.clone(),
            message: error.to_string(),
        })?;

    let (manifest_zooms, manifest_scheme) = describe(&manifest);
    let templates = manifest
        .tiles
        .filter(|tiles| !tiles.is_empty())
        .ok_or_else(|| ResolveError::Malformed {
            url: url.clone(),
            message: "the manifest lists no tiles".into(),
        })?;

    // The style's own zoom range overrides the manifest's where it states one: a style may
    // narrow a source it does not own. Where it is silent the manifest decides, which is the
    // whole reason for fetching one.
    Ok(TileSet {
        templates,
        zooms: ZoomRange {
            min: source.minzoom.map_or(manifest_zooms.min, |_| zooms.min),
            max: source.maxzoom.map_or(manifest_zooms.max, |_| zooms.max),
        },
        scheme: if source.extra.contains_key("scheme") {
            scheme
        } else {
            manifest_scheme
        },
    })
}

impl TileSet {
    /// The template to use for a tile.
    ///
    /// Sharded sources list several; which one a tile goes to must be a function of the tile,
    /// so that a retry lands on the same host and a cache in front of one of them is not
    /// bypassed at random.
    #[must_use]
    pub fn template_for(&self, z: u8, x: u32, y: u32) -> Option<&str> {
        if self.templates.is_empty() {
            return None;
        }
        let index = (u64::from(x) + u64::from(y) + u64::from(z)) % self.templates.len() as u64;
        self.templates
            .get(usize::try_from(index).ok()?)
            .map(String::as_str)
    }

    /// The URL for a tile, or `None` when this source has no template.
    #[must_use]
    pub fn url_for(&self, z: u8, x: u32, y: u32, pixel_ratio: f32) -> Option<String> {
        Some(crate::url::expand(
            self.template_for(z, x, y)?,
            z,
            x,
            y,
            self.scheme,
            pixel_ratio,
        ))
    }
}
