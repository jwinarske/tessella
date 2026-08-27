//! The server survives what a client does to it, and the policy that decides what ends its loop.
//!
//! # Two hazards, and only one of them is reachable from a test
//!
//! A bad *client* is reachable: it can connect and vanish, or send half a request. Those are the
//! first two tests, and they exercise the per-connection thread rather than the accept loop —
//! on Linux the connection is generally completed before `accept` returns, so the failure lands
//! on the read, which is already isolated.
//!
//! A failing *`accept`* is not reachable on purpose. Its causes are the kernel's: a connection
//! aborted between the SYN and the accept, or the process out of descriptors. So what is pinned
//! instead is the *decision* — [`tile_server::accept_error_is_fatal`] — because that is what was
//! wrong. The loop used to end on anything but `WouldBlock`, which left a server bound, alive
//! and permanently deaf, and failed the test it was serving somewhere unrelated.
//!
//! Stating it that way is honest about the limit: these tests would have passed against the old
//! code. The policy test would not.

use std::io::Write as _;
use std::net::TcpStream;

fn server() -> tile_server::Server {
    tile_server::Server::start(tile_server::Routes::new().at(
        "/hello",
        "text/plain",
        b"world".to_vec(),
    ))
    .expect("binds a loopback port")
}

fn get(origin: &str, path: &str) -> Option<Vec<u8>> {
    let address = origin.trim_start_matches("http://");
    let mut stream = TcpStream::connect(address).ok()?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut body = Vec::new();
    std::io::Read::read_to_end(&mut stream, &mut body).ok()?;
    Some(body)
}

/// A client that connects and hangs up without asking for anything does not end the server.
///
/// This is `ECONNABORTED` as closely as it can be produced on purpose: the connection arrives
/// and is gone again. Whether it is aborted before or after the accept is the kernel's timing to
/// decide, which is the whole reason the loop must survive either.
#[test]
fn a_client_that_hangs_up_does_not_deafen_the_server() {
    let server = server();
    let origin = server.origin();

    for _ in 0..50 {
        // Connect and drop, with nothing written.
        drop(TcpStream::connect(origin.trim_start_matches("http://")));
    }

    let body = get(&origin, "/hello").expect("the server still answers");
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("world"), "served: {text}");
}

/// A request that stops mid-headers does not end it either.
#[test]
fn a_truncated_request_does_not_deafen_the_server() {
    let server = server();
    let origin = server.origin();
    let address = origin.trim_start_matches("http://");

    for _ in 0..20 {
        if let Ok(mut stream) = TcpStream::connect(address) {
            // A request line and then silence.
            let _ = stream.write_all(b"GET /hello HTTP/1.1\r\n");
        }
    }

    let body = get(&origin, "/hello").expect("the server still answers");
    assert!(String::from_utf8_lossy(&body).contains("world"));
}

/// Shutdown is prompt even though the loop no longer breaks on error.
///
/// The loop is bounded by the shutdown flag rather than by any error, so a listener that is
/// genuinely dead costs one poll interval and not a hang. `Drop` joins the thread, so a slow
/// exit here would be a slow *test suite*, which is how a stuck server would present.
#[test]
fn dropping_the_server_returns_promptly() {
    let started = std::time::Instant::now();
    {
        let server = server();
        let _ = get(&server.origin(), "/hello");
    }
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "shutdown took {:?}",
        started.elapsed()
    );
}

/// Nothing ends the accept loop, including the error kind that has no name.
///
/// Enumerating the kinds to forgive was the original mistake in a different form: the failure
/// that matters most is a process out of descriptors, which has no `ErrorKind` of its own on
/// stable Rust and arrives as `Uncategorized`. A list would have missed exactly the one a
/// crowded test run produces.
#[test]
fn no_accept_error_ends_the_loop() {
    use std::io::ErrorKind;
    for kind in [
        ErrorKind::WouldBlock,
        ErrorKind::Interrupted,
        ErrorKind::ConnectionAborted,
        ErrorKind::ConnectionReset,
        ErrorKind::PermissionDenied,
        ErrorKind::OutOfMemory,
        ErrorKind::Other,
    ] {
        assert!(
            !tile_server::accept_error_is_fatal(kind),
            "{kind:?} ends the serving loop, which leaves a bound and deaf server"
        );
    }
}
