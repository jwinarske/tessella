# earcutr 0.5.0, with one correction

Vendored rather than depended on, for a one-line divergence from `earcut.hpp`. §9.1 diffs this
crate's output against mbgl's pixel for pixel, and mbgl calls `earcut.hpp`; a triangulation that
is merely *valid* is not enough here, it has to be the same one.

## The change

`linked_list` decides whether to build the z-order hash:

```rust
-    if vertices.len() < 80 {
+    if vertices.len() / DIM <= 80 {
         ll.usehash = false;
```

`vertices` is the flat coordinate array, two values per point, so the original compares twice the
point count against a threshold that counts points. `earcut.hpp` sets `threshold = 80`, subtracts
each ring's point count, and hashes when `threshold < 0` -- above eighty points. The Rust reads as
above *forty*, so every polygon of 41 to 80 points takes the branch the C++ does not.

Hashed and unhashed ear-finding pick different ears on geometry with no valid triangulation --
self-intersecting rings, or open LineStrings that a fill layer closes into lassos, both of which
a real vector tile carries. The two answers differ by whole triangles.

## What it was worth

A Shanghai water tile at z12 has a five-ring group of 45 points. Unpatched: 13 triangles, one of
them spanning ground the rings never cover, drawn as a wedge of water across land. `earcut.hpp`:
12 triangles. Patched: 12 triangles, and the same 3,092,779 doubled area to the digit.

Against the oracle, that camera's water went from 6,282 gross pixels to 1.

## Provenance

Upstream is <https://github.com/frewsxcv/earcutr/> at 0.5.0, ISC, `LICENSE` kept beside this file.
`src/tests.rs` and the `tests/` fixtures are not vendored; the suite was run against the patch
before vendoring and all 37 pass, `test_water_huge` and the rest of the earcut fixtures included.
