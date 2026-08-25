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
| `composite_style.dump` | `crates/tessella-style/tests/composite_style.json` | 51.505, -0.11 @ z13, 1024x768 |
| `composite_style_z13_5.dump` | the same | 51.505, -0.11 @ **z13.5**, 1024x768 |
| `live_protomaps_z5.dump` | `crates/tessella-style/tests/live_style.json` | 51.505, -0.11 @ z5, 1024x768 |
| `symbol_style.dump` | `crates/tessella-style/tests/symbol_style.json` | 51.505, -0.11 @ z13, 1024x768 |

### The one that is not hermetic

`live_protomaps_z5.dump` is captured against a **real** style over **real** tiles: a Protomaps
planet extract served locally by `pmtiles serve`, nine tiles at zoom 5, thousands of features.
Both the probe and the frontend fetch from the same origin, so both see identical bytes and any
difference is in what they do with them.

That is what the hermetic captures cannot be. A hermetic style is four features chosen to be
tractable; agreement on it says the rules were transcribed, not that they hold. This one found
a divergence nothing else could: a `water` layer whose two features are a polygon and a *point*,
where mbgl draws the point as a one-vertex degenerate ring and this build was filtering
non-polygons out of fill layers. One vertex in two thousand, in one tile.

Regenerating it needs the tile server running as well as a maplibre-native checkout, and the
archives are not in this repository — see `tools/tile-server` and the module docs of
`tessella-orchestrate/tests/live_parity.rs`.

### The one with seven elided lines

`symbol_style.dump` is the first capture with a symbol layer, and it is the first that does not
fully reproduce. mbgl packs the glyph atlas in the order glyphs arrive, and that order is not
deterministic. Over ten consecutive captures of the identical style the symbol vertex hashes and
the atlas texture hash each took four or five distinct values, one dominating; **every other line
of the eighty-seven was identical every time**.

The vertex hashes follow the atlas rather than varying on their own: the `data` attribute carries
each glyph's texture coordinates, so a different packing is different vertex bytes for identical
geometry. Attributes 0, 1 and 2 share that interleaved buffer and move together.

Attributes 3 and 4 — the dynamic position and the fade opacity, each in a buffer of its own —
were byte-identical across all ten. They stay in the golden, because eliding a stable line gives
away a comparison for nothing.

So seven lines are elided, the way `symbol_fade_change` already is, by
`tools/mbgl-codegen/oracles/elide_symbol_atlas.py`. Run it after capturing or the file will not
match. What remains still pins the drawable identities, the vertex counts, the index buffers
byte for byte, the five-attribute layout, and the painter order.

Making the vertex bytes comparable needs the atlas packed deterministically on mbgl's side.
That is a change to the probe, not to this repository, and it is the one thing standing between
symbols and the byte-exact parity the fill and line layers have.

The style's `glyphs` URL carries a `TESSELLA` placeholder for the checkout path, the way
`live_style.json` carries its origin. The font it names is vendored at
`tests/glyph-fixtures/TestFont/0-255.pbf`, so both the probe and the frontend read identical
bytes.

### Why there are three hermetic ones

The hermetic style has no paint property that varies with zoom *and* per feature, so it says
nothing about how such a property binds. `composite_style.json` is a minimal delta from it —
identical source geometry, identical layer count and order, two layers' paint turned into
`interpolate` curves over `zoom` whose stops are `match` expressions on a feature property.
The vertex and index buffers are byte-identical between the two dumps, which is what makes the
paint buffers the only thing that differs.

The third of those exists because at an exactly integer camera zoom over a tile of that zoom, the mix
factor between a composite property's two endpoints is *zero* — so `composite_style.dump` and
`hermetic_style.dump` have byte-identical uniform buffers, and an implementation that never
computed the factor would pass against either. At 13.5 mbgl writes 0.5, and the difference
becomes visible. It is also the only capture at a fractional zoom, so it is what checks the
camera off an integer.

The R0 reference is deliberately left alone: it is what the whole stream is diffed against and
it is frozen, so a new question gets a new capture rather than an amended one.

## Regenerating

```sh
cd <maplibre-native>/build-capture
ninja mbgl-capture-probe

# The symbol capture needs the checkout path substituted, and its dump elided afterwards.
sed "s|TESSELLA|<tessella>|" <tessella>/crates/tessella-style/tests/symbol_style.json > /tmp/symbol.json
./mbgl-capture-probe file:///tmp/symbol.json --dump=<tessella>/tests/golden/symbol_style.dump
python3 <tessella>/tools/mbgl-codegen/oracles/elide_symbol_atlas.py \
    <tessella>/tests/golden/symbol_style.dump

./mbgl-capture-probe --dump=<tessella>/tests/golden/hermetic_style.dump
./mbgl-capture-probe file://<tessella>/crates/tessella-style/tests/composite_style.json \
    --dump=<tessella>/tests/golden/composite_style.dump
./mbgl-capture-probe file://<tessella>/crates/tessella-style/tests/composite_style.json \
    --zoom=13.5 --dump=<tessella>/tests/golden/composite_style_z13_5.dump
```

Produced by `mbgl-capture-probe` at maplibre-native `5f9d3f77caac`, on the
`capture-backend-phase0` branch whose base commit `b237943` plan.md pins. Byte-identical
across six consecutive runs; if a regeneration produces a diff on unrelated lines, that is a
determinism regression in the probe rather than a change in the frontend, and the four
sources of variance already found are described in that commit. `5f9d3f77caac` added the
`--zoom` flag; a capture without it is byte-identical to one from before, which was checked
against `hermetic_style.dump` rather than assumed.

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
