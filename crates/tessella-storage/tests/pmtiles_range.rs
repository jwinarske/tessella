//! Reading an archive in place, over a transport that does byte ranges.
//!
//! # What this is for
//!
//! A PMTiles archive is read by walking a directory and then reading one tile: a handful of small
//! reads out of something that may be gigabytes. Fetching the archive to read a tile from it is
//! not a slower version of that, it is a different program -- and it is the only version a
//! browser could run at all.
//!
//! The transport here is a fake rather than a socket, because what needs checking is that
//! `Archive` walks a *remote* archive using nothing but ranges, and how the bytes arrive is the
//! transport's business. It counts its reads, so "a handful" is an assertion rather than a claim.

use std::sync::Mutex;

use tessella_storage::pmtiles::{Archive, HttpRange};
use tessella_storage::source::{FetchError, RangeFetch, Response};

/// A real archive, when this machine has one. The format tests elsewhere need none.
pub(crate) fn archive_bytes() -> Option<Vec<u8>> {
    let path = std::path::Path::new("/mnt/dev/maplibre-frontend/tileserver/berlin_z15.pmtiles");
    path.exists().then(|| std::fs::read(path).expect("reads"))
}

/// Serves ranges out of memory, and remembers what it was asked for.
struct Ranged {
    bytes: Vec<u8>,
    reads: Mutex<Vec<(u64, usize)>>,
    /// Answer `200` instead, as an origin that ignores `Range` does.
    ignores_ranges: bool,
}

impl Ranged {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            reads: Mutex::new(Vec::new()),
            ignores_ranges: false,
        }
    }

    fn total(&self) -> usize {
        self.reads
            .lock()
            .expect("not poisoned")
            .iter()
            .map(|(_, n)| n)
            .sum()
    }

    fn count(&self) -> usize {
        self.reads.lock().expect("not poisoned").len()
    }
}

impl RangeFetch for Ranged {
    fn fetch_range(&self, _url: &str, offset: u64, length: usize) -> Result<Response, FetchError> {
        self.reads
            .lock()
            .expect("not poisoned")
            .push((offset, length));
        if self.ignores_ranges {
            return Ok(Response {
                status: 200,
                body: self.bytes[..length.min(self.bytes.len())].to_vec(),
                ..Response::default()
            });
        }
        let start = usize::try_from(offset).expect("in range");
        let Some(slice) = self.bytes.get(start..start + length) else {
            // What an origin says when asked for bytes past the end.
            return Ok(Response {
                status: 416,
                ..Response::default()
            });
        };
        Ok(Response {
            status: 206,
            body: slice.to_vec(),
            ..Response::default()
        })
    }
}

#[test]
fn a_tile_is_read_without_fetching_the_archive() {
    let Some(bytes) = archive_bytes() else {
        eprintln!("no archive on this machine; skipping");
        return;
    };
    let size = bytes.len();
    let source = Ranged::new(bytes);
    let archive = Archive::open(HttpRange::new(
        &source,
        "https://example.invalid/berlin.pmtiles",
    ))
    .expect("the header parses over ranges");

    let tile = archive.tile(14, 8802, 5373).expect("the read succeeds");
    assert!(
        tile.is_some_and(|body| !body.is_empty()),
        "no tile came back"
    );

    // The point, as a number: a ninety-megabyte archive read for one tile.
    assert!(
        source.total() < size / 100,
        "read {} bytes of a {size}-byte archive in {} requests",
        source.total(),
        source.count()
    );
    eprintln!(
        "{} bytes in {} ranges, from a {size}-byte archive",
        source.total(),
        source.count()
    );
}

#[test]
fn an_origin_that_ignores_the_range_is_refused_rather_than_misread() {
    let Some(bytes) = archive_bytes() else {
        eprintln!("no archive on this machine; skipping");
        return;
    };
    let mut source = Ranged::new(bytes);
    source.ignores_ranges = true;
    // The body that arrives is the *head* of the archive, not the bytes at the offset. Slicing it
    // would serve the header as though it were a directory, which parses as something and draws
    // as nothing.
    let opened = Archive::open(HttpRange::new(&source, "https://example.invalid/x.pmtiles"));
    match opened {
        Ok(_) => panic!("an origin that ignored the range was accepted"),
        Err(error) => {
            let said = format!("{error}");
            assert!(
                said.contains("200"),
                "unhelpful about what went wrong: {said}"
            );
        }
    }
}

#[test]
fn a_range_past_the_end_is_a_truncated_archive() {
    let source = Ranged::new(vec![0u8; 16]);
    let reader = HttpRange::new(&source, "https://example.invalid/tiny.pmtiles");
    assert!(Archive::open(reader).is_err(), "a 16-byte archive opened");
}

/// The HTTP path, over a socket.
///
/// The fake above proves `Archive` walks an archive with ranges; this proves the transport
/// actually sends `Range` and reads a `206` back, which is the half a fake cannot check. The
/// server is thirty lines of `TcpListener` rather than a dependency, and it answers exactly the
/// two things this needs: a byte range, and a `200` when told to pretend it does not understand
/// them.
mod over_http {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};

    use tessella_storage::http::HttpFileSource;
    use tessella_storage::pmtiles::{Archive, HttpRange};

    /// Serves one file, honouring `Range`. Answers `port` and runs until the test ends.
    fn serve(bytes: Vec<u8>, honour_ranges: bool) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
        let port = listener.local_addr().expect("has an address").port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                answer(stream, &bytes, honour_ranges);
            }
        });
        port
    }

    fn answer(mut stream: TcpStream, bytes: &[u8], honour_ranges: bool) {
        let mut reader = BufReader::new(stream.try_clone().expect("clones"));
        let mut range = None;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                let (first, last) = value.trim().split_once('-').unwrap_or(("", ""));
                if let (Ok(first), Ok(last)) = (first.parse::<usize>(), last.parse::<usize>()) {
                    range = Some((first, last));
                }
            }
        }

        let (status, body) = match (range, honour_ranges) {
            (Some((first, last)), true) => match bytes.get(first..=last.min(bytes.len() - 1)) {
                Some(slice) if last < bytes.len() => ("206 Partial Content", slice.to_vec()),
                _ => ("416 Range Not Satisfiable", Vec::new()),
            },
            // Told to pretend it does not understand ranges, which real origins do.
            _ => ("200 OK", bytes.to_vec()),
        };
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&body);
        let _ = stream.flush();
        // Drained so the client sees a clean close rather than a reset.
        let mut sink = Vec::new();
        let _ = reader.read_to_end(&mut sink);
    }

    #[test]
    fn a_tile_comes_back_over_a_socket() {
        let Some(bytes) = super::archive_bytes() else {
            eprintln!("no archive on this machine; skipping");
            return;
        };
        let port = serve(bytes, true);
        let source = HttpFileSource::new(std::time::Duration::from_secs(30));
        let reader = HttpRange::new(
            &source,
            format!("http://127.0.0.1:{port}/berlin_z15.pmtiles"),
        );
        let archive = Archive::open(reader).expect("the header parses over http");
        let tile = archive.tile(14, 8802, 5373).expect("the read succeeds");
        assert!(
            tile.is_some_and(|body| !body.is_empty()),
            "no tile came back"
        );
    }

    #[test]
    fn an_origin_ignoring_ranges_is_refused_over_http_too() {
        let Some(bytes) = super::archive_bytes() else {
            eprintln!("no archive on this machine; skipping");
            return;
        };
        let port = serve(bytes, false);
        let source = HttpFileSource::new(std::time::Duration::from_secs(30));
        let reader = HttpRange::new(&source, format!("http://127.0.0.1:{port}/x.pmtiles"));
        assert!(Archive::open(reader).is_err(), "a 200 was read as a range");
    }
}
