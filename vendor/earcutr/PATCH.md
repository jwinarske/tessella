# earcutr 0.5.0, with one correction and a faster search for the same ears

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

## Faster, and still the same triangles

Ear-finding was most of what building a fill tile costs: 43% of every tick on four views
sweeping a real tile. Four changes make it cheaper without changing a single triangle. Each is
marked `tessella:` in `src/lib.rs`.

- `is_ear` read its triangle out of the list for every point it tested, copying three whole
  nodes each time. It reads the three corners once and walks the ring by index, the way
  `NodeIterator` walks it: the node after `next` first and unconditionally, then on up to `prev`.
  On a ring of three that is the whole ring, as upstream has it.
- The area and point-in-triangle arithmetic moved into `coord_area` and `point_in_triangle`,
  which take coordinates rather than nodes. `NodeTriangle::area` and `contains_point` now call
  them, so every caller uses one copy of the expressions: the same operations in the same order.
  Rust does not contract a multiply and an add into a single rounding unless asked, so the results
  are the same bits.
- `signed_area` is a loop over the same pairs in the same order that the `cycle().skip().step_by()`
  chain produced, so the terms are summed in the same sequence.
- The node list and the triangle list are allocated at the size they reach. Upstream reserved one
  node per point and then pushed the NULL node too, which reallocated and copied every list at
  least once. It also reserved a third of the triangle indices.

What that bought on the sweep: `earcut` fell from 462 to 312 million instructions per sweep, and
the whole tick by 12%. The worst frame fell 10%, because crossing frames are mostly tessellation.

What establishes that nothing else changed: a differential run of this crate against 0.5.0 with
the threshold correction alone. It covered 48,139 polygons and 1.59 million triangles, with
identical output for every one. The corpus was every polygon of every vector tile under `tests/`,
raw, at the fill layer's 8192 extent, and clipped and scaled into each quadrant of three
overzoom levels, plus 36,504 synthetic rings: simple, holed, self-intersecting, repeated-point,
collinear and lasso shapes, on both sides of the hashing threshold.
`crates/tessella-layout/tests/earcut_pinned.rs` keeps the fixture half of that as a hash taken
before these changes. A later change that alters any of those triangulations fails there.

## Known divergence, not yet corrected

On a polygon whose holes collapse to runs of one repeated point, `filter_points` never returns
when it is called from `eliminate_hole`. `earcut.hpp` returns the outer ring's triangulation for
the same input. The reproducer is an outer ring of four points with four holes on a 512-unit grid:

```text
flat  = [5632,5632, 6144,6656, 5632,1536, 5120,3072,
         3072,4096, 3072,4096, 3072,4096, 3072,4096,
         5120,4608, 5120,4608, 5120,4608, 4608,4608, 5120,4096, 5120,4096, 5120,4096, 5120,4096, 5120,4096,
         4096,5120, 4096,5120, 4096,5120, 4096,4608, 4096,5120, 4608,5120, 4096,5120, 4096,5120,
         3072,4608, 3072,4608, 3072,4608, 3072,4608, 3072,4608, 3072,4608, 3072,4096, 3072,4096, 3072,4096]
holes = [4, 8, 17, 25]
```

The fill layer does not reach it with these rings: `fill::classify_rings` drops the zero-area
holes first. Whether a hole with some area but repeated points can still reach it has not been
established. It predates the changes above, which do not touch `filter_points`.

## Provenance

Upstream is <https://github.com/frewsxcv/earcutr/> at 0.5.0, ISC, `LICENSE` kept beside this file.
`src/tests.rs` and the `tests/` fixtures are not vendored; the suite was run against the patch
before vendoring and all 37 pass, `test_water_huge` and the rest of the earcut fixtures included.
