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
| `pattern_style.dump` | `crates/tessella-style/tests/pattern_style.json` | 51.505, -0.11 @ z13, 1024x768 |

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

### The one for a feature that is not built yet

`pattern_style.dump` is the oracle for the pattern binders, captured before the binders exist.
§12's list recorded them as **blocked rather than deferred** — "no golden carries a pattern layer
until R3 brings the textures, so writing the binder now means writing it against nothing to diff
it with". This is that golden, so they are no longer blocked.

Its style is a delta from `composite_style.json`: identical source geometry, so nothing about the
geometry is what differs, plus a sprite and four pattern layers chosen to cover the cases that
bind differently — a `background-pattern`, a constant `fill-pattern`, a `fill-pattern` that steps
across the capture zoom so `from` and `to` name *different* sprites, and a `line-pattern`.

It settles several things that reading mbgl's source did not:

- Each layer's shader: background 4, fill 13 with its outline 14, line 27.
- **A data-driven pattern keeps those same shaders and adds two vertex attributes**, ids 4 and 5
  — `idFillPatternFromVertexAttribute` and `idFillPatternToVertexAttribute`. So the composite
  binder differs from the constant one in what it puts in the vertex buffer, not in what it
  draws with, which is not what reading the two binder classes suggests.
- **A stepped pattern binds no vertex attribute either.** Only `id=0`, the position, appears on
  any of the thirty-six drawables. A cross-faded binder goes composite only when the expression
  is *data-driven*; a zoom step is a camera function, so it stays constant and both rectangles
  travel as uniforms. The constant path covers more than its name suggests.
- The image atlas is **shared across tiles**, not built per tile. All six tiles bind the same
  texture; what is per tile is the position map, not the pixels. mbgl uploads into a
  process-wide `DynamicTextureAtlas` and hands each tile back where its images landed plus
  refcounted handles, released when the tile goes obsolete. So a rectangle in the capture is a
  position in that shared atlas rather than a coordinate in the source sheet.

The sprite is `tests/sprite-fixtures/emerald`, which is maplibre-native's own fixture, so the
probe and the frontend read identical bytes. Its base URL carries the `TESSELLA` placeholder the
way `symbol_style.json`'s `glyphs` does.

### The pattern capture's five elided lines

Same cause as the symbol capture's seven, one level along: mbgl packs that shared image atlas in
the order images arrive, and the order is not deterministic. Over three consecutive captures
four or five lines of two hundred and fifty-two moved and nothing else did.

What moves is the atlas texture's hash and the rectangle each pattern UBO carries — a rectangle
names where an image was packed, so a different packing is a different rectangle for the same
sprite. The *size* does not move: `sand_noise` is fifty by fifty in every capture, only its
origin travels. The texture count moves too, because some runs emit an extra 1x1 placeholder.

`tools/mbgl-codegen/oracles/elide_pattern_atlas.py` elides the pattern buffers and the atlas
hash, and drops the placeholders. It also elides the data-driven layer's per-vertex rectangle
buffers, ids 4 and 5 — those follow the packing exactly as the uniforms do, and everything else
about their descriptors is a property of the shader and stays. It keeps each buffer's `size=`, which is a property of
the shader's block rather than of the packing, so a wrong block size still fails. Run it after
capturing or the file will not match.

What remains pins all thirty-six drawables, the shaders, the texture slot each binds, every
attribute descriptor and segment, the index buffers, the painter order and the camera. What it
cannot pin is where in the atlas a pattern landed — which wants the same fix as the symbol case,
a deterministic packing on mbgl's side.

### The drawable index is not an identity

A `fill-extrusion-pattern` layer nearly did not make it into this capture. A translucent
extrusion emits *four* drawables per tile — two shaders, `sh0018` and `sh0019`, by two
render-state sets — and which of them is `#00` swaps between runs.

Looking at one capture is what settled what to do. Within a single dump the pairing is already
inconsistent: `sh0018` carries flags `1111` at `#00` while `sh0019` carries them at `#01`. So the
number is not naming anything about the drawable. It is a position in an arbitrary iteration —
precisely what this file already says of `uboIndex`, "what it points at is compared; which slot
it happens to occupy is not" — and it takes the same treatment.

`canonicalize_drawable_index.py` renumbers within each group by what distinguishes its members
(pass, flags, vertex type, index buffer) and rewrites every line naming an identity, so `attr`,
`seg`, `tex` and `draw` follow their drawable. It sorts the blocks too, because mbgl lists them
in visit order and the same set arrives in a different sequence. Painter order is untouched: that
is the `draw` lines, and they are compared exactly.

Eliding would have thrown the drawables away. Renumbering keeps every one and discards only the
order they were visited in, which is the difference worth the script.

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

# The pattern capture needs the same substitution, and its own elision.
sed "s|TESSELLA|<tessella>|" <tessella>/crates/tessella-style/tests/pattern_style.json > /tmp/pattern.json
./mbgl-capture-probe file:///tmp/pattern.json --dump=<tessella>/tests/golden/pattern_style.dump
python3 <tessella>/tools/mbgl-codegen/oracles/canonicalize_drawable_index.py \
    <tessella>/tests/golden/pattern_style.dump
python3 <tessella>/tools/mbgl-codegen/oracles/elide_pattern_atlas.py \
    <tessella>/tests/golden/pattern_style.dump

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
