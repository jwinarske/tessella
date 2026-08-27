//! A local vector tile server.
//!
//! # Why this exists rather than a mock
//!
//! Everything above the transport can be tested against an in-memory source, and is. What that
//! cannot test is the transport: a URL that is templated wrong, a body read short, a status
//! mapped to the wrong outcome, a socket held open past a timeout. Those only appear against a
//! real server speaking real HTTP over a real socket, and they are exactly the failures that
//! otherwise surface first against somebody's live map.
//!
//! So this is a server, not a fixture. It is small enough to read in one sitting, has no
//! dependencies, and binds an ephemeral port so tests can run in parallel and CI needs no
//! network. The same binary serves a browser or a real MapLibre client, which is what makes it
//! useful for looking at output by hand rather than only in assertions.
//!
//! # What it is not
//!
//! Not a production server. It speaks HTTP/1.1 with `Connection: close`, one request per
//! connection, no keep-alive, no compression, no range requests, no conditional GETs. Those
//! belong to §12.6 and to a real origin; adding a half-implementation of them here would make
//! this a thing to trust rather than a thing to test against.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// What the server serves, by path.
#[derive(Debug, Default)]
pub struct Routes {
    exact: HashMap<String, (u16, &'static str, Vec<u8>)>,
    /// Body served for any path matching the tile pattern, when set, and its content encoding.
    tiles: Option<(Vec<u8>, Option<&'static str>)>,
    /// Zooms the tile route answers for. Anything else is a 404.
    tile_zooms: Option<(u8, u8)>,
    /// A `Cache-Control` to send with every response, when set.
    cache_control: Option<&'static str>,
}

impl Routes {
    /// An empty routing table: everything is a 404.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sends this `Cache-Control` with every response.
    #[must_use]
    pub fn cache_control(mut self, value: &'static str) -> Self {
        self.cache_control = Some(value);
        self
    }

    /// Serves `body` at exactly `path`.
    #[must_use]
    pub fn at(mut self, path: &str, content_type: &'static str, body: Vec<u8>) -> Self {
        self.exact
            .insert(path.to_string(), (200, content_type, body));
        self
    }

    /// Serves `status` with an empty body at exactly `path`.
    ///
    /// For the statuses that are neither the thing nor a definite absence -- a 500, a 403 --
    /// which a client has to treat differently from a 404.
    #[must_use]
    pub fn at_status(mut self, path: &str, status: u16) -> Self {
        self.exact
            .insert(path.to_string(), (status, "text/plain", Vec::new()));
        self
    }

    /// Serves `body` for any `/{z}/{x}/{y}.pbf`, optionally only within a zoom range.
    ///
    /// One body for every tile is deliberate: what is under test is the plumbing, and a
    /// distinct body per tile would only make the assertions about the fixture instead.
    #[must_use]
    pub fn tiles(self, body: Vec<u8>, zooms: Option<(u8, u8)>) -> Self {
        self.tiles_encoded(body, None, zooms)
    }

    /// As [`Self::tiles`], declaring a `Content-Encoding` for the body.
    ///
    /// Real vector tile origins serve gzip — `pmtiles serve` and every hosted basemap do —
    /// and the body then is not a tile until something has inflated it. Serving it here means
    /// the decompression path is exercised without an external service to depend on.
    #[must_use]
    pub fn tiles_encoded(
        mut self,
        body: Vec<u8>,
        encoding: Option<&'static str>,
        zooms: Option<(u8, u8)>,
    ) -> Self {
        self.tiles = Some((body, encoding));
        self.tile_zooms = zooms;
        self
    }

    fn resolve(&self, path: &str) -> Served {
        if let Some((status, kind, body)) = self.exact.get(path) {
            return Served {
                status: *status,
                content_type: kind,
                encoding: None,
                body: body.clone(),
            };
        }
        if let Some((body, encoding)) = &self.tiles
            && let Some(z) = tile_zoom_of(path)
            && self
                .tile_zooms
                .is_none_or(|(min, max)| (min..=max).contains(&z))
        {
            return Served {
                status: 200,
                content_type: "application/x-protobuf",
                encoding: *encoding,
                body: body.clone(),
            };
        }
        Served {
            status: 404,
            content_type: "text/plain",
            encoding: None,
            body: b"not found".to_vec(),
        }
    }
}

/// One response.
struct Served {
    status: u16,
    content_type: &'static str,
    encoding: Option<&'static str>,
    body: Vec<u8>,
}

/// The zoom of a `/{z}/{x}/{y}.pbf` path, or `None` if it is not one.
fn tile_zoom_of(path: &str) -> Option<u8> {
    let trimmed = path
        .strip_suffix(".pbf")
        .or_else(|| path.strip_suffix(".mvt"))?;
    let mut parts = trimmed.trim_start_matches('/').split('/');
    let z = parts.next()?.parse().ok()?;
    let _x: u32 = parts.next()?.parse().ok()?;
    let _y: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(z)
}

/// A running server. Dropping it shuts the server down and joins its thread.
#[derive(Debug)]
pub struct Server {
    addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    requests: Arc<AtomicU64>,
    paths: Arc<Mutex<Vec<String>>>,
    routes: Arc<Mutex<Arc<Routes>>>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    /// Binds an ephemeral port on loopback and starts serving.
    ///
    /// # Errors
    ///
    /// [`std::io::Error`] when the port cannot be bound.
    pub fn start(routes: Routes) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        // A short poll timeout, so shutdown does not wait for one more connection to arrive.
        listener.set_nonblocking(true)?;

        let shutdown = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(AtomicU64::new(0));
        let paths = Arc::new(Mutex::new(Vec::new()));
        let routes = Arc::new(Mutex::new(Arc::new(routes)));

        let thread = {
            let shutdown = Arc::clone(&shutdown);
            let requests = Arc::clone(&requests);
            let paths = Arc::clone(&paths);
            let routes = Arc::clone(&routes);
            std::thread::spawn(move || {
                while !shutdown.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            requests.fetch_add(1, Ordering::Relaxed);
                            // One thread per connection, rather than serving inline.
                            //
                            // Serving on the accept loop makes the server a *serializer*: the
                            // next connection is not accepted until this one has been written
                            // in full. Any measurement of client-side parallelism over this
                            // server then measures the server. It was found exactly that way —
                            // a worker-count benchmark on the target reported that a second
                            // worker helped and a third did not, which was this loop and not
                            // the decoder.
                            //
                            // The route table is an `Arc` taken under the lock and released
                            // before the write, for the same reason: a lock spanning the
                            // response would put the serialization back one layer down. An
                            // `Arc` and not a copy because a route table holds tile bodies, and
                            // copying a hundred kilobytes per request would distort the
                            // measurement in the other direction.
                            let held = Arc::clone(
                                &routes
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                            );
                            let paths = Arc::clone(&paths);
                            std::thread::spawn(move || serve(stream, &held, &paths));
                        }
                        Err(error) => {
                            if accept_error_is_fatal(error.kind()) {
                                break;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                    }
                }
            })
        };

        Ok(Self {
            addr,
            shutdown,
            requests,
            paths,
            routes,
            thread: Some(thread),
        })
    }

    /// Replaces what the server answers with, from now on.
    ///
    /// Origins change: a tile is re-cut, a sprite is redrawn, a road is removed. Testing a
    /// refresh against a server that cannot change would only test that nothing changed.
    pub fn set_routes(&self, routes: Routes) {
        *self
            .routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(routes);
    }

    /// The origin to point a style at, as `http://127.0.0.1:<port>`.
    #[must_use]
    pub fn origin(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Connections accepted, which is one per request since there is no keep-alive.
    #[must_use]
    pub fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    /// Every path served, in the order they arrived.
    #[must_use]
    pub fn paths(&self) -> Vec<String> {
        self.paths
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// Whether an `accept` error should end the serving loop. Nothing does.
///
/// # Why the answer is always no
///
/// It used to be "anything but `WouldBlock`", and that turns a *transient* failure into a server
/// that is bound, alive and permanently deaf. The test it is serving then fails somewhere
/// unrelated, with a fetch error, while the same test passes alone — because alone there is no
/// transient failure to have.
///
/// `accept` fails for reasons that are nothing to do with this end and clear on their own:
///
/// - `ConnectionAborted` — a client hung up between the SYN and the accept.
/// - `Interrupted` — a signal arrived mid-call.
/// - `WouldBlock` — nothing is waiting, which is the ordinary case for a non-blocking listener.
/// - Out of descriptors, which has no `ErrorKind` of its own on stable Rust and arrives as
///   `Uncategorized`. A workspace test run with a binary per crate and sockets in several of
///   them reaches the process or system limit and leaves it as soon as one of them finishes.
///
/// The last is why this cannot enumerate the kinds it forgives: the one that matters most is the
/// one with no name. So the policy is inverted — nothing is fatal — and nothing is lost by it,
/// because the loop is bounded by the shutdown flag that `Drop` sets. A listener that is
/// genuinely dead costs a one-millisecond poll until the server goes out of scope.
#[must_use]
pub const fn accept_error_is_fatal(_kind: std::io::ErrorKind) -> bool {
    false
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Reads one request and writes one response, recording the path *before* answering.
///
/// The ordering is the point. Recording after the response is written is a race a client can
/// win: it reads the body, asks what was served, and is told nothing was — because the server
/// thread has not pushed yet. That made two tests fail about half the time for a reason that
/// had nothing to do with what they were testing.
fn serve(stream: TcpStream, routes: &Routes, paths: &Mutex<Vec<String>>) -> Option<()> {
    stream.set_nonblocking(false).ok()?;
    let mut reader = BufReader::new(stream);

    let mut request_line = String::new();
    reader.read_line(&mut request_line).ok()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();

    // Read the headers, keeping the one that decides whether a body is needed. The request
    // body is not read: this serves GET only, and a client that sent one would be doing
    // something this is not here to support.
    let mut if_none_match: Option<String> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("if-none-match")
        {
            if_none_match = Some(value.trim().to_string());
        }
    }

    // The query string is not part of the route: a tile URL may carry an API key.
    let path = target.split('?').next().unwrap_or(&target).to_string();
    paths
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(path.clone());

    let served = if method == "GET" {
        routes.resolve(&path)
    } else {
        Served {
            status: 405,
            content_type: "text/plain",
            encoding: None,
            body: b"method not allowed".to_vec(),
        }
    };
    let Served {
        status,
        content_type,
        encoding,
        body,
    } = served;

    let reason = match status {
        200 => "OK",
        304 => "Not Modified",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Unknown",
    };
    // A conditional request whose tag matches is answered with a status and no body, which is
    // the whole point of revalidation. Without this the cache's 304 path is untestable, and a
    // path that cannot be tested is one that will be wrong.
    let etag = format!("\"{status}-{}\"", body.len());
    let (status, reason, body) = if status == 200 && if_none_match.as_deref() == Some(&etag) {
        (304, "Not Modified", Vec::new())
    } else {
        (status, reason, body)
    };

    let encoding = encoding.map_or_else(String::new, |value| {
        format!("Content-Encoding: {value}\r\n")
    });
    let cache_control = routes
        .cache_control
        .map_or_else(String::new, |value| format!("Cache-Control: {value}\r\n"));
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         {encoding}\
         {cache_control}\
         Content-Length: {}\r\n\
         ETag: {etag}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );

    let mut stream = reader.into_inner();
    stream.write_all(head.as_bytes()).ok()?;
    stream.write_all(&body).ok()?;
    stream.flush().ok()?;
    Some(())
}

/// Reads a file, for a caller assembling routes.
///
/// # Errors
///
/// [`std::io::Error`] when the file cannot be read.
pub fn read(path: &str) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?.read_to_end(&mut bytes)?;
    Ok(bytes)
}
