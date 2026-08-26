# Image fixtures

maplibre-native's own `test/fixtures/image/`, vendored unchanged. `test/util/image.test.cpp`
states what each one decodes to, and those expectations are transcribed into
`crates/tessella-source/tests/image.rs` — so this side is checked against the oracle's numbers
rather than against its own.

- `no_profile.png`, `profile.png` — one pixel, opaque half-red. The pair exists because one
  carries an ICC profile and the other does not, and mbgl expects the *same* pixel from both: a
  decoder honouring the profile would colour-manage the tile and disagree with every other tile
  on the map.
- `no_profile_alpha.png`, `profile_alpha.png` — the same pixel at half alpha, and what says
  the decode premultiplies: the file holds `128, 0, 0` at alpha `128` and mbgl expects
  `64, 0, 0, 128`.
- `tile.png`, `tile.jpeg`, `tile.webp` — one raster tile in three encodings, 256 a side. The
  JPEG is what a satellite basemap actually serves; the WebP is the format this build does not
  yet read, and it is kept so the refusal is asserted against a real file rather than against
  random bytes.

Licensed as maplibre-native is (BSD-2-Clause); see `tests/sprite-fixtures/README.md` for the
same note about the vendored sprite pair.
