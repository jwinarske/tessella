#!/usr/bin/env python3
# SPDX-License-Identifier: BSD-2-Clause
#
# The icon a symbol annotation names. Generated rather than committed blind, so the pixels have a
# stated origin: an opaque disc with a darker rim, on a transparent ground, premultiplied by the
# consumer rather than here.
#
#   python3 marker.py marker.png
import struct
import sys
import zlib

N = 16
CENTER = (N - 1) / 2
rows = []
for y in range(N):
    row = bytearray([0])  # filter 0
    for x in range(N):
        d = ((x - CENTER) ** 2 + (y - CENTER) ** 2) ** 0.5
        if d <= 5.0:
            row += bytes((236, 72, 64, 255))
        elif d <= 6.5:
            row += bytes((90, 20, 18, 255))
        else:
            row += bytes((0, 0, 0, 0))
    rows.append(bytes(row))


def chunk(tag, body):
    return struct.pack(">I", len(body)) + tag + body + struct.pack(">I", zlib.crc32(tag + body))


png = b"\x89PNG\r\n\x1a\n"
png += chunk(b"IHDR", struct.pack(">IIBBBBB", N, N, 8, 6, 0, 0, 0))
png += chunk(b"IDAT", zlib.compress(b"".join(rows), 9))
png += chunk(b"IEND", b"")
with open(sys.argv[1] if len(sys.argv) > 1 else "marker.png", "wb") as out:
    out.write(png)
