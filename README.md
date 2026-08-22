# tessella

**A Rust frontend for the MapLibre style spec, emitting a renderer-agnostic capture stream.**

A tessella is the small tile of a mosaic — tiles without the picture, which is the
architecture. tessella does everything a map frontend does — style parse and expression
evaluation, source and tile management, network and cache, layout and bucket generation,
glyph and sprite atlases, transform and camera, render orchestration — and then stops. It
draws nothing. What it emits is a capture stream: geometry, uniforms, textures, draw order
and camera state, in a flat envelope ABI that a renderer on the far side consumes.

That seam is the point. The frontend is GPU-free and pure Rust, so it cross-compiles to
aarch64 and riscv64 without a graphics stack, and the renderer is a swappable consumer
rather than a fused-in dependency.

> **Status: pre-R0.** This repository is a workspace scaffold and a design document. No
> crate carries an implementation yet, and the `tessella` crate is published at `0.0.0` to
> reserve the name. Read [`plan.md`](plan.md) for the actual content — it is the design of
> record, currently at rev 0.6.

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
| [`tessella-source`](crates/tessella-source) | vector / raster / GeoJSON sources, clustering |
| [`tessella-tile`](crates/tessella-tile) | pyramid, cover and retain, shared tile store |
| [`tessella-storage`](crates/tessella-storage) | online and cache file sources, request coalescing |
| [`tessella-layout`](crates/tessella-layout) | buckets: fill, line, circle, pattern; symbol shaping and quads |
| [`tessella-place`](crates/tessella-place) | collision index, cross-tile index, placement, fades |
| [`tessella-orchestrate`](crates/tessella-orchestrate) | render layers, tweakers, binders, draw order, UBO packing, damage gates |
| [`tessella-capture-abi`](crates/tessella-capture-abi) | envelope structs, ring, coalescing table, reverse channel |
| [`tessella-glyph`](crates/tessella-glyph) | glyph manager, PBF path, local SDF rasterization |

Only `tessella` publishes to crates.io today; the rest are `publish = false` until they
carry content.

## Consumers

The capture stream is consumer-neutral by construction, and two mirrors keep it honest:
a Filament-backed consumer, and impeller-rs (a pure-Rust Impeller reimplementation)
consuming at the entity/HAL level. Maps require an SSBO-capable backend — Vulkan today,
GLES 3.1+ if a consumer implements one.

## Building

```sh
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Cross-compilation lanes (`cargo check` only, no linking required):

```sh
rustup target add aarch64-unknown-linux-gnu riscv64gc-unknown-linux-gnu
cargo check --workspace --target riscv64gc-unknown-linux-gnu
```

## Relationship to MapLibre

tessella is an independent project. It implements the MapLibre style specification and
ports portions of MapLibre Native's C++ frontend; it is not affiliated with or endorsed by
the MapLibre organization, and it does not use the `mln` namespace. See [`NOTICE`](NOTICE)
for attribution.

## License

BSD-2-Clause. See [`LICENSE`](LICENSE).
