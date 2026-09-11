# earcutr 0.5.0, brought to `earcut.hpp`'s behavior

Vendored rather than depended on, for a one-line divergence from `earcut.hpp` at first and for
the rest of them since. §9.1 diffs this crate's output against mbgl's pixel for pixel, and mbgl
calls `earcut.hpp`; a triangulation that is merely *valid* is not enough here, it has to be the
same one.

It now is. On every polygon it has been measured against -- 48,139 of them, described below -- it
emits `earcut.hpp`'s triangles in `earcut.hpp`'s order, index for index. The reference is the
`earcut.hpp` in maplibre-native's `vendor/earcut.hpp`, at revision `0d0897a`.

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
  nodes each time. It reads the three corners once and walks the ring by index. (It kept
  upstream's walk, which tested its first node unconditionally; the port of `earcut.hpp` below
  replaced that with the C++ loop.)
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
`crates/tessella-layout/tests/earcut_pinned.rs` kept the fixture half of that as a hash taken
before these changes, so a later change that altered one of those triangulations would fail there.
The port below altered 18 of them on purpose, and the hash it pins now is `earcut.hpp`'s.

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

## The rest of `earcut.hpp`

Upstream earcutr follows an older `earcut.js` than the `earcut.hpp` mbgl calls. Where the two
differ, this crate now does what `earcut.hpp` does. Each change is marked `tessella:` in
`src/lib.rs`.

- **Bridging a hole.** `find_hole_bridge` is `findHoleBridge`, statement for statement. Upstream
  answered `m.prev` where a hole touches an outer segment, where the C++ answers `m`. Its second
  pass began after `m`, from a finite minimum, and broke ties on x alone. The C++ begins at `m`,
  from infinity, and breaks ties with `sectorContainsSector`, which is ported too.
- **Where a hole starts.** A hole is bridged from `getLeftmost`'s point: the least x, then the
  least y among those, walking from the node `linkedList` returned. Upstream took the first point
  of least x in insertion order.
- **The order holes are bridged in.** `eliminateHoles` sorts them by that point's x with
  `std::sort`, which is not stable. `cpp_sort` is libstdc++'s `std::sort`, step for step:
  median-of-three introsort down to runs of sixteen, heapsort past the depth limit, and a final
  insertion sort. Up to sixteen holes that is a stable insertion sort. Above it, holes whose points
  share an x land where the partitioning puts them, and clipping to a tile edge makes that common.
  Checked against libstdc++ on 20,000 arrays of up to 1,999 elements with heavy ties, and the heap
  path against `std::partial_sort`, with the same order every time.
- **The z-order hash.** The bounding box is the outer ring's, taken once the holes are bridged into
  it, and `inv_size` is the reciprocal of its longer side. `zorder` is `zOrder`:
  `32767 * (x - min) * inv_size` on the untranslated coordinate, truncated to 32 bits. Upstream
  built the box while reading the rings, reading hole coordinates at ring-relative indices into the
  whole array. It then translated every point by its minimum and scaled by `32767 / size`, so every
  later area test ran on translated coordinates.
- **Is it an ear.** `is_ear` stops before testing `prev`, as `isEar` does, so a ring of three has
  nothing to test. Upstream tested its first node unconditionally. On a ring of three that is
  `prev`, whose own triangle is the ear's rotated, and on non-integer coordinates its area rounds
  to the other sign often enough to refuse the last ear of a polygon.
- **Segment intersection.** `intersects` is `earcut.hpp`'s: orientation signs for the general case,
  and `on_segment` for the four collinear ones. Upstream's `pseudo_intersects` counted only proper
  crossings and coincident segments.
- **Splitting.** `is_valid_diagonal` is `isValidDiagonal`, which also refuses a diagonal that
  creates opposite-facing sectors and accepts the zero-length one between two coincident convex
  vertices. `split_earcut` runs the half-polygons in the mode the polygon is in. Upstream always ran
  the hashed loop, over a z-order an unhashed polygon never built.
- **Curing.** Pass 1 filters points before `cure_local_intersections` and filters its result, as
  `earcutLinked` does.
- **Small things.** A ring of one steiner point gets no early NULL from `filter_points`. An outer
  ring of one or two points returns before its holes are bridged in. A hole ring with no points is
  skipped, as `linkedList`'s null is, rather than failing the polygon.

What that did, measured against `earcut.hpp` over the corpus the speed-up was checked on:

| | before | after |
|---|---|---|
| identical, index for index | 45,760 | **48,139** |
| the same triangles, in another order | 241 | 0 |
| different triangles | 2,138 | **0** |

The fixture tiles' 5,030 fill polygons are all identical. Before, 18 of them had different
triangles and 5 had the same triangles in another order; `earcut_pinned` now pins
`earcut.hpp`'s own hash for them. On 2,000 polygons built to have no bridge -- holes left of or
outside the ring, collapsed to a point, full of repeated points -- every one matches. It is also
cheaper: without a box to keep per vertex, `earcut` fell from 338 to 278 million instructions on
the four-view sweep.

## Where it could still differ

- **Another standard library.** `cpp_sort` is libstdc++'s sort, because the oracle is built with
  it. An mbgl built against libc++ or MSVC's library orders tied holes differently, but only when
  there are more than sixteen of them.
- **Another `earcut.hpp`.** The reference is revision `0d0897a`. Later revisions changed how holes
  are eliminated. If the oracle's mbgl moves to one, measure again against that revision.
- **Out-of-range z-order.** `zorder` saturates a value outside the 32-bit range, where
  `static_cast<int32_t>` is undefined. That needs a point outside the box the z-order is taken over,
  and every point the hash is asked about lies inside it.

## Provenance

Upstream is <https://github.com/frewsxcv/earcutr/> at 0.5.0, ISC, `LICENSE` kept beside this file.
The behavior it was brought to is that of `earcut.hpp` (ISC, © Mapbox), as vendored by
maplibre-native at revision `0d0897a`.
`src/tests.rs` and the `tests/` fixtures are not vendored; the suite was run against the patch
before vendoring and all 37 pass, `test_water_huge` and the rest of the earcut fixtures included.
