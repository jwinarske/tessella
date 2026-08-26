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
- `tile.png`, `tile.jpeg`, `tile.webp` — three raster tiles, 256 a side. mbgl's own tests assert
  the size of each and nothing more, and that is all they have in common as a set: the PNG and
  the WebP are the same photograph, and the **JPEG is a different one** — its red channel means
  117.6 against the other two's 63.9. Reading them as one picture in three encodings is a
  mistake the tests here name explicitly so nobody makes it twice.
  The WebP is worth more than its rarity suggests. It is a `VP8 ` chunk — lossy — inside an
  extended `VP8X` container with an EXIF chunk beside it, so decoding it exercises the harder of
  the two container paths and the whole YUV-to-RGB conversion; and it still agrees with the PNG
  to within a tenth of a level on every channel mean, which is what makes that agreement an
  assertion rather than a coincidence.

Licensed as maplibre-native is (BSD-2-Clause); see `tests/sprite-fixtures/README.md` for the
same note about the vendored sprite pair.
