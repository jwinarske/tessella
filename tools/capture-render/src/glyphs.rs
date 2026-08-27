//! Glyphs off the filesystem, so a symbol layer can be shaped without a network.
//!
//! # Why a file source and not a directory read
//!
//! Because shaping decides which files it wants, and it decides late. A symbol layer's glyph
//! dependencies are a function of the *text*, which is a function of the tile's features, so
//! nothing knows which ranges are needed until `text-field` has been evaluated. `Fonts::fetch`
//! asks for the ones it wants through a [`FileSource`]; handing it a directory listing would
//! mean loading every range on disk to serve the two a tile actually uses.

use std::path::PathBuf;

use tessella_storage::source::{FetchError, FileSource, Response};

/// Serves `{fontstack}/{range}.pbf` out of a directory.
pub(crate) struct Directory {
    root: PathBuf,
}

impl Directory {
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl FileSource for Directory {
    fn fetch(&self, url: &str) -> Result<Response, FetchError> {
        // The URL is whatever template `Fonts` was built with, so only the tail is meaningful.
        // Taking the last two segments keeps the stack and the range and discards the scheme and
        // host a template carries for the network case.
        let mut segments = url.rsplit('/');
        let range = segments.next().unwrap_or_default();
        let stack = segments.next().unwrap_or_default();

        // Percent-decoding matters: a stack is joined with commas and spaced names are encoded,
        // so `Noto%20Sans%20Regular` names a directory that `Noto Sans Regular` does and the
        // literal does not.
        let path = self.root.join(decode(stack)).join(decode(range));

        // A 404 is a response rather than an error, exactly as the trait says: a font with no
        // glyphs in a range is ordinary, and treating it as a transport failure would fail a
        // whole frame over a label that had nothing to draw.
        match std::fs::read(&path) {
            Ok(body) => Ok(Response {
                status: 200,
                body,
                ..Response::default()
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Response {
                status: 404,
                ..Response::default()
            }),
            Err(error) => Err(FetchError::Transport {
                url: url.to_string(),
                message: error.to_string(),
            }),
        }
    }
}

fn decode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = core::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(char::from(byte));
                index += 3;
                continue;
            }
        }
        out.push(char::from(bytes[index]));
        index += 1;
    }
    out
}
