#!/usr/bin/env python3
# SPDX-License-Identifier: BSD-2-Clause
#
# The terrain a hillshade scene reads, generated rather than fetched.
#
# # Why generated
#
# Three reasons, in the order they decided it. There is no DEM in the archives this harness
# serves and every public one carries a license; a height field written here is nobody's data and
# redistributes nothing. A procedural field is the *same* field on both sides by construction,
# which is what makes the comparison a comparison rather than two fetches that might have
# differed. And it has an analytic derivative, so the normals a hillshade computes can be checked
# against what the surface actually does rather than only against the other renderer.
#
# # Why it is one global function
#
# Sampled per tile out of a function of world position, not generated per tile. A hillshade's
# prepare pass backfills each tile's border from its neighbors, so a field with a seam at a tile
# edge would make a correct backfill look broken and a broken one look fine. This has no seams:
# neighboring tiles agree exactly along their shared edge because they are sampling the same
# function at the same point.
#
# # The encoding
#
# Mapbox Terrain-RGB, which is what `"encoding": "mapbox"` means and what mbgl reads by default:
#
#     height = -10000 + (R * 65536 + G * 256 + B) * 0.1
#
# So a tenth of a meter per unit over a range that covers the planet. `terrarium` is the other
# encoding mbgl reads; this serves one and the scene names it.
#
#   python3 dem.py                 serve on PARITY_DEM_PORT (default 8084)
#   python3 dem.py 14 8802 5373    write one tile to stdout, for looking at
import hashlib
import http.server
import math
import os
import re
import struct
import sys
import zlib

TILE_SIZE = 256
TILE = re.compile(r"^/dem/(\d+)/(\d+)/(\d+)\.png$")

# Separable sinusoids at named wavelengths, in meters of ground rather than in octaves of the
# world.
#
# Octaves of the whole world was the first attempt and it is the trap: amplitudes that fall by
# roughly half an octave put a meter and a half of relief inside a z14 tile, and a meter and a
# half shades to a flat wash that compares equal whatever either renderer does with it. What a
# hillshade reads is the *slope*, so the wavelengths are chosen around the zoom the scene is
# rendered at and each amplitude is set from the slope it is meant to produce.
#
# `SLOPE` is that slope, per component. A sinusoid of wavelength L and amplitude A has maximum
# gradient 2*pi*A/L, so A = SLOPE * L / (2*pi). Six components at 0.15 each is terrain that is
# steep in places and flat in others, which is what makes a shaded picture worth comparing.
#
# Mercator stretches meters by 1/cos(latitude), so these are ground meters at the equator and
# about 0.6 of that at Berlin. It does not matter to the comparison -- both renderers read the
# same bytes -- and it is recorded so nobody measures a slope here and finds it steeper than the
# number above.
EARTH_CIRCUMFERENCE = 40_075_016.686
WAVELENGTHS_M = (8000.0, 4000.0, 2000.0, 1000.0, 500.0, 250.0)
SLOPE = 0.15
SEA_LEVEL = 400.0


def _phase(index, salt):
    digest = hashlib.sha256(f"{salt}:{index}".encode()).digest()
    return struct.unpack(">I", digest[:4])[0] / 2**32 * math.tau


_COMPONENTS = [
    (
        EARTH_CIRCUMFERENCE / wavelength,  # cycles across the world, so u is in 0..1
        SLOPE * wavelength / math.tau,
        _phase(index, "u"),
        _phase(index, "v"),
    )
    for index, wavelength in enumerate(WAVELENGTHS_M)
]


def height(u, v):
    """Elevation in meters at a point in world Mercator, both axes in 0..1."""
    total = SEA_LEVEL
    for cycles, amplitude, u_phase, v_phase in _COMPONENTS:
        f = math.tau * cycles
        total += amplitude * math.sin(f * u + u_phase) * math.cos(f * v + v_phase)
    return total


def encode(meters):
    """Terrain-RGB. Clamped, because the encoding's range is the planet's and this is not."""
    units = int(round((meters + 10000.0) / 0.1))
    units = max(0, min(units, 256**3 - 1))
    return units >> 16, (units >> 8) & 0xFF, units & 0xFF


def tile(z, x, y, size=TILE_SIZE):
    """One tile's pixels, RGB, row-major from the top."""
    n = 1 << z
    out = bytearray()
    for j in range(size):
        v = (y + (j + 0.5) / size) / n
        for i in range(size):
            u = (x + (i + 0.5) / size) / n
            out += bytes(encode(height(u, v)))
    return bytes(out)


def png(width, height_px, rgb):
    raw = b"".join(
        b"\x00" + rgb[row * width * 3 : (row + 1) * width * 3] for row in range(height_px)
    )

    def chunk(tag, body):
        return (
            struct.pack(">I", len(body))
            + tag
            + body
            + struct.pack(">I", zlib.crc32(tag + body))
        )

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height_px, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 6))
        + chunk(b"IEND", b"")
    )


class Handler(http.server.SimpleHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        match = TILE.match(self.path)
        if not match:
            self.send_error(404)
            return
        z, x, y = (int(part) for part in match.groups())
        body = png(TILE_SIZE, TILE_SIZE, tile(z, x, y))
        self.send_response(200)
        self.send_header("Content-Type", "image/png")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Access-Control-Allow-Origin", "*")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    if len(sys.argv) == 4:
        z, x, y = (int(part) for part in sys.argv[1:4])
        sys.stdout.buffer.write(png(TILE_SIZE, TILE_SIZE, tile(z, x, y)))
    else:
        port = int(os.environ.get("PARITY_DEM_PORT", "8084"))
        http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
