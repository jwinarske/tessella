#!/usr/bin/env python3
# SPDX-License-Identifier: BSD-2-Clause
"""A record-and-replay proxy between both renderers and the origins an example names.

The MapLibre GL JS examples fetch from demotiles, OpenFreeMap and a handful of ad-hoc hosts. A
parity run that reaches those live measures whatever the origin served that minute, so both
renderers read through this instead: `http://127.0.0.1:PORT/<scheme>/<host>/<path>`.

Replay is the default. A URL the snapshot does not hold is answered 502 and logged as a MISS,
which is a failed run rather than a silently different one. `PARITY_RECORD=1` fetches what is
missing and keeps it.

The snapshot lives outside the tree, for the reason `tools/parity/README.md` gives: what an origin
serves stays where it is licensed. What the tree carries is each example's manifest of URL and
content hash, which is what makes a snapshot checkable.

JSON bodies -- a style, a TileJSON, a GeoJSON document -- are rewritten on the way out so the URLs
inside them point back here. The stored body is the origin's, unrewritten, so the rewrite can change
without a re-record.

It listens on loopback only. While recording it will fetch any http or https URL a local process
names, which is the job; it is a test tool and is not meant to run anywhere that is a problem.
"""

import hashlib
import http.server
import json
import os
import re
import socketserver
import sys
import tempfile
import threading
import urllib.error
import urllib.parse
import urllib.request
import zlib

# A tile is kilobytes and the largest GeoJSON an example names is about a megabyte. The bound is for
# the origin that answers with something else, and for a gzip member that inflates without limit.
MAX_BODY = 64 * 1024 * 1024

ABSOLUTE = re.compile(rb"(https?)://(?!127\.0\.0\.1[:/]|localhost[:/])")

log_lock = threading.Lock()


class TooLarge(Exception):
    pass


def port() -> int:
    return int(os.environ.get("PARITY_PROXY_PORT", "8082"))


def rewrite(body: bytes) -> bytes:
    """Points every absolute URL in `body` at this proxy, leaving loopback URLs alone."""
    prefix = f"http://127.0.0.1:{port()}/".encode()
    return ABSOLUTE.sub(lambda m: prefix + m.group(1) + b"/", body)


def origin_of(path: str) -> str | None:
    """`/https/host/a/b?q` -> `https://host/a/b?q`, with the path in one canonical spelling.

    The two renderers escape a glyph range's font stack differently -- `%20` against a literal
    space, `%2C` against a comma -- and a snapshot keyed on the raw spelling would record the same
    resource twice and miss it on replay whenever the other renderer asked first.
    """
    parts = path.lstrip("/").split("/", 2)
    if len(parts) < 3 or parts[0] not in ("http", "https") or not parts[1]:
        return None
    scheme, host, rest = parts
    rest, _, query = rest.partition("?")
    rest = urllib.parse.quote(urllib.parse.unquote(rest), safe="/,@:+")
    return f"{scheme}://{host}/{rest}" + (f"?{query}" if query else "")


def is_json(content_type: str, url: str) -> bool:
    # By declared type or by name only. Sniffing a leading `{` would also match a binary body that
    # happens to start with that byte, and the rewrite would corrupt it.
    path = urllib.parse.urlsplit(url).path
    return "json" in content_type.lower() or path.endswith((".json", ".geojson"))


class Snapshot:
    def __init__(self, root: str, log: str, recording: bool) -> None:
        self.root, self.log, self.recording = root, log, recording

    def paths(self, url: str) -> tuple[str, str]:
        key = hashlib.sha256(url.encode()).hexdigest()
        return (
            os.path.join(self.root, key[:2], key + ".body"),
            os.path.join(self.root, key[:2], key + ".json"),
        )

    def load(self, url: str) -> tuple[dict, bytes] | None:
        body_path, meta_path = self.paths(url)
        try:
            with open(meta_path) as meta_file:
                meta = json.load(meta_file)
            with open(body_path, "rb") as body_file:
                body = body_file.read()
        except (OSError, ValueError):
            return None
        # A body that does not hash to what the metadata says was not written by a finished record.
        if hashlib.sha256(body).hexdigest() != meta.get("sha256"):
            return None
        return meta, body

    def store(self, url: str, status: int, content_type: str, body: bytes) -> dict:
        body_path, meta_path = self.paths(url)
        meta = {
            "url": url,
            "status": status,
            "content_type": content_type,
            "sha256": hashlib.sha256(body).hexdigest(),
        }
        # Body before metadata, each by rename: a record interrupted part way leaves either nothing
        # or a body with no metadata, and both read as not recorded.
        write_atomically(body_path, body)
        write_atomically(meta_path, json.dumps(meta).encode())
        return meta

    def note(self, line: str) -> None:
        with log_lock, open(self.log, "a") as out:
            out.write(line + "\n")


def write_atomically(path: str, data: bytes) -> None:
    directory = os.path.dirname(path)
    os.makedirs(directory, exist_ok=True)
    handle, temporary = tempfile.mkstemp(dir=directory)
    try:
        with os.fdopen(handle, "wb") as out:
            out.write(data)
        os.replace(temporary, path)
    except BaseException:
        os.unlink(temporary)
        raise


def read_bounded(stream) -> bytes:
    body = stream.read(MAX_BODY + 1)
    if len(body) > MAX_BODY:
        raise TooLarge(f"body over {MAX_BODY} bytes")
    return body


def fetch(url: str) -> tuple[int, str, bytes]:
    # origin_of admits only http and https; checked again here because this is where it matters.
    if urllib.parse.urlsplit(url).scheme not in ("http", "https"):
        raise ValueError(f"refusing to fetch {url!r}")
    request = urllib.request.Request(url, headers={"User-Agent": "tessella-parity/0.1"})  # noqa: S310
    try:
        response = urllib.request.urlopen(request, timeout=60)  # noqa: S310
    except urllib.error.HTTPError as error:
        response = error
    with response:
        status = response.status if hasattr(response, "status") else response.code
        headers = response.headers
        body = read_bounded(response)
    # Stored decoded: a renderer is handed what the origin meant, and replay then serves it with no
    # Content-Encoding to disagree about.
    if (headers.get("Content-Encoding") or "").lower() == "gzip":
        inflater = zlib.decompressobj(wbits=31)
        body = inflater.decompress(body, MAX_BODY + 1)
        if len(body) > MAX_BODY or inflater.unconsumed_tail:
            raise TooLarge(f"inflates past {MAX_BODY} bytes")
    return status, headers.get("Content-Type") or "application/octet-stream", body


class Handler(http.server.BaseHTTPRequestHandler):
    # Keep-alive with a Content-Length on every answer. Under HTTP/1.0 the connection closes after
    # each response, which a client holding it open reads as the peer disconnecting.
    protocol_version = "HTTP/1.1"
    snapshot: Snapshot

    def answer(self, status: int, content_type: str, body: bytes) -> None:
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:
        snapshot = self.snapshot
        url = origin_of(self.path)
        if url is None:
            self.answer(400, "text/plain", b"not a proxied url\n")
            return
        kind = "HIT"
        loaded = snapshot.load(url)
        if loaded is not None:
            meta, body = loaded
        elif snapshot.recording:
            try:
                status, content_type, body = fetch(url)
            except Exception as error:  # a transport failure is not a snapshot
                snapshot.note(f"FAIL - - {url} {error}")
                self.answer(502, "text/plain", f"{error}\n".encode())
                return
            meta = snapshot.store(url, status, content_type, body)
            kind = "REC"
        else:
            snapshot.note(f"MISS - - {url}")
            self.answer(502, "text/plain", b"not in snapshot\n")
            return
        snapshot.note(f"{kind} {meta['status']} {meta['sha256']} {url}")
        if is_json(meta["content_type"], url):
            body = rewrite(body)
        self.answer(meta["status"], meta["content_type"], body)

    def log_message(self, *args) -> None:
        pass


class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    allow_reuse_address = True
    daemon_threads = True


def main() -> None:
    root = os.environ.get("PARITY_EXAMPLES_DATA")
    if not root:
        sys.exit("PARITY_EXAMPLES_DATA is not set; run through examples.sh, which sources env.sh")
    os.makedirs(root, exist_ok=True)
    recording = os.environ.get("PARITY_RECORD") == "1"
    Handler.snapshot = Snapshot(
        root, os.environ.get("PARITY_PROXY_LOG", os.path.join(root, "proxy.log")), recording
    )
    with Server(("127.0.0.1", port()), Handler) as server:
        print(
            f"proxy on {port()}, {'recording' if recording else 'replaying'} into {root}",
            file=sys.stderr,
            flush=True,
        )
        server.serve_forever()


if __name__ == "__main__":
    main()
