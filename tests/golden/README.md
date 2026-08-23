# Golden oracle dumps

Reference serializations of the capture stream, produced by the C++ implementation. The Rust
frontend runs the same style at the same camera, dumps in the same format, and the two are
diffed — which is what turns "does the expression evaluator round identically" from
archaeology into a failing test (plan.md §9.1).

Committing the dumps means the Rust side can be checked without a C++ checkout or a
maplibre-native build. Regenerating them needs both.

| file | style | camera |
|---|---|---|
| `hermetic_style.dump` | the probe's built-in inline GeoJSON style, no network | 51.505, -0.11 @ z13, 1024x768 |

## Regenerating

```sh
cd <maplibre-native>/build-capture
ninja mbgl-capture-probe
./mbgl-capture-probe --dump=<tessella>/tests/golden/hermetic_style.dump
```

Produced by `mbgl-capture-probe` at maplibre-native `796341e27793`, on the
`capture-backend-phase0` branch whose base commit `b237943` plan.md pins. Byte-identical
across six consecutive runs; if a regeneration produces a diff on unrelated lines, that is a
determinism regression in the probe rather than a change in the frontend, and the four
sources of variance already found are described in that commit.

## Reading the actual values

`--dump-vertices` prints coordinates rather than hashes, decoding the `Short2` position
attribute into int16 pairs. It is deliberately not part of the dump — a hash is what keeps the
golden file bounded and diffable — but it is how the tile-coordinate pipeline gets built
against measured values instead of guesses. It is what established that the hermetic style's
fill geometry clips to `-2048..10240` at extent 8192, and that rings keep their closing
duplicate vertex.

## What is deliberately not compared

Some of what the stream carries is not something two implementations have any reason to agree
on, and comparing it would produce failures that mean nothing:

- **`symbol_fade_change`** (global paint params, offset 28) is a wall-clock fade ratio. Elided,
  shown as `--`.
- **Order within a draw group.** mbgl's iteration over a layer's tiles is not deterministic;
  within a layer the cover tiles do not overlap and their relative order is resolved by the
  stencil rather than by the painter's algorithm (§11.2). The grouping and the relative order
  of groups are compared exactly — that is what painter order means — but the sequence inside
  a `(pass, layer, sublayer, priority)` group is canonicalized away.
- **`uboIndex`.** An artifact of that same arbitrary iteration. What it points at is compared;
  which slot it happens to occupy is not.
- **Intra-array order of a consolidated buffer.** Sorted at 16-byte granularity, the
  std140/std430 alignment unit, so a block boundary never falls inside a member.
- **Triangle emission order.** Index buffers are hashed canonically: each triangle rotated to
  start at its lowest index, then the triangles sorted. Measured reason — `earcutr` and
  `earcut.hpp` produce the *same* triangulation, but emit it in a different order once a
  polygon has a hole. Same triangles, same total area, same winding on every one; simple
  polygons, concave included, agree index for index. Rotation preserves winding, which is a
  real property since a reversed triangle is backface-culled, so a winding flip still fails.
  What is discarded is the sequence, which nothing downstream depends on.
