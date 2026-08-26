//! The online file source.
//!
//! # Why this is behind a feature, and TLS behind another
//!
//! `ureq` is pinned to `default-features = false`, which is HTTP without TLS and is pure Rust —
//! no `ring`, no `rustls`, no C. That matters because the cross `cargo check` lane has no C
//! toolchain for the target, and a transitive C dependency would break it for every crate
//! rather than only this one.
//!
//! TLS is the piece that brings C back, so it is its own feature — `tls` — rather than part of
//! `http`. The cross lane checks the workspace with *default* features and neither is in the
//! default set, so a lane that needs no cross C toolchain keeps not needing one. A build that
//! wants HTTPS asks for it, and pays for `ring` on the host, where a C compiler is not in doubt.
//!
//! Without `tls` an `https://` URL fails at the transport rather than silently falling back to
//! plaintext, because a map that quietly stops using TLS is worse than one that says it cannot.
//! That is the same reason it is not a fallback *with* the feature either: a certificate this
//! does not trust is a refusal, not a downgrade.
//!
//! What is still outstanding is §12.6's connection reuse and session resumption. Both are
//! properties of how the agent is pooled rather than of whether TLS is compiled in, and both
//! want measuring against a real origin over a real link.
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

/// Reads `Cache-Control` for the two directives that decide whether a copy may be used.
///
/// Transcribed from mbgl's `CacheControl::parse`: a comma-separated list in which
/// `must-revalidate` and `max-age=N` are recognised and everything else — including quoted
/// values that may themselves contain commas — is skipped.
///
/// `max-age` is returned as stated, relative: resolving it needs a clock, and the only
/// component entitled to one is whatever compares it against the present.
fn freshness(cache_control: Option<&str>) -> (Option<i64>, bool) {
    let Some(value) = cache_control else {
        return (None, false);
    };
    let mut max_age = None;
    let mut must_revalidate = false;
    for directive in value.split(',') {
        let directive = directive.trim();
        if directive.eq_ignore_ascii_case("must-revalidate") {
            must_revalidate = true;
        } else if let Some(seconds) = directive
            .split_once('=')
            .filter(|(name, _)| name.trim().eq_ignore_ascii_case("max-age"))
            .and_then(|(_, seconds)| seconds.trim().parse::<i64>().ok())
        {
            max_age = Some(seconds);
        }
    }
    (max_age, must_revalidate)
}

/// Parses an HTTP-date into seconds since the Unix epoch.
///
/// Only the IMF-fixdate form RFC 9110 requires a server to send —
/// `Sun, 06 Nov 1994 08:49:37 GMT`. The two obsolete formats it says a client must *accept* are
/// not parsed: they return `None`, which reads as "no stated expiry" and so as fresh. That is
/// the safe direction to be wrong in for a header that is itself a fallback for `Cache-Control`.
fn http_date(value: &str) -> Option<i64> {
    let mut parts = value.split_whitespace();
    let _weekday = parts.next()?;
    let day: i64 = parts.next()?.parse().ok()?;
    let month = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = parts.next()?.parse().ok()?;
    let mut clock = parts.next()?.split(':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next()?.parse().ok()?;
    let second: i64 = clock.next()?.parse().ok()?;

    // Days since the epoch by Howard Hinnant's civil-from-days, which is exact for every date
    // in range and needs no table.
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;

    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

impl FileSource for HttpFileSource {
    fn fetch(&self, url: &str) -> Result<Response, FetchError> {
        self.fetch_conditional(url, None)
    }

    fn fetch_conditional(&self, url: &str, etag: Option<&str>) -> Result<Response, FetchError> {
        let transport = |message: String| FetchError::Transport {
            url: url.to_string(),
            message,
        };

        let request = self.agent.get(url);
        // `If-None-Match` is what turns a revalidation into a round trip rather than a
        // download. A 304 comes back through ureq's status-error path below, which is why that
        // path returns a response rather than an error.
        let request = match etag {
            Some(etag) => request.header("If-None-Match", etag),
            None => request,
        };

        let mut response = match request.call() {
            Ok(response) => response,
            // A status ureq treats as an error is still a status this cares about: 404 is the
            // ordinary way a source says it has no tile there.
            Err(ureq::Error::StatusCode(status)) => {
                return Ok(Response {
                    status,
                    ..Response::default()
                });
            }
            Err(error) => return Err(transport(error.to_string())),
        };

        let status = response.status().as_u16();
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        let etag = header("etag");
        let (max_age, must_revalidate) = freshness(header("cache-control").as_deref());
        let expires_at = header("expires").and_then(|value| http_date(&value));

        // Bounded explicitly rather than by `read_to_vec`'s own default: the cap is a property
        // of what this crate is willing to hold, not of the transport it happens to use.
        let body = response
            .body_mut()
            .with_config()
            .limit(crate::source::MAX_RESOURCE_BYTES)
            .read_to_vec()
            .map_err(|error| transport(error.to_string()))?;

        Ok(Response {
            status,
            body,
            etag,
            max_age,
            expires_at,
            must_revalidate,
        })
    }
}
