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
    /// Body served for any path matching the tile pattern, when set.
    tiles: Option<Vec<u8>>,
    /// Zooms the tile route answers for. Anything else is a 404.
    tile_zooms: Option<(u8, u8)>,
}

impl Routes {
    /// An empty routing table: everything is a 404.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Serves `body` at exactly `path`.
    #[must_use]
    pub fn at(mut self, path: &str, content_type: &'static str, body: Vec<u8>) -> Self {
        self.exact
            .insert(path.to_string(), (200, content_type, body));
        self
    }

    /// Serves `body` for any `/{z}/{x}/{y}.pbf`, optionally only within a zoom range.
    ///
    /// One body for every tile is deliberate: what is under test is the plumbing, and a
    /// distinct body per tile would only make the assertions about the fixture instead.
    #[must_use]
    pub fn tiles(mut self, body: Vec<u8>, zooms: Option<(u8, u8)>) -> Self {
        self.tiles = Some(body);
        self.tile_zooms = zooms;
        self
    }

    fn resolve(&self, path: &str) -> (u16, &'static str, Vec<u8>) {
        if let Some((status, kind, body)) = self.exact.get(path) {
            return (*status, kind, body.clone());
        }
        if let Some(body) = &self.tiles
            && let Some(z) = tile_zoom_of(path)
            && self
                .tile_zooms
                .is_none_or(|(min, max)| (min..=max).contains(&z))
        {
            return (200, "application/x-protobuf", body.clone());
        }
        (404, "text/plain", b"not found".to_vec())
    }
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

        let thread = {
            let shutdown = Arc::clone(&shutdown);
            let requests = Arc::clone(&requests);
            let paths = Arc::clone(&paths);
            std::thread::spawn(move || {
                while !shutdown.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            requests.fetch_add(1, Ordering::Relaxed);
                            if let Some(path) = serve(stream, &routes) {
                                paths
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .push(path);
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                        Err(_) => break,
                    }
                }
            })
        };

        Ok(Self {
            addr,
            shutdown,
            requests,
            paths,
            thread: Some(thread),
        })
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

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Reads one request and writes one response. Returns the path served.
fn serve(stream: TcpStream, routes: &Routes) -> Option<String> {
    stream.set_nonblocking(false).ok()?;
    let mut reader = BufReader::new(stream);

    let mut request_line = String::new();
    reader.read_line(&mut request_line).ok()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();

    // Drain the headers. The body is not read: this serves GET only, and a client that sent
    // one would be doing something this is not here to support.
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 || line.trim().is_empty() {
            break;
        }
    }

    // The query string is not part of the route: a tile URL may carry an API key.
    let path = target.split('?').next().unwrap_or(&target).to_string();
    let (status, content_type, body) = if method == "GET" {
        routes.resolve(&path)
    } else {
        (405, "text/plain", b"method not allowed".to_vec())
    };

    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Unknown",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         ETag: \"{status}-{}\"\r\n\
         Connection: close\r\n\r\n",
        body.len(),
        body.len()
    );

    let mut stream = reader.into_inner();
    stream.write_all(head.as_bytes()).ok()?;
    stream.write_all(&body).ok()?;
    stream.flush().ok()?;
    Some(path)
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
