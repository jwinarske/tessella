# tessella

**A Rust frontend for the MapLibre style spec, emitting a renderer-agnostic capture stream.**

A tessella is the small tile of a mosaic — tiles without the picture, which is the
architecture. tessella does everything a map frontend does — style parse and expression
evaluation, source and tile management, network and cache, layout and bucket generation,
glyph and sprite atlases, transform and camera, symbol placement, render orchestration — and
then stops. It draws nothing. What it emits is a capture stream: geometry, uniforms, textures,
draw order and camera state, in a flat envelope ABI that a renderer on the far side consumes.

That seam is the point. The frontend is GPU-free and pure Rust, so it cross-compiles to
aarch64, riscv64 and wasm32 without a graphics stack, and the renderer is a swappable consumer
rather than a fused-in dependency.

> **Status: R0 complete, R1 substantially complete.** The stream is whole and frozen, and the
> picture is measured rather than asserted: `mbgl-render` renders a style, tessella's stream is
> rasterized through a consumer at the same camera, and the two images are diffed per pixel.
> Read [`plan.md`](plan.md) for the design of record. The `tessella` crate is still published at
> `0.0.0`: the name is reserved, the implementation lives in the `tessella-*` members, and none
> of them is published yet.

## What it draws

| layer | state |
|---|---|
| `background`, `fill`, `line`, `circle`, `raster` | complete, including patterns |
| `fill-extrusion` | complete, instanced walls over an earcut roof |
| `symbol` | text and icons: shaping, BiDi, line labels, variable anchors, collision, fades |
| `heatmap`, `hillshade`, `custom` | not implemented |

Data-driven paint binds as interleaved vertex attributes for every family, with the shader
permutation key on the wire; zoom-interpolated properties carry both endpoints and their mix
factor. Expressions are a hand port of mbgl's evaluator, checked against the upstream
expression-test suite. Sources are vector (MVT 2.1, no dependency), raster, GeoJSON with
clustering, and `.pmtiles` archives read in place.

Two things are ours and not mbgl's, and both are opt-in per §17's rule. **Globe projection**:
tiles bent onto a sphere, with the bend expanded about each tile's center so a vertex shader
adds small to small and does no trigonometry. **A shared store under many views**: buckets are
process-scoped and refcounted, so four map views cost one geometry stream plus four small view
streams.

## Architecture in one paragraph

State is process-scoped wherever it can be and per-view only where it must be. One style,
one file-source stack, one tile store, one set of buckets, one glyph atlas — shared across
every view, because buckets are camera-free and zoom interpolation lives in uniforms rather
than vertices. What is irreducibly per-view is small: the transform, cover decisions, symbol
placement, and a handful of uniform blocks. So four map views cost one geometry stream plus
four small view streams, and fetches, decodes and bucket builds stay flat in view count —
an invariant with CI counters behind it, not a hope.

Traffic is proportional to change. A parked map emits zero bytes; pure camera motion emits
a camera block; only churn emits geometry. The same discipline runs through the transport:
an SPSC ring where every coalescable envelope is an absolute state write, so a stalled
consumer bounds its own occupancy.

## Workspace

| crate | contents |
|---|---|
| [`tessella`](crates/tessella) | facade (name-reservation stub today) |
| [`tessella-style`](crates/tessella-style) | style JSON, expression evaluator, property types, transitions |
| [`tessella-source`](crates/tessella-source) | vector / raster / GeoJSON sources, clustering, image and glTF decode |
| [`tessella-tile`](crates/tessella-tile) | pyramid, cover and retain, shared tile store, camera and globe |
| [`tessella-storage`](crates/tessella-storage) | online and cache file sources, request coalescing, pmtiles |
| [`tessella-layout`](crates/tessella-layout) | buckets: fill, line, circle, extrusion, pattern; symbol shaping and quads |
| [`tessella-place`](crates/tessella-place) | collision index, cross-tile index, placement, fades |
| [`tessella-orchestrate`](crates/tessella-orchestrate) | render layers, tweakers, binders, draw order, UBO packing, damage gates |
| [`tessella-capture-abi`](crates/tessella-capture-abi) | envelope structs, ring, coalescing table, reverse channel |
| [`tessella-glyph`](crates/tessella-glyph) | glyph manager, PBF path, local SDF rasterization, sprite and dash atlases |
| [`tessella-ffi`](crates/tessella-ffi) | the C entry points a consumer embeds, as `staticlib`, `cdylib` and `rlib` |

Six of them are `#![no_std]` against `alloc`, which CI asserts by building each without `std`
rather than by trusting the marker. Only `tessella` publishes to crates.io today; the rest are
`publish = false` until they carry content.

Optional features are off by default wherever they cost binary size or a C toolchain:
`image`/`webp` (raster tiles and sprite sheets), `gltf` (3D-buildings tiles), `tls`, `cache`
(SQLite), `offline` (region downloads), `pmtiles`, and `collator` (ICU-equivalent collation
tables, 437 KB).

## Consumers

The capture stream is consumer-neutral by construction, and the mirrors keep it honest: a
Filament-backed consumer, a WebGL2 one in [`web/`](web), and `tools/capture-render`, which
rasterizes a stream on the CPU with no GPU at all. Maps require an SSBO-capable backend —
Vulkan today, GLES 3.1+ or WebGL2 if a consumer implements one.

## wasm32

The producer builds for `wasm32-unknown-unknown` and a browser page draws from the same records
and the same header as a Linux consumer. Nothing about the shared path changed to allow it: wasm
gets its own backend behind an existing seam and adds no indirection to the native one.

- **No `wasm-bindgen`.** The exports are `#[no_mangle] extern "C"`, so
  [`include/tessella_capture_abi.h`](include) stays the single description of the stream and a
  JavaScript consumer reads records out of `memory.buffer` directly. The ring *is* a
  `WebAssembly.Memory` view, so §3.5's zero-copy argument holds in a browser unrestated.
- **No sockets in the graph.** The browser fetches; `tessella_take_request` hands out what the
  producer wants and `tessella_answer` gives the bytes back. `ureq` is `cfg`'d out, and CI fails
  the build if `cargo tree` finds it.
- **No threads, no `Instant`.** The pool degrades to a single-threaded drain with a budget.
- **The cache is OPFS**, out in the page, where the real `Response` is — so the origin's own
  `Cache-Control` decides freshness rather than an expiry the producer would have to invent.

CI reads the built module rather than trusting that it compiled: `wasm-abi` asserts the exact
export set and rejects the forbidden ones, a Node consumer drives the fetch loop and reads
geometry out of linear memory, and the same consumer then runs under Firefox for the parts Node
has no `fetch` or `WebAssembly.Memory` for.

```sh
cargo build -p tessella-ffi --target wasm32-unknown-unknown --release
cargo run -p wasm-abi -- target/wasm32-unknown-unknown/release/tessella_ffi.wasm
node --test "web/test/*.test.mjs"
```

## Building

The toolchain is pinned in [`rust-toolchain.toml`](rust-toolchain.toml) to **Rust 1.94.1**,
the version Yocto wrynose (6.0) ships in oe-core. `rustup` picks it up automatically, so
there is nothing to install by hand. The pin follows the target distro rather than upstream
Rust — building against a compiler the board does not have is how MSRV surprises reach the
board instead of CI. CI carries an advisory `stable` lane as early warning for the next bump.

```sh
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

Cross-compilation lanes (`cargo check` only — nothing links, so no cross C toolchain is
needed while the workspace stays pure Rust):

```sh
cargo check --workspace --target aarch64-unknown-linux-gnu
cargo check --workspace --target riscv64gc-unknown-linux-gnu
cargo check --workspace --target wasm32-unknown-unknown
```

riscv64 is a producer, soak, and cross-compile lane only: maps require an SSBO-capable
backend, so a VisionFive 2 builds tessella but does not draw with it.

### Golden oracle

The C++ implementation this ports from is a differential-test oracle, which is the one asset
a Rust port of this kind does not usually have, and it is used at two levels.

**Records.** `mbgl-capture-probe --dump` emits a canonical serialization of the capture stream,
the Rust frontend emits the same format for the same style at the same camera, and the two are
diffed: geometry, uniform buffers, textures, clip masks, painter order, and the camera block.
Reference dumps live in [`tests/golden/`](tests/golden) so the Rust side can be checked without
a C++ build; that directory documents what is deliberately not compared, and why.

**Pixels.** Records agreeing is necessary and not sufficient — a shader reads them, and a
consumer can be wrong about what it read. So `mbgl-render` renders a style and a consumer
rasterizes tessella's stream at the same camera, and the images are diffed per channel. A pixel
counts as differing when any channel is more than 48 apart; on a style exercising every layer
family at Berlin, five cameras read:

| | z14 p0 | z14 p60 | z16 p0 | z16 p60 | z9 wide |
|---|---|---|---|---|---|
| differing pixels | 24 | 86 | 5 | 103 | 153 |
| of | 786,432 | 786,432 | 786,432 | 786,432 | 2,160,000 |

That measurement is what every rendering change in this tree is justified by, and it is what
caught the defects reasoning did not: two sign errors in the globe camera, a silently-dropped
attribute type that drew data-driven line widths at one pixel, and an icon layer that vanished
inside an improving headline number. It also has a known blind spot, which is worth stating
beside it: 48 is generous, and a low-contrast defect hides under it. A layer drawn solid where
the style asked for dashes scored 0.000% by this metric and 1.708% at a threshold of 12.

### Generated code

The mbgl-derived mirrors in `tessella-capture-abi` are generated from the pinned
maplibre-native tree (`capture-backend-phase0` @ `b237943`), never hand-edited — DR-6, because
a mirror that drifts from the C++ headers is a wrong-pixels bug that agrees with itself and so
survives every test in this workspace. The output is committed, so building tessella does not
need a C++ checkout; only regenerating does.

```sh
cargo run -p mbgl-codegen -- --mbgl /path/to/maplibre-native
cargo run -p mbgl-codegen -- --mbgl /path/to/maplibre-native --check   # is it current?
```

The flat C header a consumer mirror includes is generated from the Rust definitions and
committed to [`include/`](include). Every size, alignment, and field offset in it is taken from
the Rust types at generation time and lands as a static assertion, so a mirror built against a
stale header fails to compile rather than misreading the stream. Generating it needs no C++
checkout, so CI regenerates and compiles it on every push — as C11 and as C++17, and then
builds a C consumer that walks a real frame knowing nothing of ours but the header.

```sh
cargo run -p abi-header
cargo run -p abi-header -- --check
```

## Relationship to MapLibre

tessella is an independent project. It implements the MapLibre style specification and
ports portions of MapLibre Native's C++ frontend; it is not affiliated with or endorsed by
the MapLibre organization, and it does not use the `mln` namespace. See [`NOTICE`](NOTICE)
for attribution.

## License

BSD-2-Clause. See [`LICENSE`](LICENSE).
