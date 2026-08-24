//! The online file source.
//!
//! # Why this is behind a feature
//!
//! `ureq` is pinned to `default-features = false`, which is HTTP without TLS and is pure Rust —
//! no `ring`, no `rustls`, no C. That matters because the cross `cargo check` lane has no C
//! toolchain for the target, and a transitive C dependency would break it for every crate
//! rather than only this one. TLS is the piece that brings C back, so it is deliberately not
//! enabled yet: §12.6 wants connection reuse and session resumption, and that is the change
//! that has to arrive with the cross toolchains (§16) rather than ahead of them.
//!
//! What works today is plain `http://`, which is what a local tile server speaks and what the
//! live test uses. An `https://` URL will fail at the transport rather than silently falling
//! back, because a map that quietly stops using TLS is worse than one that says it cannot.
//!
//! # A 404 is a response
//!
//! A source's coverage is not a rectangle; asking for a tile outside it is how the edge is
//! found. So a 404 comes back as a [`Response`] with that status, not an error — the caller
//! records an empty tile and moves on. Only a transport failure is an error, because only that
//! is worth retrying.

use std::time::Duration;

use crate::source::{FetchError, FileSource, Response};

/// Fetches over HTTP.
#[derive(Debug)]
pub struct HttpFileSource {
    agent: ureq::Agent,
}

impl HttpFileSource {
    /// A source with the given per-request timeout.
    #[must_use]
    pub fn new(timeout: Duration) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            // A tile is not a web page: nothing in this pipeline follows a redirect to a
            // different origin, and a source that redirects is a configuration mistake worth
            // seeing rather than absorbing.
            .max_redirects(0)
            .build();
        Self {
            agent: config.into(),
        }
    }
}

impl Default for HttpFileSource {
    /// Thirty seconds, which is generous for a tile and short enough that a hung origin does
    /// not hold a worker forever.
    fn default() -> Self {
        Self::new(Duration::from_secs(30))
    }
}

impl FileSource for HttpFileSource {
    fn fetch(&self, url: &str) -> Result<Response, FetchError> {
        let transport = |message: String| FetchError::Transport {
            url: url.to_string(),
            message,
        };

        let mut response = match self.agent.get(url).call() {
            Ok(response) => response,
            // A status ureq treats as an error is still a status this cares about: 404 is the
            // ordinary way a source says it has no tile there.
            Err(ureq::Error::StatusCode(status)) => {
                return Ok(Response {
                    status,
                    body: Vec::new(),
                    etag: None,
                });
            }
            Err(error) => return Err(transport(error.to_string())),
        };

        let status = response.status().as_u16();
        let etag = response
            .headers()
            .get("etag")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = response
            .body_mut()
            .read_to_vec()
            .map_err(|error| transport(error.to_string()))?;

        Ok(Response { status, body, etag })
    }
}
