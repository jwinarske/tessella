# earcutr 0.5.0, with two corrections and a faster search for the same ears

Vendored rather than depended on, for a one-line divergence from `earcut.hpp`. §9.1 diffs this
crate's output against mbgl's pixel for pixel, and mbgl calls `earcut.hpp`; a triangulation that
is merely *valid* is not enough here, it has to be the same one.

## The hashing threshold

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

## A hole with no bridge is left out

`find_hole_bridge` answers NULL when no segment of the outer ring lies to a hole's left. The port
passed that straight to `split_bridge_polygon`, which linked the list's NULL sentinel into the
rings. `earcut.hpp` checks, `if (outerNode)`, and skips the hole. `eliminate_hole` now does the
same.

The corruption was usually harmless: it fell on the skipped hole's own list, which nothing reads
again. On holes that collapse to runs of one repeated point, it left `filter_points` walking a
cycle that never reached its end node, and `earcut` never returned. `earcut.hpp` returns the outer
ring's triangulation for the same input, and so does this now.
`crates/tessella-layout/tests/earcut_matches_earcut_hpp.rs` has that input and a plain no-bridge
square, each against the answer mbgl's copy gives.

The fill layer did not reach the hang with the rings it was found on: `fill::classify_rings` drops
zero-area holes first. It is corrected here because the triangulator's contract is to return, and
because tiles come from the network.

What else it changed: nothing that terminated before. On 48,139 polygons, and on 2,000 more
built to have no bridge, the only output that differs from the previous build is output the
previous build never produced.

## Where this still differs from `earcut.hpp`

Hole bridging. `find_hole_bridge` follows an older `earcut.js` than mbgl's `earcut.hpp` does. When
the hole touches an outer segment it picks `m.prev` where the C++ picks `m`. Its second pass starts
after `m` rather than at it, starts from a finite minimum, and breaks ties without
`sectorContainsSector`. Each of these can pick a different bridge. The result is still a valid
triangulation, but not the same triangles.

Measured against maplibre-native's vendored `earcut.hpp` on the 5,030 fill polygons of the
fixture tiles: 5,007 are identical index for index, 5,012 have the same triangles in another order,
and 18 have different triangles, 17 of them with holes. Correcting it changes those 18, so it is a
parity change to make against the oracle, and not one to fold into another.

## Provenance

Upstream is <https://github.com/frewsxcv/earcutr/> at 0.5.0, ISC, `LICENSE` kept beside this file.
`src/tests.rs` and the `tests/` fixtures are not vendored; the suite was run against the patch
before vendoring and all 37 pass, `test_water_huge` and the rest of the earcut fixtures included.
