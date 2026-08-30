# TESSELLA_PLAN — tessella: MapLibre-style-spec frontend in Rust, capture-stream producer

rev 0.10 — 2026-08-24
rev 0.10: R1 underway — MVT decode, the line layer, the data-driven paint binder, the shader
permutation key, composite (zoom-interpolated) binding, the line layer's uniform buffers and
the circle layer land — the hermetic style is now reproduced in full, 37 drawables and 14
uniform buffers — and the network path lands with it: URL templating, TileJSON resolution,
request coalescing and an HTTP file source, live against `tools/tile-server`; the probe gains
`--zoom` and two further goldens, one of them against a real style over real tiles; the
cold-start path is traced; DR-19 gains the line-path confirmation that the rotation is wagyu's
alone, the line buffers being byte-exact. §5.1's "camera-free bucket" is qualified: a bucket
is keyed by the zoom it is used at, because a composite property's endpoints depend on it.
rev 0.9: R0's stream complete and diffed against the probe envelope by envelope; DR-19 records
that GeoJSON polygon vertex *order* is wagyu's and is not ported, with the consequence for
§9.1's diff; §10's R0 entry carries its status and its two qualifications.
rev 0.8: DR-18 moves camera mode off ViewUse onto a dedicated ViewDeclare/ViewUndeclare pair;
§4 table, §5.3 and DR-9 amended; per-view configuration now has a home before the R0 freeze.
rev 0.7: DR-16 carried into §3.6 and §11.2, which still described the UBO floor as open;
§12.9 gains the debug-info posture; workspace scaffolded and the §16 reservation closed
(crates.io tessella 0.0.0, github.com/jwinarske/tessella), toolchain pinned to Yocto
wrynose per DR-17.
rev 0.6: DR-16 resolves R-12 (SSBO-only, Vulkan-first; GLES 3.0 composites, does not draw);
impeller mirror sequenced beside the R0 stub; §16 items closed; R0 ABI freeze unblocked.
rev 0.5: project named tessella; crate prefix mln-* → tessella-*; naming decision DR-15;
crates.io/GitHub reservation added to §16.
rev 0.4: added §3.6 (impeller-rs consumer), §11.7 (consumer obligations, both mirrors),
DR-13/DR-14, R-12; UBO-path caveat amended into §11.2; §16 second-consumer line upgraded;
Fluorite references generalized where the obligation is consumer-neutral.
rev 0.3: added §12 (producer hot paths), §13 (zoom regimes, four-view benchmark),
§5.5 (shared/irreducible ownership table), DR-11/DR-12, R-10/R-11; §9.3 counters and
R1/R1.5 exits extended; decision records/risks/open questions renumbered §14–§16.
rev 0.2: added seam-performance section, DR-9/DR-10 (camera ownership inversion, reverse
channel), R-8/R-9; CameraUpdate semantics amended per DR-9.
rev 0.1: initial.
Sources: maplibre-gl-native `capture-backend-phase0` @ b237943; fluorite-main (fluorite_ffi.h,
external_renderer_system.{h,cc}). File:line references are against those trees.

---

## 1. Purpose and scope

Replace the mbgl C++ frontend behind the capture backend with a pure-Rust implementation that
produces the same stream the Fluorite MapSystem consumes. The renderer is and remains Fluorite
(Filament); nothing below the stream boundary is in scope.

"Frontend" means everything the capture backend exercises: style parse and expression
evaluation, source/tile management, network + cache, layout/bucket generation, glyph and sprite
atlases, transform/camera, the render orchestrator (render layers, tweakers, paint-property
binders, draw order, UBO packing), and the stream emission itself.

**Non-goals (rev 0.1):** heatmap, hillshade/color-relief, terrain, location-indicator,
custom-layer/custom-drawable, annotations. Behind an explicit line until a target style demands
them. Raster and fill-extrusion are in scope but late (R3).

### 1.1 Scope reality

Deleting the renderer deletes less than intuition suggests. LOC from the branch:

| port | LOC | delete | LOC |
|---|---|---|---|
| style/ | 26,730 | gl/ + vulkan/ + mtl/ + webgpu/ | ~21,100 |
| renderer/ (minus gfx edges) | ~19,000 | shaders/ source strings | ~4,000 |
| util/ (subset) | ~6,000 | | |
| text/ | 6,396 | | |
| tile/ | 4,571 | | |
| layout/ | 3,497 | | |
| map/ | 3,685 | | |
| shaders/ UBO structs + attr tables | kept as generated data | | |

Roughly 75–80k LOC of C++ to port, plus the platform layer (run loop, file sources, sqlite
cache). Expression evaluation and the symbol pipeline (text/ + layout/ + placement) dominate;
budget symbols alone at roughly R0+R1 combined. Expect 50–60k LOC of Rust after ecosystem
reuse (§8).

### 1.2 Prior art

maplibre-rs is archived upstream as a proof-of-concept; text rendering was never completed and
its style/expression support is minimal. maplibre-native-rs is bindings over the C++ core.
There is no donor codebase. This is greenfield with crate reuse, with one asset no Rust port
usually has: a working same-protocol C++ implementation as a differential-test oracle (§9).

---

## 2. Contract: FrameDiff → envelope ABI rev 2

The port boundary is the capture stream (`include/mbgl/capture/frame_diff.hpp`). The Rust
deliverable is "a frontend that produces this stream." The stream is revised (rev 2) rather
than frozen, for three reasons: the aliasing model does not cross an ownership boundary
(§2.1), multi-view demands a geometry/view namespace split (§5.3), and damage management
demands a FrameOrder split (§6.3). The C++ `FrameSink`/`LogFrameSink` callback model survives
only in the golden-oracle probe; the production transport is the ring (§4) from day one.

### 2.1 Ownership: the aliasing model dies

Rev 1 leans on co-residency: `AttributeDesc::sharedVector` is a non-owning view into a
bucket's vertex vector; `UboUpdate::data` and `TextureUpdate::pixels` are BORROWED, valid only
for the duration of the callback (frame_diff.hpp, both documented as such). Rev 2 makes
ownership explicit:

- Bucket vertex/index data allocated in refcounted slabs (`Arc<[u8]>`-backed arenas); the
  geometry envelope carries a slab handle + offset/stride, released by the consumer's ack.
- UBO and texture bytes are copied into the ring at emit. The lifetime footnotes disappear
  from the protocol. Copy-on-emit for geometry adds is affordable because churn is
  tile-bounded, not frame-bounded — the property `AddReason` exists to police.

### 2.2 Semantics that survive verbatim

Consumer compatibility constraints; each is a protocol invariant with a test:

- **`permutationKey` + attrId→binding resolution**, including the drop-undeclared-override
  rule (`AttributeDesc::index == -1`; the LineShader floor-width case). The Rust frontend has
  no shader registry, so the per-permutation attribute tables become *data*: generated once
  from `shaders/*.hpp` and committed. Same for UBO struct layouts including
  `MLN_UBO_CONSOLIDATION` SSBO packing — `#[repr(C)]` mirrors with size/offset asserts
  generated against the C++ headers, so drift is a compile failure.
- **`declaredDataType` vs supplied type**: bind the declared type with the supplied
  offset/stride (packed min/max interpolation pairs; frame_diff.hpp AttributeDesc docs).
- **`projMatrix` is f64 column-major** (a bare `[f64; 16]`, see §8); **`centerZoom0` is
  scale-free** — the zoom-flicker regression documented in frame_diff.hpp is a named test case.
- **Stencil contract**: consumer synthesizes masks from `StencilTiles`; per-tile matrix is
  `matrixForTile`, not any content drawable's matrix; reference values are never carried.
- **`pixelsPerMeter` and the style light** travel in the camera block (§6.3).

---

## 3. Deployment shape

Pinned by the Fluorite external-renderer seam, which is consumer-side and unchanged by this
plan.

### 3.1 One DSO, two halves

Today's mirror `.so` = mbgl + capture backend + Filament-facing MapSystem. Under the port:
Rust frontend as a `staticlib` linked into the same `.so`; the C++ half reduces to the
Filament mirror. The fluorite_ffi.h rationale ("a large third-party dependency that has no
business in fluorite's build") strengthens: glslang gone (no shader compilation), harfbuzz →
rustybuzz (pure Rust), sqlite survives only as rusqlite's bundled C.

The Rust half links nothing Filament, satisfying the "must NOT link its own copy of Filament"
rule (fluorite_ffi.h, external-renderers section) structurally. Only the C++ mirror half
touches the re-exported Filament symbols. The internal boundary between halves is the envelope
ABI — one flat C header, single source of truth, shared with the mirror.

### 3.2 Tick model

`ExternalRendererSystem` delivers ticks from inside the ECS update loop on the Filament API
thread (external_renderer_system.h:50-55) — a pull model. The Rust map runtime is a
free-running producer; the tick drains the ring. One tick draining N producer frames is normal
and correct: the mirror only ever wants the newest camera/order state (§4).

### 3.3 Lifecycle / teardown protocol

Teardown runs synchronously on the Filament thread; contract is "drop Filament objects before
returning" (`fluorite_external_teardown_fn` docs). Order in teardown:

1. Signal the Rust runtime to stop — non-blocking: close file sources, wake the run-loop
   equivalent. Rust holds no Filament objects, so nothing on that side blocks the contract.
2. Destroy the mirror's Filament resources.
3. Join Rust threads (after step 2; joining first risks stalling the Filament thread behind an
   in-flight fetch).

The ring allocation belongs to the `user` object, whose lifetime the registration API already
governs ("must not destroy the object its user pointer refers to while that tick is still on
the stack", external_renderer_system.h:83-85). `register` returning 0 after engine teardown
(tornDown_ gate, external_renderer_system.cc) is terminal, not retryable.

### 3.4 Camera and stencil, consumer-side

Unchanged: `fluorite_get_filament_view` slot with identity custom projection, no ECS Camera
bound to the driven view, `View::setStencilBufferEnabled` opt-in. The port keeps emitting the
same doubles.

### 3.5 Latent option: process isolation

The frontend's only process coupling is the ring, so promoting staticlib-in-mirror to its own
process (ring over shm) is a linker change, not a redesign. Lines up with the bubblewrap/T2
sandbox direction if network-facing tile decode ever wants isolation. Not in scope; recorded
so nothing in the ABI precludes it (no in-process pointers in envelopes — slab handles are
offsets, §2.1 already guarantees this).

**Spiked at R4** (`process_isolation`). The producer maps a file shared and calls `ring::init`
over the mapping; the C consumer of §9 maps the same file, attaches, and reads the stream while
the producer is still writing it. The claim holds: no envelope needed changing, and the linker
change is the whole of it. Two things only a second process could show.

- **`tail` had never been published.** Every in-process consumer drains a finished buffer and is
  thrown away, so the counter the producer reads to know what it may overwrite was write-only in
  practice. Against a live producer that is a stall on the first full ring. The spike's ring is
  deliberately smaller than the frames going through it, so the producer makes progress only
  because the other process publishes — and the test asserts it was forced to wait, because a run
  where nothing filled up proves nothing.
- **A full ring and a ring too small for a frame are the same `Full`.** The first clears when the
  consumer catches up; the second never does. A producer that retries without a deadline turns
  the second into a hang, which across a process boundary is indistinguishable from a consumer
  that died. Both sides of the distinction are now tested.

**The gap it left, since closed.** Geometry bytes reached a consumer through a region packed by
`SlabArena::pack`, which ran *after* the frame naming them was on the ring — no window in
process, where the arena is the same object on both sides, but across a mapping a consumer could
hold a `GeometryAdd` whose handle the region did not yet cover. §11.3's `SlabArena::in_region`
closes it: the producer allocates out of the shared region, and the test's consumer resolves
every handle it meets from the other process.

### 3.6 Second consumer: impeller-rs (DR-14)

impeller-rs (pure-Rust Impeller reimplementation: canvas/recording over an entity layer over
Vulkan + GLES 3.0 HALs, with WSI and DRM/KMS direct-scanout presentation) is the second
consumer — not a null mirror but a shippable one, covering product shapes Fluorite is heavy
for: pure-2D cluster maps and direct scanout on a leased DRM connector with no compositor.
Both run on the Vulkan HAL; DR-16 puts GLES-only silicon outside the map-drawing set. The
producer is untouched; this section fixes the integration layer.

- **Entity/HAL level, never canvas level.** The canvas `Vertices` model (positions + colors +
  texcoords, paint materials) cannot express custom attribute layouts or `_t`-uniform zoom
  interpolation; consuming there forces per-frame vertex-color rewrites — the
  AttributesModified storm the damage model forbids, killing the §13.1 invariant. Canvas is
  for compositing the map *result*. The map draws through a `MapContents`/dedicated pass at
  the entity/HAL layer, with the mbgl shader family ported into impeller-shaders as another
  AOT pipeline set (matching its no-runtime-compilation rule).
- **Stencil**: `StencilTiles` → tile quad × carried matrix through the clip machinery or an
  owned stencil sub-pass inside the map pass.
- **Text seam**: impeller-text packs caller-supplied coverage and does not rasterize;
  tessella-glyph rasterizes SDF coverage and owns the shared atlas. Either feed impeller-text or
  draw textured quads from the map atlas — the division of labor matches from both sides.
- **Tick analog**: the registered frame callback before Recording build; drain ring → build
  command set → submit. The record-and-replay GLES backend wanting the whole scene matches
  the drain-then-build shape.
- **In-process Rust elision**: same ABI, but a Rust consumer holds slab `Arc`s directly —
  geometry "copy" degenerates to a refcount bump. Not a second transport; the ring is
  unchanged for Fluorite and for process isolation (§3.5).
- **Hardware matrix effect**: the mirror exercises the Vulkan HAL only (DR-16). The GLES 3.0
  HAL composites a map result but cannot draw one — it has no SSBO — so it does not widen the
  *rendering* matrix. VisionFive 2 stays producer, soak, and cross-compile only, and joins the
  rendering matrix if and when the Mesa pvr Vulkan driver matures. The frontend was always
  GPU-free, so nothing about that costs this design anything either way.

---

## 4. Transport: SPSC ring, coalescing table (normative)

Producer = Rust map/orchestrator thread. Consumer = Filament tick. Flat C envelope discipline
(same posture as ihs_steam / ihs_mcp rings).

| envelope | policy | key | notes |
|---|---|---|---|
| GeometryAdd / GeometryRemove | lossless, in order | — | backpressure blocks producer; ring sized for worst-case tile turnover |
| ViewDeclare / ViewUndeclare (§5.3) | lossless, in order | — | must precede any ViewUse naming the view |
| ViewUse / ViewRelease (§5.3) | lossless, in order | — | small |
| UboUpdate | latest-wins coalesce | (viewId or shared, layerIndex/ownerId, slot) | absolute writes, so latest-wins is exact; bounds occupancy under consumer stall |
| TextureUpdate | rect-list merge, spill to union | textureId | ordered within a texture; §6.4 |
| CameraUpdate | latest-wins | viewId | §6.3 |
| OrderUpdate | latest-wins | viewId | carries order-epoch; camera references epoch; consumer never applies a camera against a stale order it hasn't received — epoch mismatch ⇒ hold camera until order arrives |
| StencilTiles | latest-wins | (viewId, layerIndex) | emitted on change only |

Damage is a ring property, not just an emission property: coalescing is what keeps a stalled
consumer from unbounded occupancy, and latest-wins is only correct because every coalescable
envelope is an absolute state write.

---

## 5. Multi-view architecture

Multi-instance is what mbgl structurally cannot do: every `Map` owns its own style, tile
pyramid, file sources, atlases, workers. N views = N fetches, N decodes, N bucket builds, N
atlases. Rev 1's `TextureUpdate::contentHash` exists to let the *consumer* dedup after the
fact — compensating downstream for producer-side ownership. This plan puts ownership at the
right level from R0. **The shared-store model, the namespace split, and the single
orchestrator are R0 architecture even while R0 runs one view; retrofitting sharing into a
per-map design is the mbgl mistake being escaped.**

### 5.1 Process-scoped (shared) state

- **Style**: immutable after parse/compile; views hold an `Arc`. Mutation = new revision;
  views repoint.
- **Network + cache**: one file-source stack, one sqlite/mbtiles cache, request coalescing —
  two views wanting the same tile produce one in-flight fetch with two waiters. Same for glyph
  and sprite PBFs.
- **Tile store**: keyed `(source, OverscaledTileID, styleRev)`, refcounted retain; a view's
  cover is a set of handles. LRU sized once per process.
- **Buckets + symbol layout**: functions of (tile, layer, tile zoom), camera-free. Shareable
  because zoom interpolation lives in `_t` uniforms, not vertices (the packed min/max design
  documented at `AttributeDesc::declaredDataType`) — shared vertices serve views at different
  fractional zooms of one tile level.
- **Atlases**: one glyph atlas per fontstack, one sprite atlas per style, emitted once.
  `contentHash` is retired from the protocol (debug-build stream invariant only).

### 5.2 Per-view state (irreducible)

Transform/camera; tile cover + retain decisions; symbol **placement** (collision index,
opacity fades, cross-tile dedup — all functions of bearing/pitch/zoom); global paint-params
UBOs; CameraUpdate/OrderUpdate; StencilTiles. Placement is the per-view cost center: pace it
per view (primary display tight interval; cluster/inset views lazy).

### 5.3 ABI consequence: geometry/view namespace split

`DrawableAdd` splits:

- **GeometryAdd** — process-scoped, refcounted: shared geometry id, attrs, indexes, segments,
  textureRefs, shader identity (builtin + permutationKey), vertexCount. Removed when the last
  view releases.
- **ViewDeclare / ViewUndeclare** — per-view configuration, independent of any geometry:
  camera mode (DR-9), and reserved space for the view class and `maxzoom` clamp §5.4 wants.
  A `ViewUse` naming an undeclared view is a protocol fault (DR-18).
- **ViewUse** — per-view: (viewId, geometryId, layerIndex, subLayerIndex, renderPass flags,
  tileID). ViewRelease drops it.

`MapID` becomes `viewId` and remains on everything camera-scoped. Consolidated-SSBO UBO
traffic keys by (viewId, layerIndex) — `uboIndex` assignment is per view's draw order, exactly
as rev 1's DrawOrderEntry note says (fill_layer_tweaker.cpp:245 reassigns per pass).

Consumer effect: one renderable in multiple `Scene`s; one `View` per map via the existing view
slots. VRAM and upload bandwidth scale with unique tiles, not views.

This said "one Filament VertexBuffer/IndexBuffer per shared geometry", and DR-21 changes it: a
buffer is a *slab* and a geometry is a sub-range of one, because one draw call reads one vertex
buffer and a layer's tiles have to share one to batch. The refcount-and-release model above is
untouched — only the granularity of the buffer moves, and `GeometryRemove` still means what it
said.

### 5.4 Scheduling

One orchestrator ticking all views, not N map threads: coherent wakeups; one pass computing
every view's cover against the shared store; one decode/layout worker pool (dedicated, not a
global pool) with priority classes — foreground visible-tile decode > background view >
prefetch. Per-view tile budgets and a per-view `maxzoom` clamp (a 200 px cluster inset never
needs z16) bound worst-case memory on RK3566-class targets. Prefetch along camera velocity is
a speculative cover at the lowest priority class once cover computation is centralized.

### 5.5 Ownership table: shared vs irreducibly per-view (normative)

Process-scoped — sharing enforced by the §9.3 flatness counters; anything below appearing to
scale with view count is a bug:

| owner: process | notes |
|---|---|
| style (compiled), expression endpoints per (layer, zoom interval) | §12.1 |
| file sources, request coalescing, cache revalidation/expiry | once per tile, never per view |
| tile store + unified retain chains | adjacent-zoom views share pyramids: one view's active tiles are another's retained ancestors (§13.2) |
| buckets, symbol layout, shaping cache, glyph-SDF cache | keyed (fontstack, text, params) |
| glyph/sprite atlases, unique tile/atlas Filament Textures | one Texture per unique content, any number of scenes |
| compiled Filament materials per shader-permutation family | per-(view,layer) is a MaterialInstance over that view's SSBO — never per-drawable, never per-view materials |
| worker pool, orchestrator, deadline wheel (all timers: placement ×N, fades, expiry, pre-warm) | one wheel; N timer sets is wakeup scatter |

Irreducibly per-view — listed so nobody "optimizes" them into incorrect sharing: transform;
cover decisions; placement + collision + fades; global paint UBOs; CameraUpdate/OrderUpdate;
StencilTiles; screen-space UBO variants (R-2).

Four-view sizing: affinity is a policy evaluated against the part rather than a fact written
here — see below; ring sized for a four-view simultaneous integer crossing at worst-case tile
counts; per-view maxzoom clamps by view class (a cluster inset capped at z14 never joins a z16
crossing burst).

**Core affinity is queried, not prescribed** (`orchestrate::topology`, and it closes §16's
"explicit pinning vs scheduler hints, per target"). This section used to read "decode workers
pinned to little cores, big cores for orchestrator + Filament", and R1's measurement had to
correct it: an RK3566 is four Cortex-A55s in one cluster and has no big cores, so the sentence
described an RK3588 and not the board. The correction is not a different sentence. A frontend
that runs on an RK3566, an RK3588, a VisionFive 2 and a workstation cannot hold a right answer
about cores, and every one of those parts already reports what it is.

The number is the kernel's own: `cpu_capacity`, out of 1024, derived on arm64 from the device
tree's `capacity-dmips-mhz` and present exactly where capacity-aware scheduling is. Asking the
same source the scheduler asks is the difference between a policy that agrees with it and one
that fights it. Where it is absent — x86, hybrid parts included — `cpufreq/cpuinfo_max_freq`
stands in, normalised so the largest core is 1024; a worse measure, since frequency is not
throughput across microarchitectures, but it separates the tiers it has to. The two are never
mixed: a part answering one for some cores and the other for the rest would put them on
incomparable scales and the tiers would be an artefact of which file existed.

`Affinity` is then a preference with two answers, defaulting to leaving the scheduler alone —
which is what a capacity-aware scheduler deserves, and pinning against one is how a decode
worker ends up queued behind another on a small core while a large one idles. `SpareTheLargest`
is §5.4's old intent stated as a policy: everything below the top tier is decode's, the top is
the orchestrator's and the renderer's, and on a part with one tier it asks for nothing at all.
That last case is the RK3566, and it is the whole of the correction — reached by measurement
rather than by assertion.

Two things it deliberately does not do. It opens no file: this crate is `no_std` and has no
business growing an I/O dependency for four reads, so every path and every parse is here and the
caller supplies the bytes — which is also what makes an RK3566, an RK3588, an Intel hybrid and a
uniform server testable without owning one. And it pins nothing: applying an affinity is
`sched_setaffinity`, a syscall, and this crate is `deny(unsafe_code)` with no allowance and no
libc. The policy says which CPUs a class of work wants; the embedder, which already owns thread
creation, is where that becomes a call.

The worker *count* is not derived from any of this and deliberately: `Workers::DEFAULT` stays a
constant for the reason it always was — a number derived from the host makes a measurement on a
workstation say nothing about the device.

---

## 6. Damage management

Goal: traffic proportional to change. Static camera + no churn ⇒ **zero ring bytes**; pure
camera motion ⇒ camera-block bytes only; churn ⇒ churn-proportional bytes. These are normative
statements with counters (§9.3), not aspirations.

### 6.1 Mechanics already in the C++ backend — port verbatim

- **UBO byte-compare suppression**: `uniform_buffer.cpp:31` memcmps before dirtying;
  identical tweaker rewrites die at the source. This is what makes UboUpdate "dirty-only"
  true.
- **Texture dirty-rect union + per-frame flush batching**: `texture2d.cpp:106-122` unions
  sub-region uploads; `context.cpp:64-71` flushes once per frame so glyph-insert storms don't
  hash the atlas per glyph.
- **AddReason churn taxonomy** — a steady `AttributesModified` stream on a static scene is a
  visible bug.

### 6.2 Gaps in rev 1

- `FrameOrder` is emitted unconditionally every frame (`context.cpp:73`), even byte-identical,
  and it is the largest per-frame payload (thousands of 32-byte entries at frame rate).
- It conflates two change cadences: camera (every frame while moving) and painter order
  (changes only on tile/layer churn or sort-key change; a pure pan/zoom reorders nothing).
- Union dirty-rect over-uploads when small updates land in opposite atlas corners.
- `contentHash` is O(texture bytes) per flush, existing only for cross-instance dedup the
  shared-atlas model obsoletes.
- No protocol-level still-frame guarantee.

### 6.3 FrameOrder split (rev 2)

- **CameraUpdate** (per view): projMatrix, centerZoom0, bearing, pitch, pixelsPerMeter, light,
  frameNo, opaquePassCutoff, depthRangeSize, **orderEpoch**. Emitted only when any field
  changes (f64 exact compare; the values are deterministic functions of the transform, so
  equality is meaningful). Latest-wins in the ring.
- **OrderUpdate** (per view): the ordered entry list + new orderEpoch. Emitted only when the
  list differs from the last emitted list (cheap: hash of (id, pass, layer, subLayer,
  drawPriority, uboIndex) tuples, compared before serialization).
- Consistency: consumer applies a CameraUpdate only when it holds the referenced orderEpoch;
  otherwise holds until the OrderUpdate drains (§4).

Effect: steady-state pan traffic drops from ~100 KB/frame to ~hundreds of bytes/frame; parked
map drops to zero.

Amended by DR-9: CameraUpdate as described is the **producer-camera mode**, used for
non-interactive views. Interactive views run **consumer-camera mode** (§11.1), where the
Fluorite ECS camera is authoritative, CameraUpdate degrades to the non-matrix fields
(pixelsPerMeter, light, opaquePassCutoff, depthRangeSize, orderEpoch), and the producer reads
the camera back over the reverse channel (§11.4). The mode is per view, declared at
ViewDeclare (DR-18).

### 6.4 Texture damage (rev 2)

Small dirty-rect **list** per texture (cap ≈ 4 rects; spill to union). Maps directly onto
per-region uploads consumer-side and stops the opposite-corners pathology. Atlas shelf
allocator on the Rust side keeps insertions clustered so the list rarely spills.

### 6.5 Still-frame guarantee

The orchestrator does not run a frame for a view whose transform is unchanged and whose
sources report no churn (mbgl's upstream repaint gating, made a per-view protocol guarantee).
Placement fade animations count as churn while fading, then settle to silence.

### 6.6 Multi-view damage scoping

Falls out of §5.3: geometry/texture traffic is emitted once regardless of view count; per-view
traffic is camera + order + global UBOs + stencil sets. N views cost one geometry stream plus
N small view streams.

---

## 7. Crate map

Workspace, pure Rust, no C++ bindings (aarch64/riscv64 cross under emb manifests stays
trivial). No async runtime: mirror mbgl's actor model with threads + channels, preserving the
"all emission on the map/orchestrator thread" invariant.

| crate | contents | descends from |
|---|---|---|
| tessella-style | style JSON (serde), expression evaluator, property types, transitions | style/ |
| tessella-source | vector/raster/geojson (+clustering) sources | style/sources, renderer/sources |
| tessella-tile | pyramid, cover/retain (update_renderables), shared tile store (§5.1) | tile/, algorithm/ |
| tessella-storage | online + cache file sources, request coalescing | storage/, platform default |
| tessella-layout | buckets: fill (earcut), line join/cap, circle, pattern; symbol shaping/quads | layout/, text/ (layout half) |
| tessella-place | collision index, cross-tile index, placement, fades — per view | text/ (placement half) |
| tessella-orchestrate | render layers, tweakers, binders, order, UBO packing, damage gates (§6) | renderer/ |
| tessella-capture-abi | envelope structs (flat C header, shared with mirror), ring, coalescing | capture/ |
| tessella-glyph | glyph manager, PBF path, local SDF rasterization | text/ (glyph half), sprite/ |

## 8. Ecosystem reuse

| need | crate | replaces |
|---|---|---|
| fill tessellation | earcutr | earcut.hpp (same algorithm; output ordering matters for §9) |
| MVT decode | hand-rolled zero-copy varint reader (protozero style); geozero acceptable interim | protozero |
| geojson + clustering | geojson, geo-types, supercluster port | mapbox geojson/supercluster |
| color | csscolorparser | vendored csscolorparser |
| BiDi | unicode-bidi | ubidi/ICU |
| shaping | rustybuzz + unicode-linebreak | harfbuzz |
| local glyph SDF | sdf_glyph_renderer-style + fontdue/ab_glyph | TinySDF/freetype path |
| PNG/JPEG decode | zune-png + zune-jpeg, behind an off-by-default `image` feature (DR-20, §12.2) | mbgl's png/jpeg decoders |
| WebP decode | image-webp, behind an off-by-default `webp` feature above `image` (DR-20) | libwebp |
| cache DB | rusqlite (bundled) | sqlite vendored |
| HTTP | ureq (blocking, on workers) | cpp-httplib/curl |
| f64 math | hand-written `[f64; 16]`; see below | mbgl matrix |
| ring/sync | crossbeam (or hand SPSC matching ihs ring ABI) | — |

Expressions have no crate; hand port. Symbol placement has no crate; hand port.

`glam` was listed here for `DMat4`/`DVec` and is not used. The matrices are hand-written
`[f64; 16]` instead, because the order the terms accumulate *is* the quantity being reproduced:
every golden diff is byte-exact over matrices these produce, and a library's multiply — however
mathematically identical — is free to associate differently and move the last bit. The same
reasoning rules out a faster transcendental library for `tessella-tile`, which links the *system*
libm precisely because that is what the C++ oracle links against.

It is not a performance trade being lost. The transcendentals are per view per frame rather than
per vertex — thirty of the thirty-eight call sites are in the camera, the projection and the
cover — against a measured per-frame producer cost of 1.5 to 5.1 ms that §13.3 attributes to
cover, clip masks, drawable matrices and uniform writes. Rust does not fuse multiply-add without
being asked, so the scalar path is already IEEE-exact; a NEON version that *did* fuse would round
differently and break the diff, and one that did not would be register-bound on a 4x4 of `f64`.
Revisit against a profile on the §13.3 rig rather than against intuition.

---

## 9. Testing

### 9.1 Golden oracle (single view)

`mbgl-capture-probe` runs the hermetic inline style with no network and reports the stream —
extend it with `--dump`: deterministic serialization (sorted by id; pointers → content
hashes). The Rust frontend runs the same style at the same camera; normalized streams diff.
Covers drawable counts, attribute descriptors (attrId, index, both data types,
offset/stride), permutation keys, segment tables, index contents, UBO bytes, draw order.
Converts "does the expression evaluator round identically" from archaeology into a failing
diff. Named regression: centerZoom0 scale-freeness under zoom animation (frame_diff.hpp
historic note).

### 9.2 Multi-view invariants (Rust-native; the C++ probe cannot oracle rev 2)

- Per-view stream ≡ a single-view run at the same camera, modulo the geometry namespace.
  Asserted in `view_independence.rs`: a view's bindings — layer, sublayer, tile, pass, flags and
  their order — are identical whether it runs alone or among four, against a group chosen to mix
  exact overlap, partial overlap and disjointness. Geometry ids are renumbered by first
  appearance before comparing, since they are handed out process-wide and comparing them raw
  would assert the allocation order rather than the invariant. Checked for a view that is not the
  first built, which is the case a shared counter makes visible, and symmetrically for two views
  at one camera.
- Shared-store counters (fetches, decodes, bucket builds, atlas uploads) do not scale with
  view count for overlapping covers.
- Screen-space UBO variants (R-2) differ per view over identical shared geometry. Asserted in
  `view_uniforms.rs`, and in both halves at once, since either alone is a property a wrong
  implementation also has: over a tile four views share, the buckets are byte-identical while
  the four drawable matrices are all distinct. "Uniforms differ" alone is satisfied by sharing
  nothing, which is the arrangement §5 exists to escape. The converse is asserted too — two
  views at one camera agree — without which the test would pass for a matrix that depended on a
  view's *identity* rather than its camera. Held at every frame of the §13.3 sweep and not only
  at its ends, since the views converge as it descends and convergence is where a shared uniform
  stops being visible. The frame-wide block is checked against a scaled inset as well as a
  reshaped one: 320x240 beside 1024x768 is the same 4:3, so a block distinguished by aspect
  ratio alone would size an inset's geometry to the display. Stencil matrices are covered
  separately, because they are deliberately not the drawable's — a mask left on a neighbour's
  camera subtracts geometry rather than misplacing it. Each of the three paths was checked by
  pinning it to one canonical camera and confirming that its own test, and only its own, fails.

### 9.3 Counters (CI assertions)

Extend the LogFrameSink-stats pattern: bytes/frame parked == 0; bytes/frame during pure pan ≤
camera-block budget — asserted in `parked_is_silent.rs` as an *identity* rather than a bound:
forty frames of a pan that provably does not move the cover each cost exactly one camera block
and want no geometry. A bound is satisfied by a producer that has started sending something
small every frame it did not send before; OrderUpdate count == order-change count (asserted in `draw_order.rs`: fifty frames of an
unchanged order move the ring not at all, three successive changes emit once each and settle
immediately after, and rebuilding an identical order from scratch is not a change — the
suppression compares the resolved bytes rather than tracking whether anyone called `bind`); AttributesModified == 0 on a
static scene; dirty-rect coverage ratio (uploaded px / changed px) bounded. Zoom (§13.1): zero
geometry envelopes and zero AttributesModified during non-crossing zoom. Flatness (§5.5):
fetches, decodes, bucket builds, shaped labels, atlas uploads, material compilations flat in
view count for overlapping covers. Coverage completeness: zero uncovered viewport frames
across the §13.3 sweep. Pre-warm: warmed-but-unused ratio within budget (R-10).

---

## 10. Phasing

- **R0** — *stream complete; exit met with the two qualifications in DR-19 and below.* Every
  envelope kind is emitted and diffed against the probe on the hermetic style: geometry,
  `ViewDeclare`/`ViewUse`/`ViewRelease`, `UboUpdate` (all six buffers, byte-exact),
  `TextureUpdate`, `StencilTiles` (matrix hashes), `OrderUpdate` (painter order element for
  element) and `CameraUpdate` (all sixteen projection elements plus light and centre, bit-exact).
  Parked bytes are zero over five hundred settled frames. Qualification: GeoJSON polygon vertex
  *order* is a rotation of the oracle's, which DR-19 explains and declines to chase.
  The second qualification is discharged. `proj_matrix` refused bearing and pitch, so the
  quaternion path was waiting for a capture that does not exist; it carries them now, and the
  cover walks a frustum for them. What replaced the missing capture was not a capture: the
  unrotated path is unchanged bit for bit — the orientation is the identity at zero, so every
  golden still holds — and the rotated path is checked by properties a perspective must have,
  and by unprojecting the screen and asking the cover to contain what it lands on. Two faults
  came out of that, both invisible at zero pitch and both recorded under R3.
  Mirrors C++ Phase 0: style parse, inline GeoJSON, background/fill buckets,
  orchestrator skeleton, ring transport, damage gates (§6.3/§6.5 — cheap now, expensive
  later), shared-store ownership + namespace split (architecture only; one view), DR-9 camera modes
  and the DR-10 reverse channel in the ABI (consumer-camera exercised by a stub mirror);
  ABI freeze at R0 exit — DR-16 removed its last blocker, and what freezes is envelope/ring
  struct shape, atomics, mode-bit positions, and conventions (field additions to existing
  envelopes remain open for R2). Exit:
  stream matches the probe on the hermetic style; parked bytes == 0.
- **R1** — *in progress.* Vector tiles decode and tessellate (MVT 2.1, no dependency); the line
  layer is byte-exact against the oracle, six tiles of six, vertices and indices; data-driven
  paint binds into the interleaved per-layer buffer, byte-exact across all twelve of the golden
  dump's paint buffers; the shader permutation key is on the wire and groups as the oracle's
  does; zoom-interpolated (composite) properties carry both endpoints and their `_t` mix factor,
  byte-exact against a second golden captured at a fractional zoom; the line layer's uniform
  buffers land; the circle layer closes the hermetic style, which this build now reproduces in
  full — all 37 drawables and all 14 uniform buffers. Tiles now come off a socket: URL
  templating, TileJSON resolution, request coalescing and an HTTP file source, tested end to
  end against `tools/tile-server` on a loopback port, and opt-in against a `pmtiles serve`
  origin over real Protomaps planet extracts. An SQLite response cache with etag revalidation
  lands behind an off-by-default feature, since `rusqlite` bundles C the cross lane has no
  toolchain for, and composes into the cold start: against a Protomaps extract a warm start
  reaches first geometry in 0.4 ms and completes in 2.0 ms against 3.8/6.6 ms cold, with zero
  round trips against ten.
  The cache is bounded by bytes and evicts least-recently-used on every write.
  Offline regions land on top of it: a user picks a box and a zoom range, is shown what it
  costs, and accepts or declines. `Region::tile_count` closes a formula and never allocates —
  sizing a country is asked precisely so it can be refused, and answering by building the list
  would make asking as expensive as agreeing. `Download::plan` turns the style into URLs and
  `Download::run` fetches them, so the list shown is the list paid. A region's resources sit
  outside the ambient bound in both directions and outside its freshness rules: a downloaded
  tile is served without asking the origin however old it gets, because the user paid for a
  snapshot, and deferring to `Cache-Control` there would blank the map offline and put a metered
  user back on the network online. The exclusion is a count on the row, not a join — measured,
  `NOT IN` against the claim table cost 238 us per ambient write at zero claims and 33 ms at a
  hundred thousand, so a finished download taxed every tile fetched afterwards; the count is
  flat at 150 us. Downloads are resumable rather than transactional, since a country at street
  zoom is hours over a connection that will drop.
  §12.5's first piece lands: a style's sources resolve together rather than one after another.
  What the trace says afterwards, on loopback with a real zoom-10 tile: parse 26 µs, sources
  +10 µs, cover +15 µs, first fetch +1.14 ms, first bucket +0.82 ms, complete +1.03 ms — about
  3 ms cold and 40 µs warm. Style parse and paint resolution together are under two per cent of
  that, so §12.5's compiled-style cache is not worth building yet: it would save a fraction of a
  fiftieth. Against a real link the picture differs in one place that matters — the manifest
  round trip is 1–3 ms and sits alone in front of everything, which is what speculative fetch
  exists to hide and what a single-source style gives it nothing to hide behind. A
  source given by TileJSON URL costs a round trip to learn what it offers, and those sat in
  sequence in front of the first tile request — four sources on a link where a round trip is
  40 ms was 160 ms of a cold start spent finding out what to ask for. They do not depend on each
  other, so they go to the pool as one batch.
  TLS lands as `tessella-storage/tls`, off by default. The premise for holding it — that a
  transitive C dependency would break the cross lane for every crate — turned out not to need
  the toolchains after all: that lane checks the workspace with *default* features, `http` was
  already outside the default set, and a feature nobody enables costs it nothing. Verified rather
  than assumed: both cross targets check clean and `cargo tree` finds neither `ring` nor `rustls`
  in either. Without the feature an `https://` URL is refused at the transport rather than
  falling back to plaintext, which is asserted in both feature states — a tile request over a
  connection nobody agreed to leaks a user's position to anyone on the path, so refusing is the
  only safe way not to support something.
  `.pmtiles` archives read directly too, as `tessella-storage/pmtiles`: header, Hilbert tile ids,
  the varint directory format and leaf-directory descent, over a `RangeReader` so the same code
  serves a file today and §12.6's HTTP range requests later, and behind a `FileSource` so a
  style names one with `"url": "pmtiles:///data/planet.pmtiles"` and nothing above
  `tileset::resolve` learns a second shape. The manifest is synthesised from the header and the
  metadata document the way mbgl's `request_tilejson` does, so an archive needs no `.json`
  beside it. A `Router` dispatches by URL — mbgl's `MainResourceLoader` asking each source
  `canRequest` — which is what lets one style take its tiles from an archive and its glyphs from
  an origin. Nothing in it needs C — `flate2`
  defaults to `miniz_oxide` — so §16's toolchain question never applied; §16 itself says "cheap
  in Rust", and R1's line had borrowed TLS's reasoning by proximity. An embedded target with a
  region on local storage now reads it rather than running a web server against itself to fetch
  from localhost. Checked against the reference implementation the way everything else here is:
  six tiles spanning zoom 0 to 15, byte-identical to what `pmtiles serve` returns for the same
  archive, including the deep ones that are only reachable by following a leaf pointer.
  The worker-count budget is taken on an RK3566 (Radxa Zero 3, quad Cortex-A55 at 1.8 GHz,
  Debian bookworm), cross-built against that distribution's glibc rather than the host's, since
  the workstation's is newer and its binaries will not load there. Two things it settled and one
  it corrected.
  What it corrected first: the benchmark's own baseline. `Workers::new(1)` is not a serial run —
  `Batch::wait` makes the submitting thread help rather than idle, so "one worker" occupies 1.8
  cores. Measured against that, a perfectly linear pool reports about half of linear, which
  reads exactly like a lock somewhere. Every table now reports *cores busy* — process CPU time
  over wall time — beside its ratio, and the serial baseline runs the jobs inline with no pool
  at all. With that fixed, speedup tracks cores busy almost exactly (2.95× at 2.99 cores, 2.90×
  at 2.95 cores on pure arithmetic), which is the statement that there is no serialization; it
  is also robust to whatever else is on the machine, since both numbers move together.
  What it settled: `Workers::DEFAULT` of 4 stands. On an idle board, nine real z5 tiles decode
  and build in 54.5 ms inline, 30.9 ms on one worker, 20.6 ms on two and 18.5 ms on four — where
  it stops. Six and eight add nothing. Pure arithmetic on the same pool reaches 3.86× at 3.90
  cores busy, so the pool itself is linear to within the measurement; the decode table's 2.95× at
  3.79 cores is the gap unevenness and allocation leave. A nine-tile z5 cover spans 301 bytes to
  146 KB, and completion is bounded below by the largest tile however many workers there are —
  33% of the cover's bytes. A cover of nine *identical* tiles keeps scaling where the real one
  stops, which is how the two were told apart. Cold start on the board, four workers: parse
  120 µs, sources 2.67 ms, cover 2.71 ms, first fetch 5.61 ms, first bucket 6.07 ms, complete
  34.5 ms.
  Board measurements are worth only the quiet they were taken in: a first pass reported
  1.7–2.1× where an idle board reports 2.95–3.86×, because another project's test suite held two
  of the four cores. The cores-busy column is what makes that detectable rather than merely
  disappointing, since a ratio against a contended baseline still looks plausible.
  And a correction to §5.4: this SoC has no big cores. RK3566 is four A55s in one cluster, so
  "decode workers on the little cores, big cores for orchestrator and Filament" describes an
  RK3588 and not this board. On a homogeneous quad there is nothing to pin *to*; the bounded
  constant still matters, for memory rather than for placement.
  Remaining: §12.6's connection reuse and session resumption, which are properties of how the
  agent is pooled rather than of whether TLS is compiled in and want measuring over a real link.

  Three things this list used to carry, and why they are not on it. **Cross-faded (pattern)
  binders** are blocked rather than deferred: no golden carries a pattern layer until R3 brings
  the textures, so writing the binder now means writing it against nothing to diff it with — and
  every binder that is right is right because the oracle said so. **DR-11's bytecode VM** is
  decided, not pending: it was built, measured slower than the walk it replaced, and reverted,
  with §12.1 recording that a compact `Copy` runtime value has to come first. **§12.5's startup
  path** is done as far as it goes before symbols: sources resolve in parallel, and the trace
  says style parse and paint resolution together are under two per cent of a cold start, so the
  compiled-style cache would save a fraction of a fiftieth. What remains of §12.5 is the
  speculative **sprite** fetch, which now happens: it goes in beside the source manifests, which
  is §12.5's "issued the moment sources parse". It is the one of that section's three fetches
  that can genuinely go that early, because a sprite is addressed by the style alone and nothing
  it needs is in a manifest. Tiles cannot — the manifest carries their templates — and that
  asymmetry is the reason to issue the sprite early rather than to have a uniform rule.
  A sheet that does not answer costs the icons and not the map: every other layer draws and
  `Boot::sprites` is `None`, which is also what a style with no sprite gives, with the trace
  telling the two apart. The **glyph** half is still open and differs in kind — the stacks are in
  the style but the ranges depend on what the tiles say, so fetching any is a guess at which, and
  a guess wants R-10's warmed-but-unused counter beside it rather than a bare fetch.
  A region's area is a box or a shape. The shape path is a port of mbgl's `util::TileCover`
  scanline, checked against mbgl's own expectations — the exact 424-tile multipolygon, the
  punched hole at 8/136/87, the six-tile San Francisco outline — and against
  [`Bounds`] over six boxes and eleven zooms, since a rectangle spelled as a polygon must cover
  exactly what the rectangle does. One deliberate divergence: a chain with no vertical extent is
  dropped rather than kept as a bound. mbgl keeps them, and two axis-aligned parts at the same
  latitudes then get a full-width bound each in their shared top row, the winding never returns
  to zero between them, and the gap fills in — two selected cities download the ocean between
  them. Every mbgl expectation still passes with the chains dropped.
  `SqliteCache::pack` returns freed space to the filesystem, since SQLite never shrinks a file
  on its own and a user who deletes a download to make room would otherwise find they had not
  made any. It is a plain `VACUUM` rather than mbgl's incremental auto-vacuum: measured over
  alternating rounds, emptying a 94 MB cache and reclaiming it took 169–201 µs against 41–52 ms,
  both ending at three or four pages. The shapes differ — incremental vacuum costs what was
  *freed*, `VACUUM` costs what *survives*, and after a large delete almost nothing does. Which
  is also why packing is not automatic: with 47 MB still live it is 69 ms, and deleting one
  small region from a large cache should not rewrite every region the user kept.
  A region can also be refreshed. mbgl has no equivalent — its download treats a held resource
  as done, so re-running one fills gaps and changes nothing else, and a region stays a snapshot
  of the day it was taken. A refresh revalidates every resource against its stored etag instead,
  so an unchanged region costs its resource count in round trips and no bytes at all, which is
  what makes it affordable over the connection a downloaded region exists to avoid needing. A
  resource the origin has dropped is dropped rather than kept, so a user does not go on seeing a
  road that has been removed; and a completed refresh releases claims the plan no longer names —
  a style that lost a layer, a source that lowered its maximum zoom, an area redrawn smaller —
  which would otherwise stay pinned for the life of the region, outside the ambient bound and
  never used. A cancelled refresh releases nothing: it has not visited every URL, so what looks
  orphaned may simply not have been reached.
  Telling a call from a literal array is a registry lookup, not a shape test — the spec spells
  it `expression[0] in expressions` — and that registry is now generated from mbgl's two
  (`expressionRegistry` for the special forms, `compoundExpressionRegistry` for the rest, minus
  the `filter-` names mbgl invents when converting legacy filters). Eighty-six operators, under
  DR-6 like the shader tables, because a hand-kept list is wrong silently: the symptom is a
  style that renders slightly differently, not a build that fails. What it fixes is `text-font`,
  whose value is an `array<string>`: `["Noto Sans Regular"]` is how every style writes a font
  stack and is indistinguishable by shape from a call to an operator of that name. Read as a
  call, its fonts cannot be enumerated and the style's labels lose their glyphs.
  `Expression::parse` still names an unrecognized head rather than accepting it as an array of
  strings — the spec catches those by type-checking against the property, which nothing at that
  point knows.
  Exit: probe parity on a *real* style sans symbols — **met**: nine tiles of a
  Protomaps planet extract at z5, all 21 drawables byte-identical to the probe, fills and lines
  both, over the same live origin. The uniforms match too, which was the half outstanding: the
  frame-wide paint parameters at that camera, every layer's drawable buffer, both fills'
  evaluated properties and their sized-and-empty tile props, and the line layer's `ratio` and
  defaults — all twelve buffers the golden carries. Those read the drawable list out of the
  dump's own ids rather than rebuilding a cover, so they need no tile server; which tiles the
  cover holds is a separate assertion that does. Cold-boot-to-first-tile is traced (§12.5): style parse,
  source resolution, cover, first fetch, first bucket, complete, with the cover fanned out
  across workers. Against a local Protomaps extract a nine-tile cover reaches first geometry in
  1.9 ms and completes in 3.7 ms, against 4.1/9.1 ms serially — where the same measurement read
  6.7/22 ms and 12.7/72 ms before the decoder work below. The worker count is a bounded
  constant rather than the host's core count, for the reason §5.4 gives — decode belongs on the
  little cores and a host-derived number makes a workstation measurement say nothing about the
  device. §5.4's one process-scoped pool now exists, with the three
  priority classes, and the cold start queues onto it rather than spawning threads per view; a
  waiter helps with work at or above its own class only, so an hours-long region download at
  the background class cannot get in front of a view trying to draw. The budget the count is
  held against is the RK3566 measurement above, taken on the device lane rather than on a
  workstation loopback. Decode and bucket build are now shared as well as
  fetched once: the bucket cache is consulted *before* the network, so a warm view costs no
  request at all — which matters because coalescing alone dedupes only *concurrent* fetches and
  is deliberately not a cache, so flatness across time waits on §12.6's byte cache or on a
  caller that checks its own first. GeoJSON sources resolve by URL as well as inline — one
  fetch feeds every tile of a cover, since the tiling is the client's. A tile is built per
  *source*: layers are scoped to the source they name, and the source-less ones — a background
  — are built once per tile rather than once per source. `boot` covers both kinds and their
  different lifecycles: a vector source is fetched once per tile because the server cut it up,
  a GeoJSON source once in total because this side does the cutting.
- **R1.5** — *exit met.* Four views over the same style (§13). §9.2's three invariants are
  green, the third — screen-space UBOs per view over shared geometry — asserted in both halves
  at once, since either alone is a property a wrong implementation also has. §13.1's counters
  are at zero. §13.3's sweep now runs through the real per-view state rather than recomputing
  covers from scratch, against a pyramid where a tile takes six frames to arrive: without that
  latency a sweep never enters the state a crossing is about, and cannot tell substitution from
  holes. Sixty-five frames, complete from frame six — the fetch latency, and the earliest any
  frame could be complete — and seventy tiles fetched in seventy calls across four views. Only a
  `Required` retain fetches there, which is the necessity distinction asserted where it costs
  something: if considering a substitute were enough to request it, a crossing's burst would be
  a multiple of the cover that caused it. §13.3's benchmark is now taken on the RK3566 as well, and everything it can
  check before symbols exist is green. Sixty-five frames, four views, seventy tiles built once
  between them: per-frame producer cost — cover, clip masks, drawable matrices, uniform writes,
  the work §5.2 calls irreducibly per-view — is 1.5 ms minimum, 2.3 ms median, 3.5 ms at the 95th
  percentile and 5.1 ms at worst, against 16.7 ms of a sixty-hertz frame. The worst frames are
  the crossings, which is the case §13.3 names. Ring occupancy peaks at 39 KB against a consumer
  draining once per frame, and 239 envelopes in the busiest frame — that is the high-water mark
  §4 wants a ring sized against, for this style; a style with more layers scales it, but the
  order of magnitude is settled. Exit: zero symbol pops — **met**, once R2 had symbols that could
  pop. The sweep runs again with a symbol layer over a grid of labelled points: build each cover
  tile, take identities from the process-scoped cross-tile index, place per view, step the fades,
  and record what every label drew at on every frame of every view.
  Stating it took two goes, and the first was wrong in a way worth keeping. A pop is a label that
  keeps existing on the ground and loses its history, so the obvious assertion is continuity —
  no label's opacity moves by more than one fade increment between frames. That passes when the
  cross-tile index is deleted outright. Every frame the label is handed a fresh identity, so
  every frame it is a *new* label starting a new fade, every step is exactly one increment, and
  nothing ever jumps. What it never does is arrive: it sits at a quarter opacity forever. So the
  assertion that carries the criterion is that a label placed for long enough becomes opaque —
  a pop is the absence of history, and only asking whether a label has *finished* tests for
  history. Keyed by the label's text rather than by its identity, for the same reason: identity
  is what the implementation claims and text is what the ground says.
  Verified by deleting the index: three of the five fail, including that one. Continuity alone
  does not, which is why it is not the criterion.
- **R2** — *exit met, with one qualification named below.* Symbols: glyph manager, shaping,
  quads, per-view placement, collision, cross-tile index, fades. Largest phase; budget ≈ R0+R1.
  Exit: probe parity on a style with a symbol layer, the way R0's is on the hermetic style and
  R1's on a real one — **met**. `symbol_style.dump` reproduces through the production path:
  parse the style, cover the camera, build each tile, fetch the ranges the tile declared over the
  style's own `glyphs` URL, shape, place, encode. Drawable identities, index buffers, the five
  attribute descriptors, the atlas texture's size and format, painter order and all three uniform
  buffers, byte for byte. R1.5's remaining criterion — zero symbol pops — closed with it.
  **The qualification** is the seven elided lines: mbgl packs the glyph atlas in the order glyphs
  arrive and that order is not deterministic, so the symbol vertex hashes and the atlas texture
  hash cannot be compared. Making them comparable is a change to mbgl's atlas behaviour rather
  than to the probe's dump code, and an oracle representing a *modified* mbgl is worth less than
  one with seven elided lines. Investigated and declined, not deferred.
  **Not in this phase**: icons and sprites. R2 is spelled "symbols" and means glyphs. R3's line
  did not name them either while the R2 narrative above already said "until R3 brings the sprite
  atlas" — the plan disagreed with itself, and R3's scope now says so explicitly. What waits on
  it is named where it is missed: vertical writing, images in text and per-section scaling all
  change a line's *height* as well as its width.
  **They had no oracle, and not for the reason first recorded — and now they have one.** It said "without the sprite
  atlas". R3 brought the atlas and they still have none, because what a scaled section changes is
  the *glyph vertex buffer*, and that buffer is elided from every symbol capture — mbgl packs its
  glyph atlas in the order glyphs arrive and that order is not deterministic, which is the same
  elision R2's exit qualification names.
  Measured rather than assumed. A capture of `["format", "Big", {"font-scale": 2}, "small",
  {"font-scale": 0.5}]` and one of the same label at scale one produce *byte-identical* comparable
  data: same vertex count, same index buffer hash, same both per-frame buffers. Two maps that
  differ visibly, and the capture cannot tell them apart at all.
  The probe is this project's rather than mbgl's, and the fix was there. It now emits `fld=`
  beside `src=`: a hash over each attribute's *own* bytes rather than over the buffer it shares.
  Three attributes read a symbol's glyph vertex buffer and only one carries texture coordinates,
  so eliding the shared hash was taking two deterministic attributes with it. With the field
  hashes, attributes 0 and 2 keep a value that is the same across five consecutive captures and
  only attribute 1 is elided — and a scaled label now differs from a flat one in attribute 0,
  which is what these three features change. They are buildable against the oracle now.
  It bought something that was not the point of it, which is worth recording separately. The
  glyph vertex buffer is what the symbol pipeline *builds*, where every other buffer the parity
  test compares is one it derives — and it had never been compared, because the elision took it
  whole. It is compared now, and it matches byte for byte: the glyph positions and label anchors
  of both tiles of the symbol capture. The encoding was right; nothing had said so.
  **Per-section scaling is built, and building it found two defects older than it.** Neither is
  about scaling; both were only reachable once a capture could be read attribute by attribute.
  A symbol's anchor was up to a tile unit low, because a fill or a line reaches the tile through
  `to_tile_ring`, which rounds — geojson-vt rounds a tile's coordinates before mbgl sees one —
  and the symbol path took the projected float straight while the symbol vertex packs its anchor
  by truncating. Every point label, at every zoom. The layout test that passes against the glyph
  capture projects with its own helper rather than building a tile, so it could not see it.
  And the shaper was given `ONE_EM` as its line height, ignoring `text-line-height` — default
  1.2 — so every line of every multi-line label sat 4.8 pixels too close to the one above.
  A single line hides that completely: the vertical alignment branch a one-line label takes does
  not use the block height, and both values give a zero shift there. It takes a line that has
  *grown* to reach the other branch, which is exactly what a larger section does. The feature
  that needed the oracle is what made the older bug reachable.
  **All three are built, and R2's qualification is now only the seven elided lines.** Vertical
  writing and images in text followed per-section scaling through the same door the per-attribute
  hash opened, and each matched the capture on its first comparison — twenty-four vertices for the
  vertical label, both orientations; twelve for the inline image.
  Two things about them are worth keeping. The vertical-orientation predicates are generated by a
  route no other table here uses: `hasUprightVerticalOrientation` is a hundred and twenty lines of
  nested block tests with single characters carved out of the middle by hand, so the probe *calls*
  it for every code unit in the plane and prints the ranges. Every other generator reads a
  declaration; this one reads an answer, which is what DR-6 asks for where a static parse cannot
  be trusted. And the vertical fixture is synthetic — the vendored `TestFont` covers Latin, mbgl
  only shapes vertically when a character has an upright orientation, and every such character is
  CJK. Vendoring a CJK font for three ideographs would put a third party's outlines here for a
  test that never looks at them, so the range is generated with real metrics and a distance field
  that is a gradient rather than a letter.
  Images in text reached further than the other two. One `["image", …]` section binds the whole
  layer to `SymbolTextAndIconShader` — the capture shows `sh0034` where every other symbol
  drawable is `sh0033`, with both atlas slots bound — and it made the SDF flag per *quad* rather
  than per drawable, since a glyph is always a distance field and a sprite usually is not and the
  two now share a buffer. The `["image", …]` operator was missing outright, which the first
  comparison found by drawing nothing at all.
  Placement chooses which shaping is drawn, which is the half that makes the feature correct
  rather than doubled: both go in one buffer and the one that lost is set transparent, because
  the choice is per view and the buffer is not.
  **Was held behind a capture, and is not any longer**: the pitched paths, which stood here in
  three different states because the probe was unrotated and there was no capture to check any of
  them against — R0's second qualification reappearing rather than a new one. A line label's
  collision circles carried the signed distance from the anchor that selects a prefix of the run
  under pitch, computed and stored and read by nothing. `gamma_scale` was written as its
  pitch-zero value of one, with the perspective ratio mbgl scales it by not written at all. And
  the map-aligned branch of the label-plane and coordinate matrices — tile units per pixel,
  rotated by the bearing — was deliberately absent rather than written and untested, since
  producing it would have put a matrix on the wire against no measurement.
  All three closed under R3, once the camera stopped refusing rotation: the label planes land
  there with their map-aligned branch, `gamma_scale` stops being one, and the collision prefix
  turns out to be two mechanisms — a reach along the line and the thinning of the circles — that
  had been described here as one.
  What that qualification cost came due when a building was first drawn at a pitch. The pitched
  camera had never been *evaluated*, let alone compared: it hovered over the map's centre instead
  of orbiting back along its own forward direction, and the pitch was read as radians where it is
  documented and passed in degrees. Both are the identity at zero, so every golden held over
  both. The lesson is not that transcription failed — it is that arithmetic nothing runs is
  arithmetic nobody has checked, and a capture is not the only way to run it. A picture is
  another.
  The pitched **cover** is no longer held back. A pitched view sees a trapezoid whose bounding
  rectangle holds several times the tiles it can — so mbgl walks the tile quadtree against the
  view frustum and discards a subtree the moment its box falls outside, and that is transcribed:
  `Frustum::fromInvProjMatrix`, the conservative separating-axis test, and the depth-first
  traversal. What is left out is `intersectsPrecise`, whose own comment puts its yield under one
  percent. With no capture to diff against, what stands in is an independent computation:
  unproject a grid of screen pixels onto the ground and assert the cover holds every tile they
  land in.
  **The level-of-detail pass landed** (`cover::LOD_PITCH_THRESHOLD`, `frustum::Lod`), and it had
  to. Without it the count runs away with the angle rather than merely being large: on a
  1920×1080 view at z15, forty-two tiles at 55° and nine hundred and ninety-two at 70°, and past
  about seventy-five it exceeded `MAX_TILES` and the cover failed outright — the map went blank
  at exactly the angles a driving view uses. Drawing it coarser is the answer rather than drawing
  less of it: a tile near the horizon is a few pixels tall whatever its zoom, so its parent looks
  the same and costs a quarter as much.
  mbgl gates this on `tileLodPitchThreshold`, sixty degrees, which is also its
  `DEFAULT_PITCH_MAX` — so its camera stops exactly where the mechanism would start and with
  stock settings it never runs the code it carries. This build clamps to the horizon angle
  instead, so it reaches the angles the threshold was written for.
  **Collation is measured and deferred**, which is worth stating precisely because
  "unimplemented" reads like an oversight. A comparison may take a third argument, a collator,
  and twelve suite cases exercise it. mbgl's own default implementation says in a comment that it
  ignores the locale and would need ICU for it; what it does have is DUCET collation order and
  nunicode's `unaccent`. Approximating that with casefold, NFD decomposition and codepoint order
  was tried against the suite and passes **five of twelve** — the seven that fail need DUCET,
  where `a` sorts before `A` and codepoint order says the reverse. Shipping the approximation
  would be a comparison that looks right and is wrong for the same reason a linear stand-in for
  `cubic-bezier` would be, so it was not shipped.
  **The table is generated now** — `tools/unicode-codegen/collation.py` over `allkeys.txt`, in
  DR-6's discipline — behind an off-by-default `collator` feature, as `image`, `cache` and `tls`
  are: some two hundred kilobytes of weights that a style never writing `["collator", …]` should
  not carry (DR-12). Pure data, so it costs the cross lane nothing. Run-compressed to 7,730 runs
  and 3,998 multi-element entries, since consecutive codepoints usually have consecutive
  primaries.
  Two things the suite settled that reading the spec did not. `diacritic-sensitive: false` is not
  "ignore the secondary level": `accent-lt-en` wants `a < ä` to be **false**, and ignoring the
  secondary leaves `ä` one element longer than `a` with a tertiary on it, so it comes out
  greater. The accent has to be *removed* — elements with no primary weight dropped — which is
  what mbgl achieves from the other end, stripping accents from the input with nunicode's
  `unaccent` because it has no level control. And `resolved-locale` must answer the empty string
  rather than the locale asked for: `accent-equals-de` branches on that answer, comparing `ü`
  with `ue` where a German tailoring exists and checking the input directly where none does, so
  an implementation that overstates what it resolved takes a branch it cannot honour.
  Han is the other half of being right rather than plausible. The ideographs are not in
  `allkeys.txt` at all — they are given an order by construction, UTS #10 §10.1.3 — so without
  the implicit-weight formula every Chinese label would compare equal to every other. Not built:
  the 964 contractions, sequences collating as one unit such as Danish `aa`, which need a
  longest-match scan rather than a lookup per character. The generated table carries the number
  so the omission is countable.
  Wired into the evaluator, **all sixteen of the suite's collator cases pass** — the fifteen in
  the directory and `equal/collator-value` outside it, where the approximation passed five. Two
  of them are compile errors rather than comparisons: a collator given numbers to order is a
  category error the spec catches statically, and the two cases that assert it had been passing
  for the wrong reason, because `collator` was an unknown operator and the parse failed for that.
  A collator is taken only where one is *written* — a comparison's third argument, or
  `resolved-locale`'s only one — rather than being a value. The spec's type system does have a
  collator type, so binding one with `let` and passing it by `var` is legal by it and refused
  here with a message saying so. Making it a value would mean a `Value` variant, which is a
  wider change than the one position it buys.
  The baseline is filtered by feature rather than kept twice, since a build without the table
  cannot pass those cases and should not be told it regressed. The filter is by *name* and not by
  directory: `equal/collator-value` lives elsewhere, which is exactly what a prefix test gets
  wrong quietly.

  The SDF glyph range format reads, as `tessella-glyph/pbf`: `{fontstack}/{first}-{last}.pbf`,
  256 codepoints a file, metrics and a distance field with the ecosystem's three-pixel border.
  Almost all of it is rejection, and that is the part that matters — proto2 makes every field
  optional on the wire, so a glyph missing `advance` parses perfectly and then lays out on top
  of its neighbour. A declared width and height that disagree with the bitmap's length is the
  one that would be a read past the end, so the glyph is dropped rather than the bitmap
  clamped. Zero-area glyphs are kept: a space has an advance and nothing to draw, and a range
  that dropped its spaces would set the words run together.
  Checked against mbgl's `GlyphPBF.Parsing` and its `fake_glyphs` fixture, which is built for
  exactly this — glyphs wrong in a different way each, plus one that is right. A parser that
  accepted them all would pass a test written against a real font, because a real font has no
  bad glyphs. Two rejections survived deleting the checks anyway, since that fixture happens not
  to carry a glyph complete but for one field; those cases are hand-encoded in the test.
  The manager above it is mbgl's `GlyphManager`: the `{fontstack}`/`{range}` URL, one entry per
  font stack, and the bookkeeping that decides what to ask for. Absence is remembered per
  *range*, not per glyph, which is the distinction the whole thing turns on — a font does not
  contain every codepoint in a range it serves, and "missing because unfetched" and "missing
  because the font lacks it" look identical in the glyph table. Without it every label carrying
  one unusual character re-requests its whole range on every tile, forever, and succeeds every
  time. An empty answer settles a range and a transport error does not: one is knowledge, the
  other is a network that blinked. The stack is part of the key as well as the URL, so a bold
  face never answers for a regular one — the right letter in the wrong weight, which nothing
  errors about.
  Line breaking lands next, which is where a label stops being a string. It is a shortest-path
  problem and not a greedy fill: every break opportunity is a node, a line's cost is how far its
  width sits from the *average* line width, and the answer is the cheapest path. Aiming at the
  maximum instead would fill each line to the brim and leave the last one short, which is the
  greedy result by another route and conspicuous on a label sitting under a symbol. Penalties
  carry the typography — fifty for an opening parenthesis left at the end of a line, a hundred
  and fifty for breaking between ideographs when the server has already suggested breaks with
  zero-width spaces, and minus ten thousand for a newline, which the badness function squares
  and *subtracts* so that an author's break outweighs any raggedness it causes.
  Checked against mbgl's `Shaping.ZWSP`, which fixes the line count for four inputs at four
  widths. The Unicode blocks that permit a break without a space are generated from mbgl's own
  table under DR-6 rather than from Unicode's `Blocks.txt`: mbgl comments out the blocks it does
  not consult, and a table built from the standard would break lines where mbgl does not.
  Three of the tests around it were vacuous — the parenthesis penalty, the short-last-line
  preference and the whitespace rule all survived being deleted — so the discriminating inputs
  were searched for rather than guessed at, and all five rules now fail when removed.
  Laying the lines out follows: glyph positions, justification and anchor alignment, which is
  mbgl's `shapeLines` for horizontal text in one font stack. The anchor names the part of the
  label that touches the point, so it moves the box without changing its extent — a shaper whose
  extent varied by anchor would make placement's collision box depend on where the label
  happened to be anchored. Checked against `Shaping.ZWSP`'s four bounding boxes, which between
  them pin the line count, the line height, the widest line and the anchor's effect on all of
  it. Vertical writing, images in text and per-section scaling were listed here as unimplemented,
  with no oracle until R3 brought the sprite atlas. Both halves of that were wrong: R3 brought the
  atlas and nothing changed, and what stood in the way was the elision of the glyph vertex buffer.
  All three are built now — R2 above records what the probe change that unblocked them also
  caught. Three more tests were vacuous for want of a case — every one used zero spacing and no
  leading whitespace — so the trailing-spacing rule, the final advance in justification and the
  line trim all survived deletion until inputs that separate them were added.
  The atlas under all of it is a port of `mapbox::ShelfPack`, which is what mbgl's dynamic
  texture uses, with an R8 surface over it (§12.4: this is the largest texture the process
  keeps, and three of four channels would hold copies of the one that matters). Shelves waste
  the space above a short glyph on a tall row, and a general rectangle packer would waste less —
  but glyphs from one font are nearly all one height, and what matters more is that insertions
  stay *clustered*, since §6.4's damage is a list of rectangles and a scattering packer makes
  every upload a union covering most of the texture. Slots are refcounted, so a glyph two tiles
  want is one rectangle; a freed slot keeps its size rather than merging back into its shelf,
  which is what lets the next glyph of that size land exactly where the old one was. Padding is
  two pixels and one of them comes back inside the reported rectangle: the outer one stops
  linear filtering pulling in a neighbour, the inner one gives the shader real distance field to
  read at the glyph's own edge.
  Quads follow, which is where a shaped label becomes geometry: four corners per glyph in
  label-local pixels plus the atlas rectangle to sample. The quad is deliberately larger than
  the ink — the encoder's three-pixel border plus the atlas' one — because a distance field is
  only useful where the shader can read *outside* the letter, and sizing the quad to the ink
  clips the falloff that is the antialiasing. mbgl's own numbers pin it: a 24×24 glyph with
  `top` -8 and a 32×32 rectangle gives a quad from (-4, 4) to (28, 36), which fixes the buffer,
  the sign of `top` and the half-advance cancellation together. That cancellation is kept in
  mbgl's un-reduced form on purpose: for a label following a line the second half moves into
  `glyph_offset` so the shader can apply it after projecting, and writing the reduced form makes
  that a rewrite rather than a branch.
  The per-view half starts with the collision grid, a transcription of mbgl's `GridIndex`:
  boxes and circles in a plane, cut into cells, so a candidate is compared against what is near
  it rather than against every symbol already placed — which at street zoom is thousands per
  tile. Two of mbgl's quirks are transcribed rather than tidied: its box test is inclusive at the
  edges while its circle test is strict, and placement's output depends on the asymmetry. One is
  *not* transcribed — its circle query lacks the `return` its box query has after the whole-grid
  shortcut, so it reports every element twice; nothing catches that there because the only caller
  reaching the path stops at the first result.
  All five of mbgl's `GridIndex` tests pass, and they were not enough. Mis-sizing the cells so
  the grid collapses to one cell leaves every result *correct* — everything becomes a candidate
  and the exact tests filter it — so no assertion about query results can see it. What is lost is
  the reason the grid exists. It now reports how many shapes share a cell with a query, and that
  is asserted directly: one for a one-cell query over a hundred spread shapes, four for a
  four-cell query.
  A label's collision box follows: mbgl's `CollisionFeature` for point placement. Scale then pad,
  in that order, which is what keeps `text-padding` a constant number of screen pixels instead of
  something that widens as the map zooms in. A rotated label reserves the upright box that
  contains it, since the index is axis-aligned — mbgl notes it "may be quite large for wide
  labels rotated 45 degrees", and a long label on a diagonal duly reserves close to a square.
  A label that occupies nothing gets *no* box rather than an empty one: a zero-sized box at the
  anchor still collides with whatever covers that point, so a label still waiting for its glyphs
  would push a visible one off the map.
  One flaky test was fixed on the way. `sources_resolve_together_rather_than_in_turn` bounded a
  fan-out by wall clock, which is a measurement of the machine it ran on — this file's own header
  says as much — and it failed under a loaded workspace run while passing every time alone. It
  now counts how many manifest fetches are in flight at once, which does not move with load. The
  first version of that gauge counted *every* fetch and was satisfied by the tile phase whatever
  the manifests did; making resolution strictly serial still passed until it was narrowed. Its
  sibling `the_cover_is_fetched_in_parallel` had the same flaw and duly failed on a shared CI
  runner — four workers 881 ms against one worker's 714 ms — so it now counts overlapping *tile*
  fetches instead. Manifests and tiles are gauged apart, because a boot fans out twice and a
  single gauge is satisfied by whichever phase happened to overlap.
  Fades land next, which is where §6.5 is actually decided. Placement produces a boolean per
  symbol per frame; this turns it into the opacity it draws at, so a label that loses a collision
  leaves rather than vanishing between two frames. A fade is the one thing that keeps changing
  while nothing else does — camera stopped, tiles arrived, label still on its way to opaque — so
  it counts as churn until it settles and then has to go completely silent; a fade that never
  quite reached 1.0 would keep the map awake forever, and the counter that says so is asserted
  to reach zero and stay there. mbgl's one-frame lag is transcribed rather than corrected: the
  step takes its direction from the *previous* frame's placement, so a symbol that loses its
  collision still brightens once before it starts leaving. That is what stops a label flickering
  when a collision result oscillates, and smoothing it here would trade a rare stale frame for a
  common flicker. State is keyed by cross-tile id, so a label arriving in a new tile at a zoom
  crossing keeps the opacity it had — re-fading one that never left is exactly the symbol pop
  §13.3 asks for zero of.
  The index that assigns those ids follows: mbgl's `CrossTileSymbolLayerIndex`. At a crossing
  every tile is replaced by four children, and the label that was "Detroit" in the parent is a
  different symbol instance in the child — different tile, different buffer, nothing saying it is
  the same label. Matching is by text and by position rounded onto a four-pixel grid, since a
  label does not land on the same coordinate at two zooms. The rounding is also the bound: two
  genuinely distinct labels with the same text within four pixels become one, which is the right
  trade, since two identical labels that close together are a data error and treating them as one
  is nicer than blinking.
  mbgl's `addBucket` fixture is reproduced id for id, and **four separate mutations survived
  it** — dropping the tile origin from a position, dropping the rounding, letting a parent lend
  one label to every child, and never releasing a removed tile's claims. The fixture is
  degenerate in ways it never had to care about: perfectly aligned tiles, an offset of one tile
  unit, and two children that never contend. Four tests were built to discriminate, which needed
  a parent label placed exactly on the seam between two children before the lend-once guard is
  reachable at all.
  The decision loop closes the per-view half. Candidates are offered in the style's order — by
  `symbol-sort-key`, then feature order — and what fits is placed and inserted so it blocks
  whatever comes after. There is no global optimisation, deliberately: a cartographer decides
  what matters rather than an algorithm, and a set that re-optimised as the camera moved would be
  a map where labels swap places while you watch. `allow-overlap` and `ignore-placement` are
  different permissions — one skips the test, the other skips the insert — and a label with both
  is drawn always and blocks nothing, which is how a style pins one that must never move. The
  four-way `text-optional`/`icon-optional` combination is asserted as its whole sixteen-row truth
  table rather than at a few points, since a rule that is right for text alone and inverts when
  an icon is present looks correct on most styles.
  Resolving a feature into a label starts the wiring back the other way. `text-field` has two
  syntaxes and both are still in use, often in one document: the modern expression and the legacy
  `"{name}"` template. A frontend reading only expressions would render half the basemaps on the
  internet with no labels, so both are read — and tokens are resolved *after* an expression
  evaluates too, which is what styles written against the old syntax and later wrapped in a
  `concat` rely on. An unrecognised token survives verbatim, braces and all, the same rule the
  tile URL templates follow: a label reading `{nmae}` is a typo somebody can see and fix, and a
  label silently reduced to nothing is not. A feature with no name — which is most of them —
  produces no label rather than an empty one, since an empty label still has an anchor, a
  collision box and a place in the sort order, and would push real labels off the map to draw
  nothing.
  A token in `text-field` is a `get`, not a substitution: mbgl converts `"{name}"` at parse time
  into `toString(get("name"))`, so a feature without the property yields an *empty* label and
  therefore no symbol. This is deliberately not the tile URL rule, where an unrecognised token
  survives verbatim so a 404 says why — a label cannot do that, and leaving the token writes a
  literal `{name}` across the map on every unnamed feature. Which is what it did until an
  end-to-end test asked a water layer for its glyph dependencies and got seventy-five labels
  back from features with no names.
  Which glyphs a tile needs is collected in one pass before anything is shaped, as mbgl's
  `GlyphDependencies`: what to fetch is a property of the *data* rather than of the style, since
  one font stack needs a handful of ranges over Iceland and hundreds over Japan. Shaping needs
  advances, advances need glyphs, and glyphs cross the network — discovering a missing glyph
  mid-shape turns one round trip per tile into one per label. Measured on the fixture: seventy-five
  labels, thirty-odd distinct codepoints, one range.
  Line placement lands: `getAnchors` and `checkMaxAngle`, which is what puts a name along a road
  rather than at a point. Three things have to hold at once for a position to be kept — the whole
  label fits between the line's ends, it lies inside the tile, and the line does not bend too
  sharply under it — and the last is `text-max-angle`, which is why a name vanishes from a hairpin
  instead of wrapping round it. The bend check sums the turn over a sliding *window* rather than
  at one corner, because it is accumulated curvature that makes text unreadable, not any single
  turn. Two details carry more than they look: the spacing is widened when a label is long
  relative to it, so labels do not overlap along the line and give collision work done only to be
  discarded; and the first anchor sits half a *spacing* in on a line continued from the next tile
  and half a *label* plus two glyph widths in on one that starts inside, which is what makes two
  tiles' labels interleave at the seam rather than double up. Checked against all six of mbgl's
  expectations — position, angle and segment index — including the invariant that an overscaled
  tile's anchors are a superset of its parent's, which is what stops every label jumping at a
  zoom crossing.
  `line-center` comes with it: one anchor at the line's midpoint, for a river or a boundary whose
  name should appear once rather than march along the feature. It deliberately has *no*
  tile-bounds test, unlike the repeating case — a centred label belongs to its feature rather
  than to a position, so a line whose middle falls outside this tile still gets its name, which
  mbgl's own expectation of an anchor at (-3, -3) pins. And a bend at the centre refuses the
  label outright rather than sliding it along: the caller asked for the centre, and answering
  with somewhere else would silently answer a different question.
  `build_line_symbols` wires it through. One shaping serves every repetition — the glyphs, their
  corners and their texels are identical at every anchor and only the anchor differs, so shaping
  per anchor would redo the same work for every repetition of every road name on a street-zoom
  tile. The along-line distance rides in `glyph_offset` rather than in the corners, because the
  shader projects a line-following label before placing each glyph; baked into the corners it
  would lay the label out flat and then bend it, putting every glyph but the first in the wrong
  place. And a line label never wraps, at any width: it follows the line, and a second line of
  text would have to follow it too, offset along a curve — which the along-line projection cannot
  express and mbgl does not attempt.
  The chain then runs end to end, over a real tile: decode, resolve `text-field`, shape against
  a real glyph range, pack the atlas, build quads, derive a collision box, take a cross-tile
  identity, place, fade. Each link had its own tests and most were checked against mbgl, and none
  of that says the links *fit*. This found the mismatch immediately, and it is the one worth
  writing down: **placement happens in screen space**. Anchors arrive in tile coordinates,
  0..8192 across, and a shaped label measures in screen pixels and is tens across; mixed, every
  label is a speck on a vast plane, nothing ever collides, and all seventy-five place. Labels
  compete for screen and not for ground — two towns a kilometre apart collide at z5 and not at
  z14, and the same two collide on a phone and not on a wall display — so the anchor is projected
  before a box is built. The cross-tile index stays in tile coordinates, and that is right for
  the opposite reason: identity is about where a label is on the ground, and the ground does not
  move when the camera does. With the projection in, 32 of 75 place at z5.
  Symbol vertices land last: mbgl's `layoutVertex`, in the byte layout `SymbolIconShader`
  declares. The anchor and the corner offset share one `Short4` because some devices allow only
  eight vertex attributes — mbgl says so in a comment, and it is why the packing looks arbitrary.
  Everything is fixed point at three different scales: the corner offset in 1/32 of a pixel, the
  pixel offset in 1/16, the minimum font scale in 1/256, each the precision that term needs
  against the range it covers. Confusing two is a silent power of two — a label in the right
  place at the wrong size. The size carries `isSDF` in the low bit it vacates when shifted, which
  is why sizes cap at 255: `255 * 128 << 1` is the largest that still fits a `u16`. The
  attributes being filled are checked against the generated table, so an upstream layout change
  fails the build rather than quietly producing vertices the shader no longer reads.
  §9.1's oracle then reaches symbols: `symbol_style.dump`, the first capture with a symbol layer,
  against a vendored font both sides read. It confirmed the vertex packing from mbgl's *output*
  rather than from its source — three interleaved attributes at 0, 8 and 16 with a stride of 24,
  plus two more in buffers of their own, which is how the dynamic and opacity buffers were shown
  to be separate rather than assumed. The index buffers match byte for byte.
  It is also the first capture that does not fully reproduce. mbgl packs the glyph atlas in the
  order glyphs arrive and that order is not deterministic: over ten consecutive captures of an
  identical style the symbol vertex hashes and the atlas texture hash each took four or five
  distinct values, one dominating, while every other line of the eighty-seven was identical every
  time. The vertex hashes follow the atlas, since the `data` attribute carries texture
  coordinates. Seven lines are elided the way `symbol_fade_change` already is, and the elision is
  a committed script so a regeneration still reproduces. The two per-frame attributes were stable
  across all ten and are *not* elided — eliding a stable line gives away a comparison for
  nothing. Byte-exact symbol vertices need the atlas packed deterministically on mbgl's side,
  which is a change to the probe rather than to this. Investigated and declined: the iteration
  itself is deterministic — `std::map` by fontstack then glyph id — and what varies is *which*
  glyphs have arrived when the first upload runs, since glyph loading is async. Making that
  deterministic means changing mbgl's atlas behaviour rather than the probe's dump code, and an
  oracle that represents a modified mbgl is worth less than one with seven elided lines.
  The two per-frame buffers *are* comparable, which was nearly written off. They were assumed to
  hold post-placement state that only a matching frame loop could reproduce; solving for their
  contents showed otherwise. The position buffer is the label's anchor at build time with an
  angle of zero — a **rounded** tile coordinate, since mbgl carries an anchor as an integral
  `GeometryCoordinate` — so matching it byte for byte checks the projection from longitude and
  latitude into tile units against mbgl's to the unit, and pins that a tile's labels sit in the
  buffer in the order the layer offers them. The opacity buffer is uniformly zero, which decodes
  as *not placed* rather than the `(true, 1.0)` written at build time: the probe's frames update
  it from a placement holding no entry for these symbols. So it pins the encoding and the width
  and says nothing about placement, and comparing real placement output needs a capture in which
  the probe has placed something.
  The layout glue then moves into the library where it belongs: `build_symbols` takes a layer's
  labels and a glyph source and produces one tile's buffers. One buffer per layer per tile, which
  is what the golden shows mbgl doing — its twelve-glyph drawable is two labels, not two
  drawables — so a second label's indices have to reach its own vertices and each label's
  vertices carry its own anchor. A label whose glyphs are not all packed draws the ones that are
  and still measures the whole for collision, because a map that waited for a font before drawing
  anything would show nothing during a pan into new text.
  Symbols then reach the wire: `encode_symbol` turns a laid-out layer into a `GeometryAdd` with
  the five attribute descriptors the capture measured — three sharing one interleaved slab at
  stride 24, and two per-frame buffers with slabs of their own. A consumer reads those
  descriptors literally, so an attribute pointed at the wrong slab draws whatever is there and
  nothing in the stream says it was wrong; each is asserted to read a slab holding exactly
  `vertices × stride` bytes with the last vertex inside it. One segment, which is what the
  capture shows and what a layer sharing one buffer implies. `Encoded` grew decoded accessors on
  the way — three existing tests were hand-decoding spans out of the payload.
  The two halves then join: `ViewSymbols::frame` runs per view per frame — project the anchors,
  compete for space, advance the fades — and writes the result back into the two per-frame
  buffers. Layout runs once per tile and is shared (§5.1); this is the per-view cost centre §5.2
  names. The projection is the caller's, because placement happens in screen space and the
  camera is per view: the same two labels collide at z5 and not at z14, and on a phone and not
  on a wall display, which is asserted as behaviour rather than described. A label's per-frame
  state is written into the slice of the shared buffer that layout recorded for it, since a
  layer's labels share one buffer and a range that is off writes one label's opacity over its
  neighbour's — which draws as a label that will not fade, and errors nowhere. Fades stay keyed
  by cross-tile id rather than by buffer position, so a tile rebuilt at a crossing does not
  re-fade the labels that never moved.
  A picture then gets drawn, because every other test here checks a number and a map is a thing
  you look at. A software rasterizer behind `#[ignore]` decodes the packed vertices exactly as a
  shader would and writes a PNG, so it exercises the wire format rather than the shaper behind
  it — and it has now found three things no assertion did. Text came out illegible at an SDF
  edge of 128 when mbgl's is `(256-64)/256`; a smoothstep rewritten for one clippy lint became
  `t*t*(1-2t)`, negative past the halfway point, so glyph interiors were skipped and the fix was
  found by measuring the font's SDF histogram rather than by adjusting the threshold again. And
  every glyph of a line label drew on top of the first, because the along-line distance was
  recorded nowhere. That last one is the one worth writing down: the test asserting the distance
  is *not* in the corners passed, and nothing asserted it was anywhere, so it went missing
  between the shaper and the buffers with the whole suite green. It is mbgl's
  `PlacedSymbol::glyphOffsets` — per quad rather than per vertex, since a glyph's four corners
  share one place in the word — and it stays out of the vertex for the same reason the corners
  do not carry it: the shader projects the line first and then walks along the projected result,
  so a value baked into the geometry would be bent twice.
  Two more came out of looking at it again. The rasterizer was *scattering* — walking the glyph's
  own box and writing to the rotated position each sample mapped to — which is fine at zero
  degrees and full of holes at any other angle, because rotating a grid does not give a grid. It
  gathers now, the way the point-label path already did. And about half the road labels read
  right to left, which is `text-keep-upright`.
  So `symbol_projection.cpp`'s along-line placement lands: `place_glyph_along_line` and the
  `place_glyphs_along_line` around it. This is per view per frame and has to be — which way a
  road runs *on screen* is a property of the camera, so the same label is upright at one bearing
  and upside down at another — and it is why layout hands over one distance per glyph rather than
  a position. Three things in it are easy to drop and each is invisible until it is not: the
  direction of travel is the sign of the offset, so glyphs before the anchor walk *backwards*
  along the line; a glyph walked backwards takes a half turn so it is not drawn mirrored; and the
  perpendicular offset is signed by that direction, so a label above its road stays above it when
  the walk reverses. mbgl accumulates those half turns rather than normalizing, and this does
  too — the angle is only ever consumed through a sine and a cosine, so a glyph at two pi is a
  glyph that is upright, and a transcription that "tidied" it would be departing from the oracle
  for nothing.
  Keeping text upright is a *retry*, not a branch: place the label, and if the first glyph lands
  right of the last then it reads backwards, so place it again walking the other way. It is
  tested on the two end glyphs rather than on the anchor's angle, because a label spanning a bend
  can sit on a segment running one way while the label as a whole reads the other. A line too
  short answers "no room" rather than "needs flipping", so the caller is not sent round the loop
  to discover the same thing twice. And `text-keep-upright` off has to *place* rather than
  refuse, since the property exists for symbols meant to follow the line whichever way it runs.
  Placement then reaches line labels, which is what turns a street tile from a solid block of
  text into a map: mbgl's `bboxifyLabel`, the run of circles a label following a road reserves.
  A name on a diagonal has a bounding box close to a square, and reserving that square blanks
  everything in the quadrants either side of a road no one is standing on — which is the same
  cost the point path pays for a rotated label, except that a line label is rotated by definition
  and often more than once within its own length. The grid already indexed circles and tested
  them against boxes; what was missing was the piece between. A candidate now reserves a *shape*
  — one box, or a run of circles — because the two are never mixed and never both present, which
  is what an enum says and mbgl's `alongLine` flag does not.
  Three details in it are mbgl's and none is obvious. The walk backwards to the label's first
  segment starts at the vertex *after* the anchor's segment, so its first step measures from the
  anchor itself; starting at the segment skips that step, and an anchor most of the way along a
  long segment is then treated as sitting at the near end of it, which puts the whole run at the
  start of the line — found by a right-angled road whose label came out on the wrong arm. The run
  extends past the label, because a pitched camera draws a distant label *larger* than the box it
  was laid out for and a label that has outgrown its collision shape overlaps its neighbour with
  nothing detecting it; the padding grows with overscaling but only slowly, since an overscaled
  tile places labels closer together and each extra circle costs a query. And the padding
  *before* the label survives only when the line's vertices are coarse enough that the walk
  overshoots — on a finely divided line it is skipped, which mbgl's own comment concedes "could
  allow for line collisions on distant tiles". That asymmetry is asserted rather than tidied,
  because it is exactly what a later reader corrects on sight.
  Any circle hitting refuses the whole label rather than drawing part of a road name, and the
  per-circle distance from the anchor — padded down by a fifth, mbgl's "conservative padding" —
  is what a pitched camera will use to test a *prefix* of the run. On the street fixture 425
  repetitions become 173.
  Symbols then reach the tile builder, which is where the two-phase shape of a symbol layer stops
  being an implementation detail and becomes a type. Every other layer turns features into
  vertices in one pass: the geometry is in the tile and nothing else is needed. A symbol layer
  cannot, because shaping needs glyph metrics and the glyphs are a *network resource whose URL is
  not known until the text has been resolved*. So `SymbolLayout` holds text, geometry and the
  codepoints per font stack, and no vertices at all; the only way to get vertices is `lay_out`,
  which takes the glyphs as an argument. mbgl splits it in the same place, between constructing
  the layout and `prepareSymbols`. Making the phases *types* rather than a flag is the point: a
  half-built bucket that is sometimes shaped and sometimes not is exactly the state that draws
  blank tiles when a font is slow.
  `symbol-placement` decides which builder runs and what geometry is kept — one anchor per ring
  for a point label, the whole ring for a line one — and the layout properties are evaluated at
  the bucket zoom, since `text-size` interpolated over zoom is in most styles.
  Data-driven layout properties then land, which was the gap that piece left. `text-size`,
  `text-max-width` and `text-letter-spacing` are evaluated per *feature*, not per layer, because
  that is the granularity the spec gives them and what a style uses to set a capital larger than
  a town on the same layer. Nothing about the encoding had to change: the vertex already carried
  a size per quad, and what was missing was a size per label.
  Laying out is now by *runs* — the longest stretch of consecutive labels sharing a font stack
  and a set of text options — rather than by grouping. That fixes a divergence the font-stack
  grouping had introduced: a layer's labels sit in its buffer in the order the layer offers them,
  which the golden pins because a tile's per-frame state is written into the slice layout
  recorded for each label. Gathering every label of one stack together produces identical
  geometry in a different order, which is byte-for-byte wrong against the oracle and looks like
  nothing at all until a second stack or a second size appears. With one of each, which is the
  common case, there is one run and no join.
  Two things fell out of wiring it up. A symbol layer over a *vector* tile went through a
  different builder than one over GeoJSON, and that builder ended in a wildcard arm — so enabling
  the layer type in `is_built` would have had it silently draw nothing from every real tile. The
  wildcard is now spelled out per type, which is what turns the next such gap into a compile
  error. And the circle layer turned out to have been in exactly that position already —
  enabled in `is_built`, an arm in the GeoJSON builder, and nothing in the vector one, so every
  real tile produced an empty bucket and nothing anywhere said so. It draws now. Its geometry
  type is not checked, the way a fill's is not: mbgl's `CircleBucket::addFeature` takes whatever
  the feature carries, so a line's vertices each get a disc.
  The store between the two phases then lands as `tessella-glyph/fonts`: the manager knows which
  ranges are held and the atlas knows where a glyph sits, and neither is something a bucket
  builder can shape against. Pairing them turns "the ranges arrived" into the `Glyphs` layout
  wants. One atlas per font stack, which is §5's and mbgl's — a rectangle is a position in a
  *texture*, so the same codepoint in two fonts is two rectangles and one atlas per style would
  have the second stack read the first's pixels.
  Only what was asked for is packed. A range file is 256 codepoints and a label uses a handful,
  so packing on arrival would fill the atlas with glyphs nothing draws and evict the ones that
  are drawn; packing is driven by the dependencies the layouts declared. That is also why the
  atlas fills in the order labels ask rather than in codepoint order — the same order mbgl's
  fills in, and the reason its packing is not reproducible. A space is the case that has to go
  both ways at once: it keeps its advance and is *not* packed, since a zero-area rectangle takes
  a shelf slot and hands the shaper something to draw, which is a blank quad per space on every
  label of the map.
  Asserted where it pays: the street fixture's symbol layer resolves 873 labels over 1773 roads
  and every one is ASCII, so the whole tile costs *one* request and the next tile costs none. A
  store keyed per label, or per tile, would work perfectly while spending a round trip a label.
  The `Glyphs` trait moved to `tessella-glyph` on the way, re-exported from where it was. The
  crate that answers the question should declare it, and it could not implement a trait declared
  in `tessella-layout` without depending on the crate that depends on it.
  Laying out then resolves per font stack rather than against one. `text-font` is evaluated per
  feature, so a data-driven one gives a layer several stacks, and mbgl reaches the same place
  from the other end by handing `prepareSymbols` the whole `GlyphMap`. Labels are grouped by
  stack, each group shaped against its own glyphs, and the buffers joined — which needs the
  appended indices offset onto the existing vertices and each group's vertex *ranges* shifted by
  the same amount. Getting that wrong writes one label's per-frame state over another's, which
  draws as a label that will not fade and errors nowhere. The join asserts the `u16` bound too,
  since two buffers each inside it can be outside it together.
  The golden then reaches the *path* rather than the layout. Every symbol comparison until now
  assembled its own labels — it decided which two went in which tile and packed the atlas from a
  list it was handed — which checks the shaping against mbgl and says nothing about what a frame
  actually does: parse the style, cover the camera, build each tile, fetch the ranges the tile
  declared over the style's own `glyphs` URL, shape, encode. Each of those is a place a label can
  be lost. Driven end to end, the index buffers are still the oracle's byte for byte, and the
  encoder's five attribute descriptors are compared against the dump's rather than against
  literals for the first time.
  It found the gap immediately, which is what an end-to-end comparison is for. A point label was
  not clipped to its tile — the builder is handed the whole GeoJSON source rather than one tile's
  share, the way the fill and line arms are, and each of those clips for itself. So every tile of
  the cover drew every label: right on the tile that owns it, wrong on its neighbours, and
  invisible to any test that assembled its own tile assignment. The test is bounded half-open so
  a point on a boundary lands in exactly one tile. A *line* label is deliberately not clipped —
  `get_anchors` tests each candidate against the tile, so a road crossing a seam gets anchors on
  the near side from each tile and the two interleave; cutting the line here would give each side
  its own ends and put a name at every seam.
  The atlas then reaches the stream, which is the third texture the symbol capture has and the
  hermetic one does not: mbgl's `0x0` pattern placeholder, its `1x1` transparent image, and a
  glyph atlas at `512x512 fmt=1`. The hash is elided with the rest of the packing-order lines;
  the dimensions and the format are not, and both are on the wire. The atlas had been sized 2048
  on a hunch when the store was written — the oracle says 512, and a consumer sizing its
  allocation from the first upload would have got a different texture from the one the capture
  describes. `fmt=1` is Alpha, which is §12.4's point measured rather than argued: this is the
  largest texture the process keeps and three of four channels would hold copies of the one that
  matters.
  The upload carries dirty rectangles rather than the image, and answers *nothing* when nothing
  moved — §6.5's still frame is a frame with no envelopes in it, and re-uploading a quarter of a
  megabyte of unchanged glyphs every frame would make a settled map the most expensive one. Past
  §4's rect cap they collapse to their union, which costs bandwidth and never pixels.
  Painter order for a style with symbols in it then joins the fill and line layers': all fourteen
  entries of the symbol capture's `order` section, compared entry for entry the way the hermetic
  style's forty-three already are. It is the only place the symbol layer's pass and sublayer are
  *checked* rather than chosen — they were chosen, since the dump shows sublayer 0 in the
  translucent pass while symbols overhanging tile edges would make leaving the stencil off the
  defensible guess. Writing it turned up the trap the section is full of: the `layer=` field of a
  draw line is mbgl's depth slot, which runs opposite the style index, and the style index is in
  the drawable key beside it. Reading the wrong one puts the background on top of everything.
  Two of the symbol layer's three uniform buffers then land byte-exact: the tile props at slot 3
  and the evaluated props at slot 5. The slots and the sizes come from the tables generated out
  of mbgl (DR-6), so checking them against the capture is those tables checked against the code
  they were generated from — `SymbolDrawableUBO` is 260 bytes at a stride of 272 and the oracle's
  array is 544 for two drawables, which is the padding being the *stride* and not the size.
  Writing them needed a symbol paint spec table, which did not exist. Ten properties, five for
  text and five for icons, and the icon half is written whether or not a layer draws icons —
  one shader serves both and the buffer is its interface. That half is what catches a zero-filled
  shortcut: `icon-color` defaults to *opaque black*, so a buffer filled with zeros for the unused
  half puts a transparent black on the wire where the oracle has an opaque one. The style names
  only `text-color`; every other value in the buffer is a spec default, which is what makes this
  a check of the resolution rather than a transcription of the dump.
  `is_halo` is a second *drawable* over the same geometry rather than a flag on one — mbgl draws
  the halo first and the fill over it, so a layer with `text-halo-width` emits twice — and
  `gamma_scale` is one at pitch zero, left at one rather than given the pitched value mbgl scales
  it by, since inventing that would put a number on the wire nothing produced.
  The drawable array follows, and it is the one that is not a paint buffer: three matrices per
  entry, because a symbol is drawn in three spaces at once. `matrix` places the tile the way
  every other layer's does; `label_plane_matrix` takes tile coordinates into the screen units the
  label was *laid out* in, which is where a line label's glyphs are walked along; and
  `coord_matrix` takes that plane back to clip. Baking them into one works for a point label and
  puts every glyph of a line label in the wrong place, since the walk has to happen between the
  two — which is the same fact the along-line projection is built on, arriving from the other
  side.
  Both are mbgl's viewport-aligned branch only. `text-pitch-alignment` defaults to `viewport` for
  point placement; the map-aligned branch scales by tile units per pixel and rotates by the
  bearing, which needs a bearing this build refuses, so producing it would put a matrix on the
  wire nothing has checked. The coordinate matrix carries no tile and no camera at all — it is
  the viewport's alone, two over the width and minus two over the height — so it is the same for
  every drawable of a frame, and a version that folded in the tile would still draw a point label
  correctly. That is asserted separately, because the buffer comparison sorts its blocks and
  would pass with the two matrices swapped between entries.
  With that, `symbol_style.dump` reproduces in full but for its seven elided lines: the drawable
  identities, the index buffers, the five attribute descriptors, the atlas texture's size and
  format, the painter order and all three uniform buffers. What remains elided is the atlas
  packing order, which is mbgl's to make deterministic.
- **R3** — *in progress.* Sprites and icons, raster, patterns/dynamic textures (rect-list
  damage), fill-extrusion.
  Cross-faded pattern binders were built for fills and only for fills, which read as working: a
  data-driven `line-pattern` parsed, resolved, chose the pattern shader and bound the atlas, and
  then bound no per-vertex rectangles at all, so every feature in the layer drew whatever the
  uniform pair happened to say. Right at an integer zoom and wrong between them, right for one
  feature and wrong for the rest. It now binds them, and where they go came from the oracle
  rather than from reading the binder classes: ids nine and ten at bindings *seven and eight*,
  where a fill puts the same two streams at ids four and five, bindings one and two — the line
  shader has already spent its low bindings on colour, blur, opacity, gapwidth, offset and
  width. That needed a new capture, so `pattern_style.json` gained a data-driven line layer and
  a second line feature beside it, one line being unable to tell a per-vertex stream from a
  uniform that happened to be right.
  **Fill-extrusion walls land instanced**, which the encoder note had deferred as "a question
  about the extrusion's geometry that predates patterns". The capture shows two shaders per tile
  and this build emitted one: the roof and ground outline, which is a flat city rather than an
  empty one and wrong in a way that looks deliberate. The walls are a unit quad — four vertices
  for the whole map — with the building's own outline fed to it *per instance*, read out of the
  roof's buffer at the roof's stride. `GeometryAdd` has carried an `instance_attrs` span since R0
  and nothing had filled it.
  Three things had to come first. The generated attribute table had no entry for either instanced
  shader, because mbgl wraps their `using` declarations after the `=` and declares their
  per-instance attributes in a second array the generator never read — the third parser bug of
  that shape here, all sharing a failure mode where a missed declaration produces a smaller table
  rather than an error. `doDepthPass = (!opaque || hasPattern)` was quoted in two comments and
  implemented as `!opaque`, so an opaque patterned extrusion got one pass where the capture has
  two. And `colorBuilder->setEnableStencil(doDepthPass)` was implemented as no stencil at all,
  under a comment whose reasoning was sound and whose fact was wrong — the colour pass tests the
  stencil the prepass wrote, and without a prepass there is nothing to test, which is how both
  halves of that comment can be true at once.
  The drawable dispatch changed with it. It cached one record per bucket and copied it for the
  second pass, which is right for a bucket whose drawables differ only in render state and
  silently wrong for one with two *geometries*; it caches the bucket's records as a list now and
  picks by sub-layer. An extrusion is four drawables — roof and walls, in the depth pass and
  again in the colour pass — where the fill it was modelled on is two.
  The sprite index lands first, as `tessella-glyph/sprite`: mbgl's `SpriteParser`. A style names
  one sprite *base* and the origin serves two resources for it, the suffix going before the
  extension rather than after the URL — `sprite@2x.json`, not `sprite.json@2x` — and a query
  string surviving in front of the suffix, which is what makes a signed sprite URL work.
  Almost all of it is refusal, and that is the part worth having. The index is hand-written or
  tool-generated JSON with no schema behind it, so every field can be wrong in a way that is not
  a parse error: a negative width wraps when it reaches an unsigned rectangle, a zero pixel ratio
  divides by zero, a rectangle running off the sheet samples whatever the neighbouring icon left
  there and looks like the wrong icon rather than like an error. mbgl's bounds are transcribed
  rather than chosen — a dimension over 1024, a ratio outside `0 < r <= 10` — and a bad entry is
  dropped while the sheet is kept, because a style with one broken icon still has three hundred
  that draw.
  The pixel ratio is carried rather than folded into the rectangle: everything downstream
  measures in logical pixels, and folding it in would lose the sheet coordinates the upload
  needs. Stretches and the content box come with it — a route shield is drawn around a label
  whose width was not known when the sprite was made, so the icon says which of its columns and
  rows may stretch — and a range that is not exactly two numbers is refused rather than truncated,
  since taking the first two of `[0, 4, 9]` would read it as `[0, 4]`.
  One inconsistency is pinned rather than papered over: `-1` is a value that parses and is then
  refused, so its entry is dropped, while `1e400` is not a value at all and the parser refuses the
  whole document — so one number JSON cannot represent takes every icon in the sheet with it. The
  two granularities belong to different layers and nothing here can widen the second without
  hand-rolling a number parser. For an index no tool would emit, failing loudly beats half
  loading.
  `icon-image` resolution follows, and it turned up a structural assumption rather than a bug in
  the small: the tile builder resolved `text-field` first and returned early when a feature had
  no name, so a layer with an `icon-image` and no `text-field` produced *nothing at all*. Most
  markers on a map are exactly that. A symbol needs one half or the other, not the text half
  specifically, and the resolvers are separate for the same reason.
  Tokens resolve the same way in both halves and the consequence is not the same. `{name}` as a
  `text-field` on a feature with no name is an empty label and nothing to draw; `{name}-marker`
  as an `icon-image` is the sprite `-marker`, because the token is a `get`, an absent property is
  an empty string, and the surrounding literal survives. mbgl does that too and then misses at
  lookup — so `icons()` is what a layer *asked for* rather than what the sheet has, and a missing
  icon is a layout-time miss rather than a resolution failure. The obvious reading is the other
  one, which is why it is pinned: the two rules look identical until a style writes
  `{name}-marker` and gets an icon it did not mean instead of no icon at all.
  The icon quad follows: mbgl's `shapeIcon` and `getIconQuad`, which are two steps and not one.
  The box is what collision measures and the quad is what draws, and the quad is a pixel larger
  on every side — mbgl's comment says why, and it is not a fudge: a ten-pixel icon that is not
  aligned to the pixel grid covers eleven actual pixels, so a quad sized to the icon clips a
  sliver off one edge. The pad is on the *quad* and not on the texture rectangle, since the extra
  pixel samples the atlas padding the atlas already reserves; padding the rectangle instead would
  sample the neighbouring icon.
  `shape_icon` takes *logical* pixels, which is the unit the pixel ratio exists to produce.
  Handing it the sheet size draws every `@2x` icon at twice its size, and that reads as a broken
  sprite sheet rather than as a unit mix-up — so the conversion is asserted where the two meet.
  The anchor rule is the text one and catches people out the same way: it names the part of the
  icon that *touches* the point, so `top` puts the icon below it. Inverted, every marker sits on
  the wrong side of what it marks, consistently, which looks like a style problem.
  Icons then reach the bucket. The two halves of a symbol are two *drawables* and not one — text
  goes through `SymbolSDFShader` and an icon through `SymbolIconShader` — so they cannot share a
  vertex buffer even when they belong to the same feature, which is why `lay_out_icons` sits
  beside `lay_out` rather than inside it. It is also what `is_text` in the tile props and
  `is_text_prop` in the drawable buffer are for, both of them already checked against the oracle
  before there was an icon to set them for.
  There is nothing to pack. Unlike a glyph atlas, a sprite sheet arrives already laid out and the
  index gives rectangles into it, so the "atlas" *is* the sheet — which is why the layout records
  the name a layer asked for rather than a rectangle: the sheet may not have arrived, and an icon
  it does not have is skipped so a style with one missing sprite still draws the rest.
  Two defaults look alike and are not. `icon-size` is a *multiplier* and defaults to one, because
  a sprite is already the size its author drew it; `text-size` names a size in pixels and defaults
  to sixteen. Reading one as the other draws every marker sixteen times too large, which looks
  like a broken sprite sheet rather than a units mistake, so the two are read by separate
  functions and the defaults are asserted against each other.
  Whether an icon is a distance field is the *sprite's* property and not the layer's. A shield
  drawn as a field is recolourable by `icon-color`; a photographic icon is not, and putting a
  plain image through the SDF shader draws its alpha as a coverage ramp. The flag rides in the
  low bit of the packed size, where the text path already put it.
  **Line-placed icons draw where they accompany a label.** They repeat along a line the way a
  label does and need the anchors `get_anchors` produces; taking the line's first vertex instead
  would place every icon of a road at one end, which draws and is wrong, so they were skipped
  rather than approximated. What they needed was not the anchors — those existed — but to be
  built from the *instances* rather than from the pending symbols. A line-placed symbol is one
  pending and one instance per anchor, and icons were built per pending, so there was nowhere
  for a shield's second repetition to come from. `LaidOut` names its pending now and icons pair
  on it, which also removes a latent fault: the pairing was by position into the pending list,
  which holds only where the two lists are the same length — point placement, the one case that
  reached it.
  **And the icon is shaped before the anchors**, which is the ordering mbgl has and this did
  not. It computes a feature's anchors once from *both* extents — `getAnchors(…,
  shapedText.left, shapedText.right, shapedIcon.left, shapedIcon.right, …)` — so a symbol with an
  icon and no text still gets anchors, from the icon. Laying out takes the sprite index for that
  reason, and two things follow. A `symbol-placement: line` layer carrying only an `icon-image`
  draws: oneway arrows and lane markings, which drew nothing before because a feature with no
  text had no extent, produced no instances, and had nowhere to hang an icon. And the icon's own
  width decides where its repetitions go, since `get_anchors` measures whether a label fits
  between two bends and a wide shield is rejected where a narrow one is accepted — asserted by
  placing the same road with an eighteen-pixel sprite and a four-hundred-pixel one and requiring
  the counts to differ, which zeroing the extent makes fail.
  The sheet itself then decodes, with `zune-png` behind an off-by-default `png` feature — the
  pattern `cache` and `tls` already use, and for a different reason than `tls` has: `zune-png` is
  pure Rust, so unlike `rustls` it costs the cross lane nothing even enabled. It is a feature for
  binary size (DR-12), not for the toolchain. `zune-png` rather than the whole `zune-image` that
  §8 named: a sprite sheet and a raster tile are PNG, and the family's other decoders are bytes
  §12.4 would carry for nothing.
  Everything is widened to RGBA whatever the file's colour type. The rectangles the index hands
  out are in *pixels*, so a decoder returning the source's own channel count would make every
  offset downstream depend on how the sheet happened to be encoded — a greyscale sheet and an
  RGBA one with identical rectangles would sample different things. Greyscale broadcasts across
  the colour channels rather than staying in red, which is the failure that decodes to the right
  place at the right size and draws the wrong colour; RGB gains an *opaque* alpha rather than a
  transparent one, which is the failure that draws nothing at all.
  Not premultiplied. mbgl premultiplies on upload and the capture's texture hash is over the
  decoded image, so doing it here would put different bytes on the wire than the oracle has.
  The test encodes its own PNGs with stored zlib blocks rather than using a second image crate:
  the decoder is the dependency under evaluation, and encoding with another one would make the
  test pass or fail on either.
  The store above it is the icon counterpart of `Fonts`, and simpler in the way that matters:
  there is nothing to pack. A glyph atlas is built by this process out of ranges that arrive
  separately; a sprite sheet arrives already laid out and the index is its map, so the store
  fetches two resources once and holds them. A style has one sprite and every tile asks the same
  question, which is what a second call fetching nothing is for.
  The order inside it is load-bearing and reads backwards: the *image* is decoded before the
  index is parsed, because the index's bounds check is against the sheet's size — a rectangle
  running off the image is refused, and a store that parsed the index first would admit
  rectangles that sample past the end of a texture. A sheet that does not decode leaves no index
  behind either: icons pointing at an image that is not there is worse than no icons.
  The sheet reaches the wire as a *whole-texture* upload rather than a rect list, which is the
  difference from the glyph atlas stated as a format: an atlas fills in as labels arrive and
  changes in small places, while a sheet arrives once, complete, and never changes again — and
  zero rects is what the envelope spells "all of it". RGBA rather than the atlas's R8, since
  §12.4's single-channel argument does not reach a picture.
  `texsize_icon` stops being a hardcoded zero with it. Both texture sizes ride in every drawable
  entry whether or not that drawable uses them, because one shader samples both and the buffer is
  its interface — the same reason the evaluated props carry an icon half for a layer with no
  icons. The symbol capture's style has no sprite, so its zero is a value the oracle carries
  rather than a placeholder, which is why it is passed rather than defaulted.
  Placement then takes both halves. `place` has modelled `text-optional` and `icon-optional`
  since R2 and nothing exercised them, because until now `Candidate::icon` was always `None` —
  which is the kind of gap where every rule agrees with every other for the wrong reason, so the
  first assertion is that the icon half is offered at all. The four combinations are four
  different maps: a shield that vanishes with its label, a label that vanishes with its shield,
  either alone, or both or nothing. Neither optional is the spec's default and the strictest —
  a shield with a number in it is one thing, and drawing the number without the shield is worse
  than drawing neither.
  A symbol with no icon must not be held back by one it does not have, which `icon-optional`
  defaulting to false makes easy to get wrong: most symbols are text-only, and a rule read as
  "the text needs the icon" rather than "the text needs the icon *if there is one*" blanks every
  label on the map.
  `icon-padding` is its own value and not the text's. The spec's defaults differ — two pixels
  around text and one around an icon — and sharing one crowds icons or spaces them depending
  which way it is shared, either of which reads as a collision bug rather than a padding one.
  The sprite work is then checked against maplibre-native's own, which turned out to be on the
  same machine all along — `sprite_parser.cpp` beside its seventeen-test suite and the
  `emerald.png`/`emerald.json` pair those tests read. Reading the source found three places where
  the transcription had been reasoned rather than read, and the direction of two of them is the
  interesting part.
  Every rectangle field is an unsigned sixteen-bit integer with a **default of zero**, not a
  required value: `getUInt16` logs and returns zero for anything absent, fractional, negative or
  above 65535, and the entry carries on to the bounds check with that zero in it. So the *same*
  rule refuses a fractional width and accepts a fractional origin — zero is a fatal width and a
  perfectly good origin — and `{"width": 32, "height": 32}` with no origin at all is a valid icon
  at the sheet's corner, which mbgl's `SpriteParsingSimpleWidthHeight` says outright. This build
  required an origin and refused a fractional one: a style whose minimal entries all vanish, and
  a rule that is closer to what a person would want and further from what the oracle does.
  `pixelRatio` is the odd one and deliberately so — read as any number and *kept*, which is why
  mbgl's own error message for a zero ratio quotes `@0x` rather than `@1x`. Two readers, not one,
  and a fractional ratio survives where a fractional width does not.
  `textFitWidth` and `textFitHeight` were not read at all. An unrecognized value is *absent*
  rather than defaulted, because the three behaviours resize a shield differently and guessing
  between them is worse than not stretching.
  `emerald.json` is now vendored beside the glyph fixtures: a two-hundred-by-two-hundred-ninety-
  nine sheet with seventy-three icons, among them the
  `dlr.london-overground.london-underground.national-rail` family whose names carry dots. A
  hand-made fixture does not have that shape, and a parser splitting names on a separator would
  pass every test written against one.
  The same pass over the rest of the ports found two more things, one wrong and one absent.
  **The sprite sheet is not the texture.** `parseSprite` copies each icon *out* of the sheet into
  an image of its own, and `DynamicTextureAtlas::uploadIcons` packs those into a texture with
  `ImagePosition::padding` plus an extra pixel around each; `ImagePosition::displaySize` then
  takes that padding back out, and the icon quad's one-pixel border samples it. mbgl's own
  `getIconQuads.normal` is the arithmetic that proves it — a 15x11 *padded* rect displays at 13x9
  and quads to a 15x11 box. This build had drawn straight from the sheet on the reasoning that a
  sheet is already laid out, which is true and beside the point: a sheet has no padding between
  icons, so the border sampled the neighbouring picture and every marker on the map carried a
  hairline of the wrong icon. Icons are cut and repacked now, into an RGBA atlas using the same
  `ShelfPack` and the same two-reserved-one-reported padding the glyph atlas already used — which
  the same pass confirmed was right, since our reported rectangle for a 24-pixel glyph is 32 and
  so is mbgl's.
  **Clip masks are absent, and the golden could not have caught it.** mbgl's `updateTileMasks`
  gives each rendered tile the set of sub-tiles to draw, so a parent under one child draws the
  other three. Every tile in every capture is `o13` at its own zoom — substitution never happens
  in a settled frame at a fixed camera — so the case has never been captured, while
  `sweep_never_blank` exercises it on this side several times a run. Recorded rather than fixed,
  and named here because the next reader will otherwise conclude from a green golden that masks
  are handled.
  *This paragraph originally went on to call it a wire question — `StencilTiles` carries a tile
  and a matrix and has no word for a quadrant, so a mask looked like a field addition against a
  frozen ABI. That was wrong, and reading the two consumers is what settled it; the correction is
  below, where the masks land.*
  The audit then found a third: **`mergeLines` was missing entirely.** mbgl runs it on a symbol
  layer's features whenever `symbol-placement` is `line`, before any anchor is chosen, and it
  joins features that share an endpoint *and* say the same thing. A road is rarely one feature —
  a tile cuts it at its edges and a source cuts it wherever an attribute changes, a speed limit
  or a surface or a bridge — so "Main Street" arrives as a dozen stubs laid end to end. Without
  the join each stub is labelled separately, and most are *dropped*: a stub shorter than its own
  label cannot hold one. That is why the street fixture produced so many fewer labels than it has
  roads, a number that had been read as the fixture being short of long roads.
  Ported with mbgl's own `MergeLines.*` expectations, coordinate for coordinate, because the
  merge is order-dependent: joining the same set in a different order gives different *lines* —
  the same points distributed between features differently — and every anchor moves. The
  three-way case is the one to get exactly, and getting it half right is the easy failure: this
  build joined one end and left the other, which is indistinguishable from correct on any fixture
  where only one side touches. mbgl passes the *already merged* line to the second join, and
  passing the original appends nothing because its points have already moved.
  One property is worth stating because it looks like a bug: the merge is **not idempotent**, and
  running it twice joins more. The index holds one entry per text and endpoint, so where two
  roads of the same name start at the same place only one is reachable — the street fixture has
  fifty such junctions. mbgl's index is an `unordered_map` assigned into and overwrites the same
  way, so one greedy pass is the oracle's behaviour. Running to a fixed point would be a
  divergence, and a *silent* one, since the extra joins look like better labelling rather than
  like a difference.
  The sweep then reached the rest of R0–R2, and most of it came back clean. Expressions were
  already checked against the 350-case spec suite rather than against mbgl, which is a stronger
  oracle for that piece. `GridIndex`, the cross-tile index, `getAnchors`, `TileCover`'s geometry
  cases and `GlyphPBF` were already ported from mbgl's own tests. The glyph atlas's padding turns
  out to match exactly — a 24-pixel glyph reports a 32-pixel rectangle here and in mbgl — which
  is what made the icon path's missing padding legible as a divergence rather than a choice.
  `replaceTokens` is used by mbgl **only** for URL templates, never for `text-field`, which
  confirms the earlier token decision from the other side: the URL rule leaves an unknown token
  literal, this build's URL path already did, and `text-field` correctly uses the `get` rule
  instead. `Filter.ID`'s thirty-odd assertions all hold — `$id` is type-strict in both directions
  and a property named `id` does not answer to it.
  One more divergence turned up, in bounds cover. mbgl's `TileCover.Arctic` expects *nothing* for
  a box between 86 and 90: Mercator stops at 85.051129, so a box beyond it names no ground the
  pyramid has. This build clamped it into the world, which is right for a box that *reaches* the
  pole from below and wrong for one that never enters — clamping collapses it to a zero-height
  strip on the top row, and the degenerate-box rule then inflated that into a row of tiles nobody
  asked for. The two are separated now: a box crossing into the world is clamped, one lying
  wholly outside it covers nothing.
  The degenerate box itself stays a deliberate divergence. mbgl's `SingletonZ0` expects nothing
  for a zero-area bounds; this answers with the tile under it, because the two functions are used
  for different things — mbgl's is a viewport cover, where a zero-area viewport draws nothing and
  that is the whole of it, while this one sizes an offline region, and a user dropping a pin
  means the tile under the pin. It is named in the test now rather than reasoned, so a future
  diff reads it as a decision.
  `icon-text-fit` then lands, which is what draws a route shield: a sprite whose middle stretches
  to hold a number and whose border must not stretch with it. Two mechanisms and not one, which
  is the thing to keep straight — the *layer* says `icon-text-fit`, which axes may stretch, and
  the *sprite* says `textFitWidth` and `textFitHeight`, how far that stretch may distort it. mbgl
  keeps them in two functions and so does this.
  Fitting deliberately ignores the icon's anchor, which mbgl says outright: `icon-text-fit` is a
  statement about where the icon sits relative to the *text*, and honouring the anchor as well
  would move it off the label it is drawn around. An axis that is not fitted is *centred* rather
  than left alone, which is the branch it is easy to read as a no-op — without it a width-fitted
  shield stretches across its label while sitting above it.
  `applyTextFit` corrects the aspect afterwards, and only a `proportional` axis does anything:
  with both `stretchOrShrink`, or with neither field set, the content rectangle already matches
  the content. Checked against mbgl's own numbers — a 100x20 sprite with a 5,5,95,15 content box
  fitted to a square comes back 144 by 16, its content's nine-to-one aspect restored — and the
  vertical branch against its mirror, since the two are written out separately and one can be
  right while the other is not.
  Placement sees the other half. A sprite with a `content` box carries *margins* between that box
  and its own edges, and they grow the collision box: once fitting has stretched a shield around
  its label the extent is the shield's content area, and the drawn picture reaches further out by
  its border, which is what has to be reserved. The margins are in the sprite's own pixels and are
  divided by the pixel ratio, so a 2x shield's border is the same size on screen as a 1x one's;
  and they scale with the box where `text-padding` does not, which is why the two are separate
  arguments rather than summed.
  Wiring it through the layout took a correspondence that had quietly stopped holding. An icon
  has to find *its own* label to be fitted to it, and the obvious index — position in the
  laid-out list — stopped matching position in `pending` the moment icon-only symbols became
  possible, because `lay_out` skipped the ones that shape no text. Nothing failed: the styles in
  the tests all had text for every feature. So `lay_out` now answers one entry per pending for
  point placement, with an empty extent where a symbol has no text, and an empty extent places as
  nothing — which is what a symbol with no text should reserve anyway.
  Point placement only. A *line* label is laid out once per repetition along its road, so there
  is no one-to-one to keep and nothing needs one: icons are point-placed. Making the line path
  one-to-one as well silently dropped every repetition after the first, which showed up as the
  spacing test's label count halving rather than as anything about icons.
  `icon-text-fit-padding` is in the spec's CSS order — top, right, bottom, left — and not the
  top-bottom-left-right the extents here use. Reading one as the other rotates the padding a
  quarter turn, which is a shield fatter above than beside its number.
  The atlas then gets looked at, which is the point of having a rasterizer behind `#[ignore]`.
  Nothing about the icon *pixel* path had been seen — the audit found its one bug by reading
  mbgl, and a packer that shears a row, drops a channel or mislays the padding produces
  arithmetic that checks out and a picture that does not. mbgl's own `emerald` sheet packs to
  seventy-three icons: shields, pins and roundels, colours intact and unsheared. It is drawn over
  a chequerboard rather than a ground, so the transparent padding reads as padding — a solid
  ground would make a dropped alpha channel look correct, which is the failure most worth seeing.
  A security pass over the untrusted decoders then puts a stated ceiling on every one of them.
  Every byte this build parses came off a network and a source is not a trusted party — an origin
  can be hostile, compromised or merely wrong, and a plain-HTTP one can be anybody on the path.
  `forbid(unsafe_code)` holds in all ten crates, so the risk is not memory corruption; it is
  *allocation*, and on a device-class target an out-of-memory rather than a slow frame.
  Three decoders, in three different states, and the difference is worth recording. The HTTP body
  was already bounded — at ten mebibytes, by `ureq`'s own default rather than by anything here,
  which is a bound a dependency bump could remove with nothing saying so; it is stated explicitly
  now. The PMTiles gzip path was genuinely unbounded: `read_to_end` on an attacker-supplied
  member, where a few hundred bytes expand without limit. And the PNG path was bounded by
  `zune-png` at 16384 square, which is a *gibibyte* of RGBA from a file of sixty bytes — inside
  the letter of a limit and far outside anything a sprite sheet is.
  All three refuse rather than truncate. A short tile decodes as a protobuf wire error several
  steps from the cause, which is the failure this crate's own notes already argue against, and a
  caller seeing one could not tell a bomb from a corrupt archive. The sheet is checked from its
  *header*, before a pixel is decoded, so refusing costs a parse of twenty-five bytes rather than
  the allocation being asked for.
  The tests build real bombs rather than asserting the constants: a gzip member of under sixty-four
  kilobytes that expands past ten mebibytes, and a PNG under a hundred bytes claiming eight
  thousand square. The second is deliberately sized to sit *inside* `zune-png`'s own cap and
  outside this one — a larger header is refused by the decoder instead, which would let the test
  pass without exercising the bound it is about.
  Every parser that reads bytes off a network then gets run against bytes it was not written for.
  Not `cargo-fuzz`: libFuzzer needs nightly and DR-17 pins this workspace to the stable toolchain
  the target's Yocto release carries, so a fuzz target CI cannot run is a fuzz target nobody
  runs. The mutation happens in a test instead — deterministic, seeded by a constant, and going
  with every commit. A weaker search that runs a thousand times more often, which for the
  failures this is about is the better trade.
  **`fuzz/` now holds the other half**, in its own workspace with its own nightly toolchain so
  that nothing in an ordinary build reaches it. Three targets, and the third earns its place
  differently from the other two: a vector tile and a style document go deeper into parsers the
  harness already covers, where `capture_ring` is new ground — the bytes are the one input
  another *process* writes, and every number the consumer walks is one it reads rather than one
  it computed.
  Its first version handed `attach` raw fuzz bytes and got four new coverage units in two hundred
  thousand runs: every input failed the ABI-revision check at the front door, so the walk under
  test never ran. What is interesting is not that `attach` rejects garbage, which a unit test
  asserts once, but what the walk does with records — so the control block is well formed by
  construction and the fuzzer owns the data region. Fifty-eight units on the same budget, and the
  walk is capped so that a record consuming nothing reports as a named failure rather than as a
  timeout.
  CI runs each target for a minute, which is a smoke test rather than a campaign. What it buys is
  that the targets still build and still return — the failure that actually happens to a fuzz
  target nobody runs.
  The contract is *return, either way*. `forbid(unsafe_code)` holds across all ten crates, so a
  malformed input cannot corrupt memory; what it can do is panic — which on a worker takes down a
  tile build, and which a hostile origin can then trigger at will — or allocate from a number it
  was told rather than one it checked. On a device with one map and no supervisor to restart it,
  both are denial of service.
  Seven parsers: the vector tile decoder and a walk of what it returns, the glyph range, the
  sprite index, the sprite sheet, GeoJSON, the style document with its filters and paint
  resolution, and the PMTiles header. Nothing panicked, which is a result worth being careful
  about — a harness that has never caught anything may be one that cannot. So the harness is
  tested too: it is handed a parser that panics on a byte the mutations reach, and asserted to
  report it. `catch_unwind` is doing the swallowing, and one that caught a panic and forgot to
  re-raise it would be silent about every real one.
  The seeds are the *conformance* fixtures rather than real tiles, and not only because it cut
  the run from thirty seconds to eight. A bit flip in half a megabyte lands on a coordinate
  almost every time; in fifty bytes it lands on a tag, a length or a geometry command, which is
  where a decoder breaks. One real tile is kept beside them for the shapes a hand-made fixture
  does not have.
  The camera then stops refusing rotation. `proj_matrix` answered `CameraError::Rotated` for any
  bearing or pitch, which made the whole build an unrotatable map — and cascaded: every pitched
  path written since was dead code, the map-aligned label matrices were left out for want of
  anything to check them against, and six of mbgl's own tile-cover cases could not be run.
  The missing piece was mbgl's orientation quaternion, and it is written out rather than taken
  from `glam` for the reason the rest of this module is: the order the terms accumulate is the
  quantity being reproduced. Bearing and pitch are *negated* and roll is not — mbgl's comment
  explains it as a clockwise rotation about each axis, and the asymmetry is real, since the first
  two describe where the map points and the third describes the camera. The order is bearing,
  then pitch, then roll, and the product does not commute: pitching a rotated camera is not
  rotating a pitched one.
  The far plane is the rest of it, and the whole of what pitch changes about the frustum. Tilting
  puts the top of the screen further away than its centre, so the far plane must reach it or the
  horizon is clipped; the reach is `tan(fov/2)` in the units mbgl uses, and the pitch's tangent
  turns it into a fraction of the distance the top edge adds. Two clamps, and neither is
  redundant: `MAX_PITCH` at 89.25 degrees bounds the *angle*, and 0.99 bounds the *arithmetic*,
  because at ninety degrees the top of the screen is the horizon and the far distance diverges.
  What stands in for the capture that does not exist is that the unrotated path is *unchanged*.
  The quaternion is exactly the identity at zero bearing and zero pitch, so the rotation matrix
  is exactly the identity matrix and the far plane collapses to the centre distance — every
  golden still holds to the bit, and the rotated path is the same arithmetic with a rotation that
  is no longer the identity. That is weaker than a diff against a rotated dump and it is not
  nothing: it says the change added a term rather than moved one.
  `CameraError` keeps a variant, and a better one. Nothing produced `Rotated` any more, and a
  `Result` that can never be `Err` is a lie in the signature — so it is now `EmptyViewport`, which
  guards a division by zero that was never guarded and which mbgl returns early on. Including a
  viewport that is *not a number*: a resize arriving mid-flight is where one comes from, and the
  guard is a negated `>` because `<= 0` accepts a NaN and would put one in every matrix of the
  frame.
  The label planes follow it, and the map-aligned branch that was left out for want of a camera
  now has one. The two branches are different *kinds* of matrix, which is the thing to hold on
  to: a viewport-aligned label is laid out in screen pixels, so its plane is a projection and
  carries the tile matrix, while a map-aligned one lies flat on the ground and is laid out in
  tile units, so its plane is a *scale* and carries no camera at all — the tile matrix already
  places it. Folding the projection into the second would place the label twice.
  `text-pitch-alignment` and `text-rotation-alignment` are separate properties because a label
  can lie flat *and* stay upright, which a road name on a tilted map is; so the bearing is undone
  in the plane when the label does not turn with the map, and left alone when it does.
  What holds the two halves together is that they are inverses *through the tile*: a point taken
  into the label plane and back must land where it started. That is the assertion worth having,
  because getting one of the two rotations' signs wrong satisfies every structural check —
  right scale, right zeros, right translation — and fails only this. Verified by flipping the
  sign, which failed it alone.
  One hazard is written down rather than fixed: `pixels_to_tile_units` here and `ubo::line_ratio`
  compute reciprocals of the same quantity, one through the system libm this crate links against
  and one through the `libm` *crate* the `no_std` one uses, and the two are free to round
  differently in the last bit — so they are folded into one: `line_ratio` is now the reciprocal of
  `pixels_to_tile_units`, and a test pins them as one quantity at fractional zooms as well as
  whole ones. At a whole zoom the exponent is exact and any two implementations agree; the
  fractional case is the one that separates them, and it is the case `composite_style_z13_5`
  captures. Every golden held across the change, which is what says the two routines happened to
  agree today rather than that it did not matter.
  `glam` went with it. §8 listed it for `DMat4`/`DVec` and nothing ever used it — the matrices are
  hand-written `[f64; 16]`, because the order the terms accumulate *is* the quantity being
  reproduced and a library's multiply is free to associate differently. Three crates carried the
  dependency for nothing, which is a §12.4 cost against no benefit. The same reasoning rules out
  a faster transcendental library, and the profile says it would not be worth having anyway:
  thirty of the thirty-eight transcendental call sites are in the camera, the projection and the
  cover, which run per view per frame rather than per vertex.
  Symbols then respond to the camera that can now rotate. `text-rotation-alignment` and
  `text-pitch-alignment` both default to `auto` and resolve in two steps whose *order* is the
  whole of it: rotation goes first and takes `map` for a line-placed symbol and `viewport` for a
  point-placed one — a road name follows its road, a town name stays upright — and pitch then
  *inherits what rotation became*. Resolving pitch first gives every line label a viewport pitch
  and lays none of them flat on a tilted map, which is a plausible-looking map that is wrong.
  Three things follow from the pair and each is a different mechanism turning the same symbol,
  which is why doing two of them is a double rotation rather than a stronger one. A label walked
  along a line gets the *identity* label plane, because the projection does the walk along the
  projected road and a plane would bend it before the walk bent it again. A label lying flat is
  turned by the map-aligned plane. What is left — turning with the map while standing up on
  screen — is the only case the *shader* turns, which is mbgl's `rotateInShader` and what
  `rotate_symbol` had been hardcoded false for.
  `along_line` is both conditions and not just the placement: a line-placed symbol that does not
  rotate with the map is drawn upright at each anchor rather than following the road, so it is
  not walked.
  `gamma_scale` stops being one. A label lying flat and pitched away covers fewer screen pixels
  than it was laid out for, so a fixed distance-field ramp is sampled across too few of them and
  the text thins to nothing at the horizon; the cosine of the pitch times the camera distance
  widens the ramp to match. It is the correction a mipmap would make, done in the shader because
  a distance field has no mip levels to choose between. One for a label standing up, whose glyphs
  are the size they were laid out at.
  The capture's style names neither alignment and is point-placed, so both resolve to viewport —
  which is the branch every golden pins, and the reason they all still hold now that the other
  branch exists.
  The last of the pitch path is the collision prefix, and it turns out to be two mechanisms that
  had been described as one. `calculateTileDistances` says how far along its line each vertex sits
  from the anchor — a *reach* in each direction rather than a signed position, so the run is a
  valley with its floor at the anchor's segment, which is what lets placement take the prefix a
  label actually covers instead of its whole run. Ported with mbgl's three expectations and the
  property none of them alone pins; an anchor naming a segment the line does not have answers zero
  throughout rather than panicking, because the anchors and the line reach it from different
  places and a line can be merged or clipped after an anchor was chosen.
  The thinning is the half that pays now. A run's circles overlap by construction — they step by
  half a box so the run is a covering rather than a dotted line — so adjacent circles are often
  nearly coincident on screen, most of all where a pitched map squeezes the far end of a road into
  a few pixels. mbgl drops one when its centre is within √2 radii of the last kept, with two rules
  that are not optional: never two in a row, and never the last. A run that thinned itself away
  would reserve a single point of the road it covers, and the *end* of a label is where it meets
  the next one, so dropping the final circle is how two labels come to overlap at their ends while
  every circle between them was tested.
  Both the test and the reservation use the thinned set. Reserving every circle while testing a
  thinned one would make a label block more than it checked against, which reads as a map that
  thins out as it fills rather than as an asymmetry.
  Bidirectional text then stops being wrong. `unicode-bidi` had been a declared dependency of
  `tessella-layout` that nothing called, so Hebrew and Arabic were laid out in the order they are
  *stored* rather than the order they are read — every such label drawn backwards. That is not a
  missing feature but a wrong answer, and the kind a reader who does not know the script cannot
  see: every letter is correct and the word is not.
  Runs, not characters. A line is cut into stretches of one direction and the stretches laid out
  in visual order, each right-to-left one reversed within itself. Reversing the whole line instead
  puts an embedded Latin word or a number backwards — which looks *nearly* right, and is what a
  simpler implementation does; mbgl's own `ReverseArabic` is that case, since digits are their own
  bidi class and lead the display line while the letters around them do not.
  Reordering happens after line breaking and not before, which is mbgl's order and its note:
  breaking is decided on the logical order. And it carries each character's *advance* with it —
  the trap in reordering a shaped line rather than a string, since a reorder that moved codepoints
  and left the widths behind would set every right-to-left label with its letters spaced by their
  neighbours' widths.
  A line the algorithm leaves alone is borrowed rather than rebuilt. Most labels on most maps are
  left-to-right and shaping runs per label per tile, so that is the difference between the pass
  being free for them and costing an allocation each.
  Arabic *shaping* — the contextual letter forms, mbgl's `applyArabicShaping` over ICU's
  `u_shapeArabic` — was the remaining half, and lands here. Reordering alone left each letter in
  its isolated form rather than joined to its neighbours; mbgl's `BiDi.ArabicShaping`, `Tashkeel`
  and `MixedShaping` state the exact strings, and they pass.
  Arabic is written joined: which of a letter's four shapes is drawn depends on whether the
  letters either side join to it, so the same letter is four different pictures and text is
  *stored* as none of them. A renderer drawing the stored forms produces something a reader can
  decipher and no reader would call written Arabic.
  The table is generated, not written — `tools/unicode-codegen/arabic_shaping.py`, in DR-6's
  discipline. Seventy-six letters times four forms plus the joining types that select between
  them is a table recalled rather than read, and one that would pass mbgl's three strings while
  being wrong about the rest of the alphabet. Both halves are in the Unicode Character Database:
  `ArabicShaping.txt` gives the joining types and `UnicodeData.txt` gives the forms, as the
  `<isolated>`/`<initial>`/`<medial>`/`<final>` decompositions of Presentation Forms-B.
  Two things reading the data taught that guessing would not have. The ligature substitution is
  the four *lam-alefs* and nothing else: `<isolated> 0644 ...` also matches lam-jeem, lam-hah and
  the rest of Presentation Forms-A, which `U_SHAPE_LETTERS_SHAPE` leaves as two letters — taking
  them draws ligatures ICU does not. And joining type is a **different question from having
  forms**. Every diacritic is Transparent and has no presentation forms of its own — `FE76 ARABIC
  FATHA ISOLATED FORM` decomposes to a *space* and a fatha, two code points, so it is rightly
  absent from a forms table — while Unicode lists only five transparent characters explicitly and
  *derives* the other two thousand from General_Category. Reading the joining type off the letters
  leaves every mark non-joining, and a mark then breaks the join around it: every voweled word
  comes apart while unvoweled text stays perfect, which is most of a Qur'anic inscription and none
  of a road sign.
  Shaping runs before breaking and breaking before reordering, which is mbgl's order and each step
  depends on the one before: the forms come from *logical* neighbours, so reordering first joins
  every letter to whatever ended up beside it on screen. A lam-alef consumes two characters for
  one, so the rewrite walks input and output in step rather than matching codepoints — a
  presentation form does not equal the base it came from.
  Two of this build's own expectations were wrong and were corrected rather than the code: a
  medial form needs a join on *both* sides, and the letter after a lam-alef stands alone because
  an alef is right-joining and does not join forward.
  The raster layer lands next, and it is the largest functional hole rather than the largest
  piece of work: without it a satellite or hillshade basemap cannot be shown at all, and those
  are most of what a style names a raster source for.
  The geometry is a rectangle. mbgl's `RasterBucket` builds one quad per entry of the tile's clip
  mask, which with no mask is the whole tile, and each vertex carries *two* coordinates — where
  it sits in the tile and where it samples the image. They are the same numbers for a whole-tile
  quad and are still two attributes, because a masked quad covers a quadrant of the tile while
  sampling a quadrant of the image and the two rectangles stop agreeing the moment a parent
  stands in for a missing child. Keeping them apart now is what makes the mask a caller passing
  one later rather than a rewrite; the mask itself is still the ABI question recorded above.
  A raster layer draws with **no features**, which is the way it differs from every other layer
  here. Fills, lines and symbols all build from a tile's features and produce nothing when there
  are none — a rule that, applied to a raster tile, draws no imagery ever, since a raster tile
  carries no features to build from. The layer arm is in both tile builders for that reason.
  The colour adjustments are the part worth transcribing rather than deriving. Each is a *factor*
  and not the property, and two of the three are asymmetric: reducing saturation or contrast is
  linear while raising either is a reciprocal that runs away as it approaches its limit. Read as
  symmetric — one multiply either way — the picture is nearly right at small values and visibly
  wrong at large ones, which is a defect nobody reports until a style leans on it. The `1.001` in
  the saturation branch is a bound in the arithmetic rather than in the property, and it is what
  keeps the property's own maximum finite. Hue rotation is a rotation about the grey axis of the
  colour cube, so its weights sum to one at every angle: a version that normalised wrongly
  brightens or darkens as the hue turns, which reads as a broken image rather than a broken
  rotation, and is asserted as the sum rather than as three numbers.
  `RasterEvaluatedPropsUBO` is transcribed offset by offset against the header's own comments,
  because the buffer is the shader's interface and a value in the wrong slot is read as a
  different property — a saturation read as a brightness — and produces a picture instead of an
  error. `tl_parent`, `scale_parent` and `fade_t` describe a tile fading in over the parent
  standing in for it, and are written as *not fading*: a still frame is what every capture is,
  and a value invented for the transition would be a number on the wire nothing produced.
  Painter order takes it as translucent whatever the opacity says, which is `render_raster_layer`
  and not an approximation of it — a raster tile has no interior to depth-test against, and an
  opaque pass would let a tile drawn later fail the test against one drawn earlier at the same
  depth. None of its eight paint properties is data-driven, and that is structural rather than a
  gap: a raster tile is an image rather than a set of features, so there is no feature for a
  property to vary over, which is why the layer has no paint binder while every other tiled layer
  does.
  The layer alone draws nothing, though, because nothing fetched a raster tile: `Source::Raster`
  fell through source resolution, so a satellite basemap resolved to no templates and asked for
  no tiles. Closing that is a decoder, a cover, and a builder.
  **The decoder reads JPEG as well as PNG, and that is not an extra.** Satellite imagery is
  photographic, and a photograph stored losslessly is several times the bytes for a difference
  nobody looking at a map can see — so every commercial imagery source serves JPEG, and a build
  that reads only PNG draws no satellite at all. The failure is silent, too: the tile fetches
  successfully and then does not decode. Terrain shading and label-free overlays go back to PNG
  for the alpha, so the two are not alternatives to pick between; a real style uses both.
  The format is sniffed from the bytes rather than taken from the URL or a `Content-Type`. A tile
  template ends in `.png` for plenty of sources that serve JPEG behind it, and a header can be
  absent, wrong, or `application/octet-stream`. The first eight bytes cannot be any of those.
  WebP is the third mbgl reads and, at this point, the one this build did not; `tile.webp` was
  vendored so the refusal could be asserted against a real file, and it lands a few paragraphs
  below.
  Sprite sheets go through the same decoder now rather than a second copy of it. The bound
  against the header, the widening to RGBA and the sniff are the same questions for a sheet and
  for a tile, and answering them twice is how two answers drift apart.
  **The decode premultiplies, and it did not before.** mbgl's `decodeImage` returns a
  `PremultipliedImage`; this build's style colours are stored premultiplied and its shaders are
  mbgl's, so an image that is not premultiplied was the odd one out in a pipeline that assumed
  otherwise. Left straight, an icon's anti-aliased edge blends its own colour at full strength
  against the background and draws a bright fringe around every marker that fades out — invisible
  on the opaque sprites that are most of a sheet, and wrong everywhere else. mbgl's rounding is
  transcribed with it: `(c * a + 127) / 255` is a round-to-nearest where `c * a / 255` truncates,
  and the two differ by one over most of the range, which is a diff on nearly every translucent
  pixel of a sheet compared byte for byte.
  The oracle's own `test/fixtures/image` is vendored and its `image.test.cpp` numbers
  transcribed. The profile/no-profile pair is the assertion worth having: mbgl expects the *same*
  pixel from both, so a decoder honouring an ICC profile would colour-manage one tile of a
  basemap and not its neighbours, and the seam between them reads as a bug in the tile server.
  **A raster source is covered at its own zoom, not the map's.** mbgl computes `tileCover` per
  source with that source's `coveringZoomLevel`, which shifts by `log2(512 / tileSize)` and
  *rounds* where a vector source floors. A 256-pixel source — which is what most imagery services
  serve — therefore needs one level more to fill the same screen, and covering it at the map's own
  zoom fetches imagery at half the resolution of the labels drawn over it: a blurry basemap
  rather than anything that reads as a cover bug. So `cover_at` takes a stated level and the job
  planner became source-major, since the tiles are no longer one list.
  Which turned up a defect underneath it. `tileSize` had **never been read**: the spec's source
  keys are camelCase where a layer's properties are kebab-case, and serde needs told per field
  because most source keys are single words needing no rename — so the key fell into `extra` and
  the field was `None` for every style ever written. Not a parse error, and indistinguishable
  from a style that stated nothing. `clusterRadius` and `clusterMaxZoom` were the same and are
  fixed with it.
  **Clustering itself is built**: `supercluster.hpp` and the `kdbush.hpp` it stands on,
  transcribed and checked against supercluster's own expectations over maplibre-native's
  `places.json`. A transcription rather than any clustering, because the grouping is a property
  of the whole construction and not of the radius — the index's visit order decides which cluster
  absorbs a point, so two implementations that both group within a radius draw different maps.
  The expectations reach through all of it: thirty-nine features standing for a hundred and
  ninety-six points in the world tile, a named cluster's four children in the index's own order,
  five expansion zooms, and ten leaves from an offset of five. What is not built is
  `clusterProperties`, the map/reduce pair that accumulates arbitrary fields into a cluster; the
  style layer does not parse it either, so a hook would have no caller.
  It is wired at source resolution rather than per tile, which is what the index is for: the
  levels are built deepest-first from the whole document once, and every tile of every zoom is a
  range query into them. Building one per tile would cluster the world once per tile of the
  cover. A tile's features are then the clusters at *its* zoom, handed over as ordinary points in
  longitude and latitude — so they project and clip through the same tiler every other source
  uses, and `point_count` is a property a style draws with like any other. One difference from
  mbgl in that: supercluster's own `getTile` buffers by the cluster radius and returns tile
  units, where this takes the same set and lets the tiler clip to its own buffer, so which points
  just outside a tile survive is decided by the rule every other source obeys rather than by a
  second one. The round trip is asserted as well
  as the read: an offline region records the style it pinned, so a rename that reads correctly
  and writes `tile_size` produces a document nothing else can read back.
  The builder is a third one rather than an arm of the other two. The existing pair take features
  and differ only in what a coordinate means; this takes none, and every stage they share —
  filter, classify, tessellate, bind — has nothing to act on. It is the same fact that makes a
  raster layer draw with an *empty* source tile where a fill does not: a fill with no features has
  nothing to draw and correctly draws nothing, while a raster tile with no features is every
  raster tile there is. A raster layer pointed at a *vector* source therefore draws nothing at
  all, which is the only correct answer — its picture is its source's tile, and emitting a quad
  anyway would put geometry on the wire sampling a texture nothing uploaded.
  The picture rides with the geometry and is shared between the layers built from it. A style
  drawing one imagery source twice — a base pass and a tinted overlay — is two buckets over one
  image, and a raster tile is a quarter of a megabyte (§11.5).
  A 404 imagery tile is a hole and not a failure, as a vector one is: coverage is not a rectangle,
  and a style should not fail to start because a corner of the screen is outside a survey. An
  undecodable body *is* reported, though, and the distinction is the point — an origin serving an
  error page with a 200 is a real failure mode, and treating those bytes as an empty tile would
  draw a hole and report success.
  What is still missing is the wire: the texture upload and the `texture_refs` that bind it. That
  gap is not the raster layer's — the glyph atlas has it too, and `TextureRef` has never been
  populated by anything but `whole_stream`'s hand-built records — so it is one piece of work for
  both rather than a raster one, and it is what lands next.
  Textures then reach the wire, which closes a gap that was never raster's alone. `TextureRef`
  had been in the ABI since R0 and populated by nothing: `texture_refs` was `Span::default()` on
  every drawable this build emitted, so the glyph atlas had the same hole. A drawable naming no
  texture binds nothing, and the tile draws as whatever the consumer last had in that slot.
  A texture's *slot* belongs to the shader, not to the texture, and that is what makes this a
  DR-6 table rather than three constants. The glyph atlas is slot 0 of `SymbolSDFShader` and slot
  0 of `SymbolTextAndIconShader`; the sprite atlas is slot 1 of the second and has **no slot at
  all** in the first. A producer that remembered "the icon atlas is slot 1" would bind, on an SDF
  drawable, a texture that shader has no sampler for — a label with no glyphs rather than an
  error. So `texture_slots.rs` is generated from `shader_defines.hpp`'s anonymous enums and the
  `TextureInfo` arrays in `src/mbgl/shaders/vulkan/*.cpp`, exactly as the attribute tables are.
  Two independent agreements with the oracle come out of it, and neither was fitted to: the
  table says `SymbolSDFShader` has one sampler at slot 0, and `symbol_style.dump` gives every
  symbol drawable exactly one `tex ... slot=0` line; the table says a plain fill has none, and
  the dump's fill drawables carry no `tex` line at all. A build that bound nothing would satisfy
  the second and fail the first; one that bound something everywhere would do the reverse.
  The distinction between *no samplers* and *no table* is kept, because an empty slice cannot
  make it. A fill shader genuinely samples nothing — mbgl writes `std::array<TextureInfo, 0>` —
  while a shader missing from the generated match would also answer with an empty slice and mean
  that generation had missed it. `texture_count` returns `Option` for that reason, and the
  parser recognises the one-line empty form as a table rather than skipping it.
  Supplying too few textures is refused rather than truncated. A shader's samplers are all of
  them or none: what a shader reads from an unbound sampler is the backend's business rather
  than a defined black, so a drawable missing one cannot draw and a prefix is not a lesser
  version of the right answer.
  Which is exactly the raster case, and the one that looks like a bug in mbgl and is not.
  `RasterShaderSource` declares *two* textures and `render_raster_layer.cpp` sets the same image
  to both: slot 1 is the parent tile a fading tile blends against, and with no fade in progress
  it is the tile's own picture. Binding only slot 0 would leave the second sampler reading
  whatever the backend left there.
  A raster tile uploads whole, like a sprite sheet and unlike a glyph atlas — it arrives
  complete and is never touched again, and a *new* tile is a new texture rather than a region of
  an old one. One texture per tile rather than one atlas per source, which is mbgl's arrangement
  and is forced rather than chosen: tiles arrive and are dropped on their own schedules, and
  packing them into a shared atlas would make evicting one a repack of the rest. It is also why
  the binding belongs on the drawable — every raster drawable samples a different texture, where
  every symbol drawable of a style samples the same atlas.
  RGBA, not the atlas's single channel. §12.4's argument does not reach a picture, and a raster
  tile may carry alpha — a label-free overlay, a hillshade, a corner outside the survey — which a
  format that dropped it would draw as opaque black rather than as nothing.
  The last assertion is a protocol one and invisible to any test of a single function's return
  value: the upload reaches the ring *before* the geometry that names it. The ring is ordered and
  the consumer acts on records as they arrive, so a `GeometryAdd` carrying a `TextureRef` the
  consumer has seen no upload for binds nothing at all.
  WebP closes the last of the three formats a basemap is served as. It is what a source reaches
  for when it wants photographic compression *and* an alpha channel — MapTiler and Mapbox serve
  their satellite and hybrid layers as it — and it is the format a URL is least likely to admit
  to: a `.png` template answered with WebP behind a content-negotiating CDN is an ordinary
  arrangement rather than a misconfiguration, which is the case the byte sniff was written for
  and is now asserted end to end.
  `image-webp` is the decoder: pure Rust and `forbid(unsafe_code)`, like the zune pair. It sits
  behind a feature of its own rather than inside `image` for two reasons that are not the same
  reason. A VP8 decoder is a great deal more code than a PNG one, so DR-12 bites hardest there;
  and it reads through `std::io` where the zune decoders take cursors of their own, making it the
  one decoder that pulls `std` into a crate that is otherwise `no_std`. That is declared at an
  `extern crate std` inside the function rather than left to happen, so the feature reads as a
  decision about the crate's discipline and not only about its size.
  The RIFF signature is checked in both halves — `RIFF` at the start *and* `WEBP` at offset
  eight. `RIFF` alone is a container tag shared with WAV, AVI and a dozen other formats, so
  matching on it would hand a sound file to the image decoder and report a decode failure where
  "not an image" is the truthful answer.
  Two behaviours are chosen rather than inherited. An *animated* WebP decodes to its first frame
  rather than being refused: a raster tile is a picture of the ground, the texture behind it
  holds one image, and there is no frame clock in this pipeline to advance a second one with —
  so refusing would drop a tile whose first frame is perfectly usable. And chroma is upsampled
  bilinearly, which is `image-webp`'s default and libwebp's, and therefore what mbgl gets; the
  alternative is faster and leaves jagged edges along every colour boundary.
  The fixture turned out to be a better oracle than its own test claims. mbgl's `image.test.cpp`
  asserts the size of `tile.webp` and nothing more, but the file is a *lossy* `VP8 ` frame inside
  an extended `VP8X` container with an EXIF chunk beside it — the harder of the two container
  paths — and it still agrees with `tile.png` to within a tenth of a level on every channel mean.
  That is an assertion the size check cannot make: a decoder that swapped the chroma planes,
  upsampled them wrongly, or read the container's dimensions instead of the frame's produces
  something of exactly the right size and visibly the wrong colour.
  Reading the fixtures as one picture in three encodings would have been wrong, though, and the
  tests say so out loud. `tile.jpeg` is a *different photograph* — its red channel means 117.6
  against the other two's 63.9 — so it is excluded from that comparison with the numbers written
  down, rather than left for someone to "fix" the tolerance around later.
  The mutation harness takes the WebP seed too, and it is the one that earns its place: an
  extended container is a chunk walk over a length field the file states, which is the shape where
  a malformed body reads past the end if the walk trusts it. Nothing panicked. It is also now the
  slowest test in the workspace in a debug build — a thousand VP8 decodes is a great deal more
  work than a thousand PNG ones — which is the price of the coverage rather than an accident, and
  `--release` brings the file back under four seconds.
  Hosted styles then load at all, which they did not. A style written the way a vendor writes one
  contains **no HTTP anywhere**: `mapbox://styles/mapbox/streets-v11` is the whole address of the
  document, and every source, sprite and glyph range inside it is written the same way. With no
  rewriting the boot fetches nothing, and the failure is not a 404 — it is a scheme no transport
  claims, several layers from the style that wrote it.
  `util::mapbox` is the port, with `util::URL` and `util::Path` under it, and all of it is data
  rather than code: mbgl expresses three vendors as a [`TileServer`] of templates, domain names
  and version prefixes so a self-hosted server is *configured* instead of special-cased. This
  does the same, and even Mapbox's one oddity — `&secure` appended to a source URL — is a field
  rather than an `if`, which is the one place mbgl's own configuration leaks into its code.
  The API key is the other half and it belongs here rather than in a template a caller fills in.
  Its *name* differs by vendor — `access_token` for Mapbox, `key` for MapTiler — and it is
  appended to every request derived from the style. MapLibre's demo server needs none at all,
  which is why `requires_api_key` is a field and not an assumption: a normalize that demanded one
  would make the demo style unloadable.
  Only a *source* refuses outright for a missing key. mbgl throws there and merely logs for the
  other four kinds, and the asymmetry is right: a source with no key produces a TileJSON fetch
  that fails and takes every tile of that source with it, while a sprite with no key is one
  missing picture on a map that otherwise works. One error naming the parameter, before a socket
  is opened, beats a hundred 401s.
  Three details of the URL parse are transcribed rather than reasoned, and each is a defect if
  guessed. A `@2x` immediately before the dot is part of the **extension**, not the filename, so
  `streets-v8@2x.png` splits as `streets-v8` and `@2x.png` and the sprite template puts the scale
  back on the far side of the rewrite; reading it as part of the filename produces
  `streets-v8@2x/sprite.png`, a directory that does not exist. A `#` before the `?` means the
  fragment swallowed the query and there is none. And a query of span *one* is a bare `?` that
  carries nothing and is dropped — the check is `> 1` rather than non-empty, or every such URL
  ends in a stray ampersand.
  The query carried across from the original has its `?` turned into an `&` when the template
  already contributed one. Two question marks make every server read the second as a literal
  inside the first parameter's value, which for a signed URL means the signature silently stops
  matching — the failure mode that looks like an expired credential and is not.
  One deliberate divergence, and it is about the *answer* rather than the parse. mbgl logs a
  domain mismatch and returns the input unchanged, which hands the transport a `mapbox://` URL
  nothing claims. This reports instead: the information exists exactly there — which kind was
  asked for, which domain the URL carried, that they disagree — and passing the URL through
  throws all three away and replaces them with a fetch failure. It is safe because no well-formed
  style reaches it; `mapbox://////` is the input that does, and mbgl's own test pins it.
  **Where the rewriting sits in the stack is the load-bearing decision.** It goes *below* the
  in-flight coalescing table of §5.1 and the byte cache of §12.6, both of which key on the URL —
  so those layers see the canonical `mapbox://tiles/a.b/0/0/0.pbf` and only the transport ever
  sees the address with the credential in it. Two consequences follow and both are the point. A
  cached tile survives a key rotation, because an API key has a lifetime of its own — rotated,
  refreshed, scoped per user — and a cache keyed on the address it appears in would treat every
  rotation as a cold start for a whole downloaded region. And two views sharing a tile share it
  whatever key each was configured with, because they agree on the canonical form before either
  reaches a socket. mbgl solves the same problem from the other end, canonicalizing a normalized
  URL back before storing it; putting the rewrite at the bottom makes the storing side never need
  to know.
  That claim is asserted where it is observable rather than argued: one cache file, two stacks
  with different tokens, and the second request is a *hit* — with the negative beside it, that a
  different tile under the same token is still a miss, so the hit is not the cache answering
  everything.
  The resource kind is read off the URL's own domain segment, which is what lets a transport
  rewrite one at all: a file source is handed a URL and no context, and `mapbox://sprites/…` says
  what it is. What that gives up is the cross-check a caller who knows the kind can make, so
  `normalize` still takes one for callers that do.
  Clip masks land, and the first thing to say about them is that the earlier entry above had the
  layer wrong. It called a mask a *wire* question — `StencilTiles` carries a tile and a matrix
  and has no word for a quadrant, so a mask looked like a field addition against an ABI frozen at
  R0 exit, wanting a capture nobody could produce. Reading the two ends settled it the other way.
  **`renderTileClippingMasks` never sees a mask.** It builds one `ClipUBO` per render tile
  carrying a matrix and a stencil reference, draws a full-tile quad for each, and that is the
  whole of the stencil path — on every backend, and in the capture backend's `TileLayerGroup`
  too, which records exactly `{id, matrix}` per tile. `StencilTiles` is complete as it stands.
  **`TileMask` is consumed by two things, and both turn it into geometry.**
  `RasterBucket::setMask` and `HillshadeBucket::setMask`, each building a quad per entry at
  `EXTENT >> z`. So a mask is not a field the stream is missing. It is vertices, and vertices
  already travel — which means no ABI decision, no capture of a partially covered parent, and
  nothing frozen in the way. What was blocking the work was a misreading rather than a protocol.
  `algorithm::updateTileMasks` ports directly, and mbgl's own
  `test/algorithm/update_tile_masks.test.cpp` is the oracle: every case transcribed, including
  the two that a plausible implementation fails. A mask **descends** rather than stopping at
  quadrants — a single z4 tile under a z0 one masks it into twelve rectangles, three at each of
  four levels — and it is stated **relative** to the tile, `x - (root.x << depth)`, which at
  street-zoom indices is where an implementation that shifted the wrong operand produces a
  plausible mask nowhere near the tile.
  The empty mask and the whole-tile mask are opposites and are the pair a caller confuses. A tile
  covered by four children draws *nothing*; reading empty as "no restriction" renders that region
  twice, which on a translucent raster layer is visibly darker. Both are named in the API rather
  than left to a length check.
  Two structural consequences follow from the mask being geometry. It is **per view**, not per
  tile: §5.1 shares tiles across views, and two views loading at different rates hold different
  masks for the same tile, so the mask arrives as an argument to the builder rather than being
  baked into a cached bucket. And a masked bucket is therefore *different geometry* with a
  different id — which is not a loss of sharing but the same rule §5.3 already states, since two
  views that agree on the mask produce the same bytes and share as before.
  The whole-tile mask builds byte-identically to the unmasked bucket, which is what lets the two
  paths converge: a settled cover produces `{(0, 0, 0)}` for every tile, and anything else would
  re-upload geometry that had not changed on every settled frame. mbgl special-cases it to keep
  using shared full-extent buffers; here it is one quad either way, and the sharing is by
  geometry identity instead.
  A cold start passes the whole-tile mask explicitly rather than defaulting to it, because a
  cover *is* one zoom level and saying so is the point — the substitution case belongs to the
  sweep, and that is where the mask is computed over a renderable set.
  Importing an offline pack downloaded by another client is out of this tree, in a separate
  crate that is not part of this repository.
  Placing a model tile then needed one fact settled from the data rather than assumed, and it is
  the fact everything else follows from: **a buildings mesh is tile units in x and y and metres in
  z**. Measured across 972 nodes of a real store — node translations span 60 to 8189, which is the
  tile extent, while node z-scale is exactly 1.0 and heights run to 330 with a 95th percentile of
  136. Those are buildings in metres, not a normalised range. Half the nodes are flat, because a
  buildings tile carries a footprint mesh beside each extruded one.
  That is the same mixed convention `fill-extrusion` uses, which is why mbgl's own `heightFactor`
  — `-numTiles / tileSize_D / 8.0` — is the conversion rather than something derived here. It
  carries no latitude term and that is not an omission: heights are drawn in Mercator-scaled units
  so a building keeps its proportion against the locally-scaled ground, and putting latitude in
  would make one at sixty degrees correct against the metre and wrong against its own street.
  The matrix is the *drawable* matrix, not a second one computed beside it. A model tile sits in
  the same tile space as every other layer and takes the same layer and sublayer depth bias, so a
  parallel implementation would leave two copies of mbgl's bias arithmetic to keep in step — and
  they would agree right up until the day they did not.
  **The slot is the first number this build chooses rather than transcribes.** Every other slot
  comes out of the generated table, evaluated from mbgl's chain of anonymous enums under DR-6, and
  mbgl has no mesh layer to read one from. It is placed with a gap rather than adjacent to mbgl's
  range: mbgl's run zero to eight with `MAX_UBO_COUNT_PER_SHADER` at nine, and taking nine would
  collide the moment mbgl added a shader's worth of buffer. Sixteen leaves room to fifteen, and a
  compile-time assertion turns a future collision into a build failure rather than two things
  writing one slot. A search over the whole generated chain checks the number is unused by name as
  well as by bound.
  The placement travels as a `UboUpdate` — a consolidated buffer, one entry per mesh in the
  layer's draw order, exactly as a fill's does. Nothing new on the wire for it, which is the point
  of having put the mesh in the geometry id space.
- **R4** — hardening: ring backpressure under stall ✅, teardown protocol under fault ✅,
  process-isolation spike (§3.5) ✅, riscv64 soak.
  Two things elsewhere in this document are assigned to this phase and were not on this line,
  which is how a phase comes to look nearly finished while work is still pointed at it. §12.8's
  **pacing counters** ✅. And §13.2's **acknowledged-renderable** ✅.
  The item §3.5's spike pointed at §11.3 — the slab region packed after the frame that names it —
  is closed there: `SlabArena::in_region` allocates out of the shared region, and the isolation
  test now resolves every handle from the other process rather than sequencing around it.

---

## 11. Seam performance: Fluorite ↔ frontend

Four distinct costs — camera latency, tick-thread CPU, upload bandwidth, allocation churn —
each with its own mechanism. Ordered by payoff.

### 11.1 Camera ownership inversion (DR-9)

Rev 1's Phase-A model ships the fused `projMatrix` and requires the mirror camera to
contribute nothing (identity custom projection per the `fluorite_get_filament_view`
contract), putting every pan on the full round trip: input → producer transform →
CameraUpdate → ring → tick → Filament. The frame_diff.hpp FrameOrder comment already names
the alternative — carry the factors separately so a consumer can put the world on a real
camera. Rev 2 takes it to the conclusion:

- **Consumer-camera mode** (interactive views): the Fluorite ECS camera is authoritative.
  Producer emits tile-local transforms in the shared world space + `pixelsPerMeter`; Filament
  projects. Pan-to-photon latency equals Fluorite's own render latency; the ring drops out of
  the interactive path. The producer still needs the camera (cover, placement, screen-space
  UBOs) and reads a one-frame-stale copy off the reverse channel (§11.4) — cover has padding,
  placement is throttled, and screen-space widths lagging one frame is imperceptible.
- **Producer-camera mode** (non-interactive views: cluster insets, fixed tracks): the
  CameraUpdate path of §6.3 unchanged.

Mode is per view, declared at ViewDeclare (DR-18). This is an ABI decision, not an
optimization pass, so
it lands before R0 (see DR-9) — retrofitting it moves the world-space convention under the
consumer.

### 11.2 Tick budget and object collapse

The tick runs inside the ECS update on the Filament API thread; every microsecond is stolen
from the frame.

- **Time-budgeted drain with priority.** Camera/order/UBO envelopes first (cheap,
  latency-relevant), then geometry up to a per-tick budget (N buffer creations / M bytes).
  Tile churn bursts at zoom crossings; amortizing creation across 2–3 ticks is invisible, a
  12 ms tick is not. Spillover ordered by view visibility class (§11.4).
- **Renderable collapse.** One Filament renderable per mbgl drawable puts thousands of
  entities in the scene. `SegmentDesc` maps onto Filament multi-primitive renderables: merge
  drawables sharing (layer, shader permutation, texture set) into one renderable with
  per-primitive index ranges. Painter order survives — layers are contiguous in the draw
  order, within-layer tile order is stencil-resolved. Scene goes from thousands of
  renderables to ~tens.
- **Consolidated SSBO is the only uniform path** (DR-16). Rev 2 drops the per-drawable-buffer
  variant: latest-wins coalescing + one buffer update per (view, layer) per tick, drawables
  index via `uboIndex`, no length ceiling; per-drawable parameter-setting at map scale is not
  left available as a path. SSBOs need Vulkan or GLES 3.1+, which makes the support statement
  capability-based rather than device-based: maps require an SSBO-capable backend. No fallback
  path exists and none is half-built — the mode bit is reserved and the batch-splitting
  allowance documented but dormant, so a future GLES-3.0-only SKU is an addition rather than a
  flag day. There is no GLES map-drawing CI lane, because there is nothing to keep green.

### 11.3 Zero-copy bucket → driver

What the slab-handle design (§2.1) is for: Filament `BufferDescriptor` /
`PixelBufferDescriptor` take a release callback, so the mirror wraps the slab directly —
`setBufferAt` over the shared memory, callback drops the refcount when the driver's copy
completes. Geometry is touched exactly once after layout: by the driver's upload. Textures:
the §6.4 rect list maps one-to-one onto sub-region `setImage` over the shared atlas backing.
Obligations on the Rust side: slabs immutable once emitted (already guaranteed — drawables
are immutable after build; the AddReason premise), and slab lifetime extends to the Filament
release callback, which is exported C ABI back into the Rust half.

**A second reason to want this, from §3.5's spike — done.** A consumer that does not share the
arena reached geometry through a region `SlabArena::pack` built *after* the frame that named it,
so it could hold a `GeometryAdd` whose handle the region did not yet cover. In process there is
no window, because the arena is the same object on both sides.

`SlabArena::in_region` writes into a mapping the caller owns, and there is no pack step left to
be late. The ordering then comes free from the ring rather than needing anything of its own: a
frame's records become visible with one releasing store of `head`, every byte written before it
included, so a consumer that acquires `head` and then reads the region sees the slabs of every
record it can see. The isolation test asserts it from the other process — every attribute of
every record it can see resolves, and the same counter read forty unresolved before this.

Two things the region costs, both recorded rather than hidden. The table is *reserved* at
construction, because a handle indexes it and the bytes have to start at a fixed offset, so the
slot count bounds how many slabs can be live at once — not how many over time, since a sweep
recycles a slot. And the byte allocator is a bump cursor, so the space a swept slab leaves is
recovered only once everything above it has gone: a region that fills reports `RegionFull` and
the caller's recourse is DR-21's, displacing what its poorly-packed slabs still hold so the
survivors are re-announced into fresh slots. Compacting behind the caller would move a slab a
consumer holds a handle to, which is the one thing the region promises not to do.

The `Mapping` the arena takes lives in `tessella-capture-abi` beside `ring::init`, which does
the same job for the ring. `tessella-orchestrate` is `deny(unsafe_code)` and has never needed an
allowance; stating the obligation once, at the constructor that can break it, is what keeps that
true.

### 11.4 Reverse channel (DR-10)

The SPSC ring is one-way; add a small consumer→producer strip of explicit-width atomics:
last-consumed epoch, current camera per consumer-camera view, viewport + visibility per view
slot. Three uses:

- **Pacing**: producer throttles to actual consumption instead of free-running — coalescing
  hides overproduction from the consumer, but not from the CPU budget on RK3566-class parts.
- **Visibility gating at the source**: a view whose slot reports hidden gets cover
  maintenance only — no placement, no emission.
- **Symmetric parked path**: producer parked ⇒ tick is one atomic load and return.

### 11.5 Allocation churn

Tile bursts are the allocator stress. Rust side: arena-per-tile with slab reuse pools;
steady-state at zero malloc (frame-economy discipline, as in the drm-cxx migration). Mirror
side: pool Filament entities/renderables and vertex/index BufferObjects at the high-water
mark rather than create/destroy per tile — creation is not free and the churn pattern is
predictable.

### 11.6 Seam-wide measurement

Unified Perfetto tracks across both halves (slots into FLUORITE_PERF_PLAN tracing): ring
occupancy, drain time per tick vs budget, burst amortization depth, and pan-to-photon —
producer input timestamp carried through to the tick that applies it. §9.3 counters prove
traffic is proportional to change; these prove the change is cheap to apply.

### 11.7 Consumer obligations (both mirrors)

The obligations §11.2–§11.3 state for the Fluorite mirror are consumer-neutral and bind any
mirror, restated once: time-budgeted drain with priority classes; geometry batching (merge by
(layer, shader permutation, texture set), subject to R-9); one GPU buffer/texture per shared
geometry/atlas regardless of view count; honor the opaque/translucent split — opaque layers
front-to-back with depth writes per `pass` + `opaquePassCutoff`, or TBDR parts eat
full-screen overdraw per layer; sub-range buffer updates from UBO dirty ranges; sub-region
texture uploads from rect lists; hold CameraUpdate until its orderEpoch is held; release slab
references only after the driver's copy completes. Per consumer: Filament — renderables in
multiple Scenes, MaterialInstance per (view, layer) over the shared SSBO, release via
BufferDescriptor callback; impeller-rs — MapContents at entity/HAL level per §3.6, canvas
reserved for composition, in-process slab elision.

---

## 12. Producer hot paths

Ranked by expected payoff on the hardware matrix (RK3566 as the gate, RK3588/SA8155P as the
easy pass).

### 12.1 Expression evaluation (DR-11)

Bucket build evaluates expressions per feature; mbgl walks a boxed AST. Largest pure-CPU line
item after tessellation, and the one place a rewrite beats mbgl outright:

Measured on this port, against a real zoom-14 Protomaps tile with every paint property
data-driven: 1.13 ms to build against 0.72 ms with the same properties constant, over a 0.48 ms
decode. §12.1's premise holds, at about a third of the build rather than the three quarters the
first measurement gave — that one used `real-world-0-0-0.mvt`, a zoom-0 view of the whole world
whose 17 202 features are 17 153 of them in one dense `admin` layer. Both tiles are valid; only
one is shaped like a tile anyone looks at, and the difference is a factor of two in what the
numbers recommend. Every conclusion below the first was reached against the world tile; the wins
are real, the weights were not. But "data-driven" is not "evaluation": the gap was 8.5 ms against 2.2 ms
until the binder stopped allocating a scratch vector per feature and two more per slot inside
`encode`, which was a quarter of the surcharge and no evaluation at all. It ran only when a
property was data-driven, which is what made it easy to read as evaluation cost.

Where that cost sits is worth knowing before building the VM. `Feature::property` called
directly — the same dyn-dispatched call `["get", k]` makes, same scan, same owned `Value` — is
2 ns per feature. The data access is not the cost. A literal number evaluates in 3 ns and a
literal string in 7 ns, the difference being the `String` clone `Expr::Literal` does every time.
`["get", "admin_level"]` was 26 ns, of which two were the lookup and the rest the walk. Reading
a string without copying it took that to 12 ns: `["get", k]` holds its key as a literal, and
`expect_string(&evaluate(key))` allocated twice per feature — once cloning the `Value`, once
copying the text out of the clone — to read something known at parse. Borrowing the literal
straight from the tree is 2.5x on `get` and `has` and about a third off `match`. End to end it
is inside the noise on the style above, whose cost is dominated by nested `interpolate` rather
than by key reads; on a style whose data-driven properties are mostly plain `get` and `match` it
is the larger part. What remains at 12 ns is the walk
itself: recursive non-inlined `evaluate` calls returning a 40-byte
`Result<Value, EvaluationError>` by memory to carry what is nearly always an 8-byte `f64`, plus
the wrapping and the drops on the way back. The VM's target is the walk, not the data access.
**Against mbgl, on the same bytes: 0.49x by instruction count — half the work, having started
1.40x behind.** `crates/tessella-source/benches/decode.rs` does what
`Parse_VectorTile` does — the same tile, the same accounting — and run alternately with
maplibre-native's own benchmark runner the ratio held at 1.40 across minima, medians, means and
the median of paired ratios, at a coefficient of variation under two per cent. mbgl decodes
lazily and this port eagerly, but that benchmark touches every feature's geometries and
properties, so both do a full decode.

The forty per cent was the geometry. A feature averages 6.6 rings of 7.2 points on that tile, and
`Vec<Vec<[i32; 2]>>` asks the allocator for one vector per ring — 3937 of them, of 58 bytes, to
decode 593 features. One buffer with the ring ends beside it is two allocations a feature however
many rings it has, and it took the ratio from 1.40 to 1.00. Writing the points straight into that
buffer rather than accumulating each ring separately and copying it in — a tile is tens of
thousands of coordinates, and they were each written twice — took it to 0.97. Decode allocations
went from 17.1 a feature to 9.1 across the two changes.

Level was not ahead, and callgrind said where the rest was — deterministically, which on a
machine at load 14 is worth more than a stopwatch. Varint decoding was 38.8 % of the
instructions a decode executes: `varint` walked a ten-iteration loop with a bounds check and an
`Option` per byte, and nearly every varint in a tile is *one byte*, geometry deltas being
zigzagged small numbers and tags being table indices. A single-byte fast path in the packed
reader cut total instructions 21.5 %, halved varint work, and took the ratio to 0.70.

Then the geometry buffers were reserving per *ring* rather than per feature, so a feature of
eighteen rings reallocated eighteen times climbing to its size. The command stream bounds the
point count on its own — a point costs at least two varints of at least one byte — so one
reservation up front replaces all of them: 187 µs against mbgl's 307, ratio 0.61, and the tile's
decode allocates 567 KiB where it allocated 1146. `memcpy` left the profile altogether, which is
the reallocation copying that instruction counts undercount and a stopwatch does not.

Then the buffers moved off the feature and onto the *layer*. A feature holds ranges — into the
layer's points, its ring ends, its properties — and reads them through a `FeatureRef` that pairs
it with the layer, which is also where the `Feature` trait impl now lives. Decoding writes
straight into those buffers rather than building a per-feature `Geometry` and copying it in;
doing the latter would have paid the allocation and the `memcpy` this arrangement exists to
remove. One reservation per layer, from the bytes its features occupy, rather than per feature:
growing a shared buffer per feature copies everything already in it, which is exactly what
reappeared as `memcpy` the moment the buffers became per-layer. Decode allocates 4.7 times a
feature, from 17.1 when this started.

Confirmed on a quiet machine, which took most of a day to get: 14 alternating rounds pinned to
one core read 151 µs against mbgl's 306 by minima, a ratio of 0.494 — against callgrind's 0.491.
The two methods agree to within half a per cent, which is what says neither is measuring the
machine. Under load 30 the same wall-clock comparison read 0.43, flattering this side by about a
tenth.

The comparison below is by instruction count, not by stopwatch. mbgl's benchmark body compiled as
a standalone program over the same fixture, both under callgrind: 110 308 089 instructions
against 224 763 669, and both print the same total so they are provably doing the same work. That
matters because a stopwatch on this machine flatters the result — mbgl proved about 1.8x more
sensitive to load than this decoder, so the wall-clock ratio drifts from 0.61 to 0.43 as the
machine fills up while the instruction ratio does not move at all.

What is left, by instruction share: the inlined decode body at 53 %, the packed reader at 27 %,
`memcpy` at 6 %, and the allocator no longer in the profile. The allocator share is the three vectors a
feature still owns — its properties, its points and its ring ends — which one buffer per *layer*
with features holding ranges into it would take to nothing amortised.

**SIMD: deferred, with the analysis kept so it need not be redone.** The simd-json approach does
not port — it finds structural characters in parallel and protobuf has none, a field's position
depending on decoding the one before it. What does port is Masked VByte over the *packed* runs,
which is exactly what MVT geometry commands and tags are, and which published results put at
2–3x on that portion. The packed reader is 27 % of instructions, so that is worth perhaps a tenth
overall.

Not taken, for now. It buys one tenth against three code paths — x86, NEON and scalar — because
`std::simd` is nightly and DR-17 pins the toolchain, and riscv64 vector support is not somewhere
to be relying on (§16). The decoder is already at half of mbgl's instruction count, which is the
bar this was chasing, and the same effort spent on symbols or the startup path buys more than a
tenth of a decode. Revisit if a profile on the RK3566 lane says decode is the thing missing a
budget — the argument above is what to pick up, and the standalone callgrind comparison in
`crates/tessella-source/benches/decode.rs` is how to tell whether it worked. 

`benches/expression_cost.rs` holds the rest of the measurement, against the zoom-10 tile
`benchmark/parse/vector_tile.benchmark.cpp` decodes in mbgl's own `Parse_VectorTile` — so the
two sides can be compared on the same bytes rather than argued about. Every absolute figure in
this section was taken on a machine that also measured the same decode at 455 µs and 812 µs an
hour apart under somebody else's build; the with-and-without ratios were alternated across
rounds and held, the absolutes wandered by a factor of two. Read the ratios.

Averaging more runs does not fix that, and is worth being precise about because it is the
obvious thing to reach for. Interference is one-sided — another process can take time from a run
and never give any back — so the distribution has a floor at the true cost and a tail above it.
The mean is biased upward by exactly the contamination it is supposed to average out, and more
samples converge on the biased figure rather than the true one: under load, mbgl's own harness
reported a mean of 414 µs where its minimum was 345 and the quiet-machine number is 302. The
minimum is the estimator of the floor; alternating the two things being compared is what makes
the *ratio* trustworthy while neither absolute is.
It counts allocations as well as timing:
a build with data-driven paint does 99 231 of them and one with constant paint 75 045, so the
data-driven surcharge was about 24 000 — roughly one per feature, half of it colours. A colour
had no runtime type: `Type::Color` existed statically, but the value was a `Value::Array` of
four numbers, so every evaluation allocated a `Vec` for sixteen bytes of channel and a colour
was indistinguishable from a plain array of the same numbers. Giving it a variant removed 12 116
of those allocations, took `["rgb", …]` from 38 ns to 27, and left the golden dumps byte for
byte identical. The 75 042 underneath are tessellation and bucket building, four and a half per
feature before any expression is involved.

Decode was invisible to that measurement, which decodes once outside
the timed section: on the world tile, 3.9 ms and 282 186 allocations — 16.4 per feature, against
the whole build's 5.1. Three of those per feature were the property keys. MVT keeps a layer's
keys in one table and has features refer to them by index precisely so a key is stored once, and
decode was cloning a `String` out of it per tag per feature. Sharing them took decode to 3.4 ms
and 230 677 allocations, lower in every alternating round.

The rest of the gap was growth, not structure. A feature's own vectors — its properties, its
geometry, and the ring inside it — are 3.3 allocations per feature, against the 13.4 measured, so
ten were the packed-varint scratch buffers rebuilt per feature and the reallocation of everything
grown by pushing. Reusing the scratch across a layer's features and pre-sizing from counts the
format states before the data — a feature's tag count, a ring's point count — took decode to
2.6 ms and 160 317 allocations. Against where it started, a third off the time and 43 % of the
allocations, without changing what is decoded.

The next structural step is the geometry, still a `Vec` of `Vec`s at roughly 2.5 allocations per
feature: one buffer per tile with ring offsets would take that to nothing amortised, which is the
same shape as the tessellation output and the layout buffers below it.

**Lazy decode is not worth it, measured.** mbgl decodes lazily and memoised — `getLayer(name)`
for the layers a style names, `getValue(key)` for one property rather than a map, and a filter
that runs before `getGeometries()` so a rejected feature never decodes its geometry. This port
decodes eagerly, which was never a decision so much as what a straightforward decoder looks like.
On three real Berlin tiles at z12, z14 and z15, a real style reads 100 %, 99 % and 80 % of the
points and 91 %, 83 % and 25 % of the features — so skipping unnamed layers saves between nothing
and a fifth of decode, and the layers it would skip are `places` and `pois`, which is precisely
what R2's symbols will need. Decode costs about 0.21 µs a feature plus 0.016 µs a point, so
geometry is between a fifth and two thirds of it depending on the tile; filter-before-geometry
has a real ceiling there, but no style available here carries a filter to measure the rejection
rate against. Revisit when symbols land and filters are in play.

- **Strict classification at compile time.** Constant → folded at style parse. Camera-only →
  evaluated once per (layer, integer-zoom interval), process-wide, cached as interpolation
  endpoints; per-view/per-frame cost is one mix factor at that view's fractional zoom (mbgl
  re-walks per frame per map). Data-driven → per feature at build, never per frame.
- **Bytecode VM for the data-driven residue.** Flat bytecode, no virtual dispatch, no
  per-eval allocation. JIT (cranelift-class) rejected for embedded code size; bytecode gets
  ~80% of it. **Tried, and it lost.** A flat evaluator with an operand stack of `Value` —
  compiling `get`/`has`/`match`/`case`/`coalesce`/comparison/arithmetic, leaving zoom curves to
  the walk so their shape stays readable — measured *slower* than the tree at every size: `get`
  46 ns against the walk's 9, `match` 60 against 20, the build 7.9 ms against 5.6.

  The cost was the operand frame. `Value` is 32 bytes and has a destructor, so a fixed frame is
  initialised and dropped on every evaluation: `get` measured 46, 22, 17 and 14 ns for frames of
  32, 8, 4 and 2 slots — about 1.3 ns a slot — which puts a *free* frame at roughly 10 ns,
  still no better than the walk. The walk is not slow because it recurses; it is slow because of
  what it moves, and a VM moves the same things through a stack instead of through returns.

  So the prerequisite is not the VM. It is a runtime value that is `Copy`, has no destructor and
  fits in a register pair — a compact representation with strings and objects interned or boxed
  behind an index. With that, a frame costs nothing to set up and stack traffic is register
  moves; without it, flattening the walk buys nothing. That ordering is the correction: DR-11
  schedules the VM and does not mention the value representation, and the representation is the
  part that decides whether the VM can win at all.
- **Columnar evaluation.** One expression batched across a tile's feature array rather than
  expressions interleaved per feature — cache-resident program, SIMD-ready arithmetic.

### 12.2 Decode and layout granularity

Parallel unit is (tile, layer-family): fill/line tessellation of a tile parallelizes across
layers while symbol shaping of the same tile proceeds independently — per-tile is too coarse
(one symbol-heavy tile blocks a burst), per-feature too fine. MVT decode is zero-copy: varint
cursor over the fetch buffer, geometry decoded straight into the slab arena, no intermediate
feature materialization for layers that don't read properties. Raster (PNG/WebP) decode on
the same pool with a SIMD decoder (zune-image class), directly into the texture slab.

### 12.3 Shaping and glyph caches

Shaped-run cache keyed (fontstack, text, layout params), LRU sized in glyphs — label text is
massively repetitive across tiles, zooms, and views (a road name recurs in every tile it
crosses), and the same keying feeds the cross-tile index. One level down: glyph-SDF
rasterization cache for the local-glyph path. Both process-wide (§5.5).

### 12.4 Memory formats

i16 tile-local positions; u16 indices with u32 spill per segment; R8 single-channel glyph/SDF
atlases (not RGBA — 4× on the largest persistent texture). Vertex-format audit rides the
golden oracle: the C++ formats are the floor, not the target — several f32 streams survive as
snorm16. f64 stays in transform/tile-placement math only; everything per-vertex across the
ABI is f32/i16 tile-local, which is also what keeps consumer-camera mode (DR-9) precision-safe
at high zoom: tile-local + camera-relative composition, never world-absolute f32.

### 12.5 Startup / first-tile-to-photon

Cold start today serializes style → manifests → tiles → decode → placement. Instead:
speculative parallel fetch (sprite + glyph ranges + cover tiles issued the moment sources
parse, before layer compilation finishes); binary compiled-style cache keyed by style etag so
warm start skips parse + expression compilation; first-frame fast path — fills/lines render
the moment buckets land, symbols fade in on the first real placement pass rather than gating
the frame. Cold-boot-to-map is an IVI spec number: dedicated trace metric beside
pan-to-photon (§11.6), exit criterion on R1.

### 12.6 Cache and network path

SQLite WAL + mmap read path; cache hits decode straight from the mapped page, no intermediate
copy. HTTP/2 multiplexing matters more than usual: request coalescing (§5.1) concentrates
traffic onto one or two origins, so connection reuse + TLS session resumption is the
difference between burst latency and burst stall on flaky automotive links. Etag
revalidation per TileJSON expiry; zstd where the origin offers it.

### 12.7 Incremental cover/retain

Cover + retain recompute gates on crossing a tile boundary or an integer-zoom threshold (with
the velocity-scaled margin from R-8); between crossings cover is provably unchanged. This is
what keeps the single-orchestrator multi-view tick cheap at input rate.

**Amended by measurement: the gate is on the *result*, not on predicting the crossing.** `cover()`
is 0.10 µs for a nine-tile z14 viewport, so four views at sixty frames a second spend twenty-four
microseconds per second computing it — a predictor to avoid that would cost more than it saves
and add a way to be wrong about what is on screen. So the cover is recomputed every frame and its
*change* gates everything downstream: retain and release against the shared store, rebuilt
bindings, and the damage that follows. `viewcover::ViewCover` is that, with `entered()`/`left()`.
This is also the concrete difference from mbgl, which re-derives the downstream work every frame
whether or not the cover moved (DR-22).

### 12.8 Power and pacing

Wakeup pattern matters as much as throughput on DVFS-governed parts. One deadline wheel for
all timers (§5.5); produce at the consumption rate the reverse channel reports, not at loop
speed; parked extends to the scheduler — a parked view holds no timers except cache expiry.
Sustained-idle-then-burst beats constant medium load.

**Pacing counters landed in R4** (`tessella-orchestrate::pacing`). Nothing in the producer
drives frames — §3.2 puts the tick on the consumer's side — so what is here is the answer to
*should this tick produce a frame*, and counters of what pattern of answers came out. The caller
keeps the loop.

The consumption rate needed no new field anywhere. It is the ring's `tail`, which the consumer
publishes and `Producer::consumed_through` reads: emit when there is something to send and the
consumer has drained what it was already sent. A consumer that stalled forever would otherwise
stall the map forever, so it is bounded — a change held past the latency budget goes anyway and
the ring's own backpressure takes over, which at least fails loudly where a held change is a map
that is quietly wrong. The bound is what separates pacing from blocking: a slow consumer makes
the map update less often, not stop.

The counters are of *wakeups* rather than bytes, which is the half §9.3 does not cover. §10's
parked identity says nothing left the producer; a parked view that sends nothing and still
builds a frame to discover it has spent the power anyway. And the burst shape is made checkable
rather than aspirational: what a governor punishes is a producer busy a little of every tick, so
the question is whether the emissions clumped, and a run of one is a dribble however many of
them there are. Both cases are tested at sixty ticks — the same sixty frames of work, once as
two runs of thirty and once every other tick.

Tested against a real ring as well as as a state machine, because a policy reading the wrong
number is still a policy: sixty ticks of a moving camera against a consumer that drains every
sixteenth, with the pacer and without. Unpaced fills the ring and is refused twenty times;
paced is refused none and never holds more than the frame it just sent.

**Not built**: the deadline wheel, and with it "a parked view holds no timers except cache
expiry" — there are no timers in this tree to hold. The counter for that is owed when the
scheduler is.

### 12.10 Beating mbgl, and how that gets decided (DR-22)

The goal is not parity. This has to be measurably faster than maplibre-gl-native and visibly
better on the same hardware, and where the architecture stands in the way the architecture goes.
Parity with mbgl is the *oracle's* job — what the stream says — and it is not the bar for what
the frontend costs to produce it.

What that rules out is optimising by assertion. Every claim below is a number or it is not a
claim, and the order of work follows from that.

**Step one is to make it work with the fewest changes that can be made.** Not the fastest
arrangement — the one that draws a correct map soonest, so there is something to measure. An
architecture chosen before the profile exists is a guess, and this document has three examples of
guesses that measured wrong: §12.7's boundary predictor (cover is 0.10 µs; the predictor would
cost more than it saves), the horizon cull (§13.4: four to six of the cheapest tiles on the map),
and the "AttributesModified storm" the damage model was built to prevent, which turned out to be
one line of gating rather than a mechanism.

**Step two is instrumentation, and it is the deliverable rather than the preamble.** §9.3's
counters and §11.6's Perfetto tracks exist to say where time goes; what they have not yet been
asked is where it goes *compared to mbgl on the same frame*. The probe already runs mbgl headless
over a style and a camera, which is most of a two-sided measurement: the same sweep through both,
per-frame, with the counters aligned.

**Step three is re-architecting where the profile says to, with the before and after both
recorded.** A change that cannot show its improvement did not make one.

#### What mbgl actually does, read rather than assumed

`TilePyramid::update`, per source, per frame. Its only early-out is `!needsRendering` — the
source has no visible layer at all. If the source is drawn then every frame, unconditionally:
`tileCover` recomputes the ideal set; `updateRenderables` walks it, allocating a fresh
`std::unordered_set<OverscaledTileID>` per call, creating missing tiles and falling back to
children and then parents; a `retain` set is rebuilt and `setNecessity` stamped on every tile;
anything unretained goes to the cache or is abandoned.

Three things follow, and they are the shape of the opportunity rather than a criticism:

- **It re-derives per frame what changed per crossing.** A still map pays for a cover, a
  renderables walk, a hash-set allocation and a retain set on every frame and every source.
  §12.7's arrangement — recompute the cheap thing, gate the expensive things on the cover
  actually changing — is where the difference is, and `viewcover::ViewCover` already implements
  it with `entered()`/`left()` deltas.
- **Necessity is per tile and per frame.** `Required`/`Optional` gates whether a request goes
  out, which is the right idea expressed as a per-frame stamp over every tile.
- **Prefetch is a lower-zoom cover, requested whole.** `panTiles` at `zoom - prefetchZoomDelta`,
  as a second full cover. It puts something on screen sooner and costs the bandwidth of a second
  set of tiles.

#### What is ours to build, and what is already built and unwired

The parts are further along than the composition. `renderables::update_renderables` is
transcribed and passes all eighteen of mbgl's own `update_renderables.test.cpp` cases;
`viewcover::ViewCover` holds the per-view cover with the zoom latch and reports the deltas. Both
are used only by tests: `map::Map::tick` calls `cover::cover` directly and has no parent/child
fallback at all, so a pan into new ground would show *holes* where mbgl shows blurry ancestors.
That is worse than mbgl, it is a wiring gap rather than a design one, and it is step one.

**Prefetch is owed, and as onion layers rather than as a second cover.** mbgl requests one extra
level whole. What a map wants is progressive refinement: the coarse level standing in for the
fine one while it loads, each layer replaced as it arrives, with the request order following what
is visible rather than what is enumerable. That is a different data structure from
`panTiles` — a per-tile chain of ancestors already held, which `update_renderables` half provides
by finding them — and it is where "visibly better" is most likely to be won, because the metric a
user sees is time-to-first-legible-frame rather than frames per second.

#### The metrics that decide it

Both sides, same style, same camera, same machine, reported per frame rather than as an average:

- **Time to first legible frame** — a cold start to a frame with the viewport fully covered at
  any resolution, which is what the onion layers are for.
- **Idle cost** — CPU on a settled map. mbgl's floor is a cover, a renderables walk and a retain
  set per source per frame; ours should be a comparison. This is the one where an order of
  magnitude is plausible.
- **Cost of a crossing** — an integer zoom crossing and a tile-boundary crossing, worst frame
  rather than mean, since §13.1's invariant is about the tail.
- **Bytes on the wire per frame** — §9.3's traffic-proportional-to-change claim, against mbgl's
  per-frame re-derivation.
- **Frames to settle** — how many frames a sweep takes to stop churning.

A delta smaller than the repeat-to-repeat spread is not a result, and the harness has to say so:
`maplibre_fluorite/test/sweep_bench.sh` already refuses to report a number without saying what
else the machine was doing, and the two-sided version inherits that.

#### First measured result: idle cost, 79× to 215×

Both sides asked for a frame with the camera unmoved, after the map has settled. mbgl through
`mbgl-capture-probe --bench-idle=N`, which calls `Renderer::render` regardless of the probe's own
dirty flag — the gate being measured is mbgl's, not the harness's. tessella through
`benches/idle_frame`, which calls `Map::tick` on a settled map.

| | mbgl | tessella | ratio |
|---|---|---|---|
| p50 | 11.08 µs | 0.14 µs | 79× |
| p95 | 11.49 µs | 0.15 µs | 77× |
| p99 | 14.62 µs | 0.15 µs | 97× |
| **max** | **47.88 µs** | **0.27 µs** | **177×** |

Two thousand settled frames each, same style, same camera, same machine. The gap is widest at the
*maximum*, which is the number a frame budget is a promise about (§13.1).

**And mbgl's floor moves with the style while ours does not.** Measured across three: 11.2 µs for
the symbol style, 28.7 µs for the hermetic one, 30.4 µs for the composite — because the work is
per source and per layer, and it happens whether or not anything moved. tessella's idle path is a
camera-key comparison and a dirty-flag test, which is O(1) in the style by construction: the gate
returns before the cover, the cache, the arena or the ring are touched. So the ratio is 79× on
the style that flatters mbgl most and 215× on the one that flatters it least.

**What this is not.** The scopes are not identical, and pretending otherwise would make the
number worthless. `Renderer::render` also evaluates paint properties and assembles draw calls,
which `tick` leaves to the consumer. What both include is the part being compared: deciding what
the frame contains. So the honest claim is narrow — this is the cost of *deciding there is
nothing to do* — and it says nothing about steady-state throughput, where the same tiles decode
into the same buckets either way and §12.1 is where the difference would come from.

It is still the number that matters most for a cluster: a map that is settled is the common case
by a wide margin, and four views paying 30 µs each per frame for nothing is 7.2 ms per second of
CPU spent establishing that nothing happened.

#### Second result: a zoom crossing, and why its ratio is not yet a clean claim

The sweep is the honest half. Idle cost measures who *skips* better; a z8→z16→z8 sweep measures
who does the unavoidable work faster, because the cover really has changed and mbgl's per-frame
re-derivation is work that had to happen this frame.

Both sides settled at every step, which is what makes it a comparison: the probe runs its loop
until the drawable set stops moving before timing a frame, and the tessella harness has every
tile available throughout.

| | mbgl | tessella | ratio |
|---|---|---|---|
| p50 | 45.53 µs | 4.20 µs | 10.8× |
| p95 | 52.72 µs | 6.21 µs | 8.5× |
| p99 | 54.77 µs | 8.16 µs | 6.7× |
| max | 394.05 µs | 8.29 µs | 47.5× |

**The caveat, before the numbers are quoted anywhere — and it was wrong the first time it was
written here.** The claim recorded was that mbgl's sweep includes producing tiles, since its
geojson source calls `geoJSONVT::getTile` as the cover changes, making the ratio an upper bound.
Checked afterwards, that is not what happens: `GeoJSONVTData::getTile` takes a `runSynchronously`
flag, `isUpdateSynchronous()` is false unless a style asks for it, neither the probe's inline
style nor `symbol_style.json` does, and the asynchronous path goes to
`Scheduler::GetSequenced()` — a background pool. Tiling is not inside the bracket being timed.

So the scopes differ, but not that way. mbgl's `render()` carries paint-property evaluation, draw
assembly and the capture backend's own diffing, none of which `tick` does; `tick` carries
encoding geometry into slabs, which is the analogue of the second. Neither carries tiling or
layout, which are on a worker on both sides.

**The second bias was predicted backwards, which is worth recording.** The expectation was that a
sweep driven by `jumpTo` would time mbgl mid-load — drawing less than a settled map and reporting
*less* than a real frame costs. Settling before each timed frame was added to remove that, and it
moved the numbers the other way: p50 from 51.85 to 45.53 µs and p95 from 110.97 to 52.72 µs. A
mid-load frame is *dearer*, not cheaper, because it is the frame where drawables are being added
and removed. So the earlier figures were inflated by exactly the thing predicted to deflate them,
and the settled ones above are what a fair comparison shows.

What remains uncontrolled is the scope difference already stated — paint properties and draw
assembly on one side, slab encoding on the other — and it is not resolvable by harness changes,
because it is a real difference in what each half does. Both sides now exclude tiling, both are
settled, and both are asked the same question: what does a frame cost when the cover has moved.

**What does survive the caveat is the tail, because it is internal to each side.**

| | worst frame ÷ median |
|---|---|
| mbgl | 8.7× |
| tessella | 2.0× |

That is a statement about predictability rather than about speed, and §13.1 is a promise about
predictability: mbgl's worst crossing frame costs nearly nine times its median, and tessella's costs
twice. Neither figure depends on what the other side was doing. At sixty frames a second one
mbgl view spends 2.4% of a frame budget on its worst crossing and four spend 9.5%; the same four
here spend 0.2%.

#### Third result: never blank through the loop, and 7.8× reuse

`sweep_never_blank` asserts completeness over `ViewCover`; this asserts it over `Map::tick`, which
is what a consumer drives, against a fixture where a tile becomes drawable three frames after
something first asks for it. Forty-eight frames of continuous zoom: **no frame drew nothing**, and
the map settled the frame after the camera stopped.

Across that sweep it **drew 447 drawables and sent 57 geometries — 7.8× reuse.** That is §9.3's
traffic-proportional-to-change stated as a ratio, and it is the metric a seam's bandwidth
follows: a tile that stays in view keeps its geometry id and is drawn again for nothing.

**The first version of this test was wrong in a way worth recording**, because the mistake is
available to anyone reading the same counters. It called a frame blank when `Emitted::geometries`
was zero, and reported forty-two blank frames out of forty-eight. `geometries` counts what had to
be *sent*; `drawables` counts what was *drawn*, announced this frame or not. Forty-two frames of
correct incremental emission read as forty-two holes. The field's own documentation says the two
compared are "the whole measure of an incremental emission" — which is exactly right, and is also
why one of them alone measures nothing.

#### Fourth result: the onion, 5× to a legible frame

Time to first *legible* frame — the viewport covered at any resolution — on a cold start with a
fetch latency of three frames and two fetches in flight.

| | legible at | tiles requested |
|---|---|---|
| no prefetch | frame 15 | 9 |
| onion, four levels | **frame 3** | 19 |

Frame three is the fetch latency, so the onion reaches the floor: the map is legible as soon as
*anything* can arrive. Nine tiles two at a time is four and a half rounds without it.

**It is not mbgl's prefetch.** mbgl covers a second time at `zoom - prefetchZoomDelta` and
requests that whole cover. This asks for the ancestors of the tiles it is *missing*, which is a
subset and usually a tiny one — siblings share a parent, so nine ideal tiles collapse to one or
two per level, and four levels up very often to the single tile that covers the viewport. Asking
for what is missing also means asking for nothing when nothing is: a settled map prefetches
nothing at all, where a second cover must be computed and diffed to discover the same.

**The order is the mechanism, not the depth.** Every real fetcher has a bounded queue, and the
order decides what occupies it. Ideal-first spends the first slots on detail tiles covering a
ninth of the screen each; coarsest-first spends the first slot on the tile that covers all of it.
Same requests, same bytes, different moment of legibility.

**The cost is 2.1× the requests on a cold start**, and that is the trade rather than a free win:
with nothing cached, every ancestor is a real fetch.

**On the path a user actually takes it is 1.07×.** Measured rather than assumed, because the
claim was written here before it was checked: a steady zoom from z2 to z10 fetches 69 tiles
without the onion and 74 with it — five extra tiles across eight levels. The coarse levels a
deep cover would want are the ones already being drawn from on the way down to it, and the filter
against the source means they are not asked for twice.

So the worst case is a cold start at depth, which is a deep link or a restored session, and the
common case is nearly free.

**The first attempt measured it as 12 frames *worse*, and the metric was at fault.** With the
onion on, legibility was reported at frame 27 against 15 without. The coverage counter was
over-reporting holes: `update_renderables` short-circuits when a second ideal tile shares an
ancestry a sibling already walked, which is correct and loses the answer — the sibling's walk may
have *rendered* an ancestor, and that ancestor covers this tile too. The walk reported such tiles
as holes while they were plainly drawn. Recording which ancestors were rendered, and consulting
that at the short-circuit, is the fix; all eighteen of mbgl's own cases still pass.

Worth stating because the wrong number pointed the wrong way. Had it been believed, the onion
would have been reverted as a regression on the exact metric it improves fivefold.

#### What instrumenting mbgl's side found about the metric itself

Counting holes on mbgl — the same count, in the same place in the same algorithm — reported that
its map *never becomes legible*: no rendered frame with zero uncovered ideal tiles, on a style
that plainly draws. Traced per frame, the count starts at twelve on the first drawing frame,
falls to five, and stays at five forever.

**Five is correct, and the definition was wrong.** A hole is an ideal tile with nothing drawn
over it, and that conflates two states: *not loaded yet*, and *loaded and legitimately empty*. A
sparse source has many of the second — this style is three points near London, and most of a z13
cover is ocean — so the count settles at a floor above zero that is a property of the style, not
of the map's progress.

So legibility is **the frame the count stops falling**, not the frame it reaches zero. Measured
on this style: floor of five, reached at frame 27.

**Checked on this side, and the floor is zero.** The suspicion was that the tessella figure was
flattered by a fixture where every tile has content. Run against a sparse source — one tile in
three with data, the rest loading empty — it reports a floor of **zero, reached at frame two**.

The difference is in what a source is permitted to say. `Tiles::buckets` returning `Some` of an
empty list is *loaded, and empty*, which is distinct from `None` meaning *not loaded*; the
substitution pass treats the first as covering its ground, because it does — there is nothing
there and the map is complete over it. mbgl has no such distinction to draw: a tile with no
features never reports renderable, so it is indistinguishable from one still in flight, for ever.

| | hole floor | reached |
|---|---|---|
| mbgl | 5 | never zero |
| tessella | 0 | frame 2 |

This is worth more than a row in a table. A permanent floor above zero means no consumer can ask
"is the map finished" and get an answer, and no measurement of when it became legible can be
stated absolutely — both must be phrased against a floor rediscovered per style. With a floor of
zero, "complete" is a question with an answer.

**A cold-start frame count is not comparable between the two, and the reason is structural.**
mbgl's `Map` exists before its style does: it is constructed, then handed a URL or a document to
load asynchronously, so a cold run spends its first frames rendering with no layers at all.
Measured, the style finishes loading at frame 1, 3, 3, 9 and 27 across five runs of the same
document — it dominates a from-process-start figure and is not stable enough to subtract by
assumption. A `Map` here takes a parsed `Style` by value and cannot exist without one, so its
harness starts where mbgl's arrives.

What *is* comparable is the tile work: frames from style-loaded to the hole floor.

| | frames from style-loaded to floor |
|---|---|
| mbgl | 2–4 |
| tessella | 2–3 |

Which is to say: **close.** The onion's 15→3 is tessella measured against itself, and it is a
real result about request ordering under a bounded queue; it is not a claim about mbgl, whose
prefetch does the same job by a different route. Quoting a from-start figure — 27 against 3 —
would have been quoting mbgl's style loader as though it were its tile pipeline.

The difference that does hold on this metric is the floor itself, above: mbgl arrives quickly at
five holes and stays there, and this arrives at zero.

This is the third measurement this session whose first form was wrong, and the second where the
error would have been invisible without the other side to check it against. Instrumenting the
oracle is worth it for that alone, separately from any number it yields.

**And it says the idle result is not merely a gating trick.** If the two were close once the work
became unavoidable, the 79×–215× would have been a story about when work is skipped rather than
about what it costs. They are not close. That reading survives the correction above, because the
correction removed a reason mbgl's number might be inflated and added a reason it might be
deflated — and the conclusion did not depend on which.

### 12.9 Binary size (DR-12)

50–60k LOC of generic-heavy Rust monomorphizes. Posture set early: `panic=abort`, fat LTO,
`opt-level=s` on non-hot crates, `dyn` boundary at the style-parse layer (parse is not hot;
stops the largest serde/expression monomorphization fan-out). Size tracked per target in CI.

Debug info is the one place size loses. Release builds keep line tables and are not stripped
in-tree: under `panic=abort` a field crash otherwise yields an address and nothing else, and
the packaging layer already splits symbols into a `-dbg` package, so stripping at the profile
would trade field diagnosability for a number CI measures after the split anyway.

---

## 13. Zoom performance: two regimes, four views

Requirement: variable zoom is flawless across four simultaneous map instances. "Flawless" is
made mechanical by the §13.3 benchmark; four is a number to budget against, not an abstract N.

### 13.1 Fractional zoom (between integer levels) — must cost ~nothing

- Consumer-camera mode (DR-9): fractional zoom on an interactive view is pure camera motion —
  zero geometry traffic; Filament re-projects.
- Producer traffic is interpolation state only: per-layer `_t` mix factors and screen-space
  sizing UBOs — a handful of (view, layer) consolidated-SSBO writes per frame, hundreds of
  bytes. The packed min/max vertex design (endpoints per tile level, per-frame cost one
  scalar mix) is the enabling invariant.
- Camera-only expressions: shared endpoints per (layer, zoom interval) (§12.1); per-view
  per-frame work is one mix factor.
- **Policed invariant (CI):** zero `AttributesModified`, zero geometry envelopes, during any
  zoom that does not cross an integer level. Asserted in `parked_is_silent.rs`: sixty frames of
  13.0 → 13.9, every one at a new zoom, every one owing camera bytes and none of them geometry,
  with the ring head unmoved. The fact it rests on is asserted separately — a cover is the same
  set of tiles across a whole integer level and changes at the boundary — because that is a
  property of the cover and not of the damage tracker, whose `geometry` flag means "something
  landed" rather than "the camera crossed a level".

### 13.2 Integer crossings — where flawless is earned

A crossing is a burst: new cover, fetch/decode/layout, placement redo, consumer buffer
creation — and four views can cross simultaneously (a synchronized four-view zoom transition
is the realistic worst case, not a contrived one).

- **Predictive pre-warm.** Zoom velocity off the reverse channel; approaching a boundary,
  fetch + decode + layout the next level before the crossing, so the crossing is a handoff of
  built buckets, not a build. Warm window: one level in the direction of travel; both
  neighbors briefly on gesture reversal. Converts the burst from crossing-synchronous to
  background-priority work.
- **Hysteresis** (~0.1–0.2 z) on cover recomputation at the boundary; pinch oscillation
  around an integer zoom must not rebuild cover at gesture rate. `cover::ZoomLatch` holds it, at
  0.1 by default. Separate from `ViewTransform::tile_zoom` on purpose: that is a pure function of
  a camera and the cover, the oracle parity and the tile keys all depend on it staying one, while
  hysteresis needs memory of the level currently held. The band is measured against that held
  level rather than against distance travelled, so a fly-to across nine levels still lands where
  it was aimed. Both it and the never-blank substitution are now held by
  `orchestrate::viewcover::ViewCover`, which is the answer to where per-view cover state lives
  (§5.2): one object per view, walked by §5.4's single pass. It answers §12.7 differently than
  the section words it — predicting boundary crossings to skip the computation is not worth
  doing, since `cover()` is 0.10 µs for nine tiles and four views at sixty frames spend
  twenty-four microseconds a *second* on it. What is expensive is retain, release, bindings and
  damage, so the cover is recomputed every frame and the *change* gates the rest. Measured, a
  pan across one whole z14 tile changes it twice in two hundred frames — once per vertical edge
  — and sixty frames of pinch either side of an integer zoom change it not at all. The delta is
  reported rather than the set, so a tile another view holds is not released and re-retained
  through zero, which would be an eviction and a rebuild for a tile that never stopped being
  needed.
- **Never-blank, acknowledged.** Ancestors retained until every covering descendant's buckets
  are consumer-**acknowledged** via the reverse-channel epoch — mbgl retains until *built*,
  and the build→GPU-upload gap is exactly where its single-frame holes come from. Per-tile
  handoff as descendants land; stencil resolves overlap.
  The substitution itself lands as `tessella_tile::renderables`, a transcription of mbgl's
  `algorithm::updateRenderables`: an ideal tile that is not ready falls back to its children if
  *all four* are ready — three children and a hole is a hole — otherwise to the nearest ready
  ancestor, which is almost always what was on screen a moment ago. The map goes momentarily
  blurry rather than momentarily empty. Necessity is carried separately from retention because
  it decides what may be *fetched*: an ideal tile is required, a substitute optional, since a
  request for a stopgap competes with the tile that would make it unnecessary. The property it
  exists for is asserted separately from the port — a faithful transcription of a wrong algorithm
  passes an oracle diff and fails this: across a crossing in both directions, under every arrival
  order a coprime stride reaches, no ideal tile is left with a hole. Coverage is decided on the
  quadtree rather than by sampling, since a hairline of background between two tiles is exactly
  the artefact at issue and a sampling test passes for a hole thinner than its spacing. And it
  counts only tiles that *have data*, which mutation testing forced: dropping the renderable
  check on a substitution left every coverage assertion passing, because filling a hole with an
  empty tile covers it as far as tile ids are concerned. That in turn needed a pyramid that
  models a pending tile, mid-crossing being mostly pending. Checked against
  all eighteen of mbgl's own expectations, whole action logs rather than final state — what the
  algorithm declines to ask for (the ancestry a sibling already walked, the request it does not
  spend on a substitute) is as much of the contract as what it draws.
  **The acknowledged part landed in R4.** The producer wrote the records and knows where each one
  ended, the consumer publishes how far it has uploaded through, and the comparison is the whole
  of it: `GeometryRegistry::is_acknowledged` takes the furthest position a tile's drawables were
  announced at and asks whether the reverse channel has passed it. No new field on either side —
  the acked position has been there since DR-10. Re-announcing moves it forward, which is right:
  a displaced drawable's bytes are in a different slab and have to be uploaded again.
  The algorithm did not change, as this said it would not. What did is `TileState::loaded`, and
  that was not foreseen here. mbgl reads `loaded` as *done waiting on this tile*: an ancestor is
  worth a `Required` request precisely when the tile below has finished and still cannot be
  drawn. Under `renderable = built` the two moments coincide and the distinction never shows.
  Under `renderable = acknowledged` a tile that is built but not yet uploaded has finished
  loading and is still about to become drawable without anyone fetching anything — so calling it
  loaded makes the ascent spend a request on an ancestor no view covers, once per upload gap, at
  every crossing. A caller that defines `renderable` as acknowledged must define `loaded` as
  acknowledged-or-failed. The §13.3 sweep found it: modelling a two-frame upload gap turned the
  "only ideal tiles are fetched" assertion red before the completeness one, which is a better
  order to find it in than on a board.
- **Bounded, prioritized burst.** Decode/layout center-out within visible cover, foreground
  view class first; the tick geometry budget (§11.2) amortizes buffer creation across 2–3
  frames while ancestors still cover. Symbols cross-fade through placement; fades count as
  churn until settled (§6.5), then silence.
- **Retain-chain unification across views** (§5.5): views at adjacent zooms over one area are
  one pyramid — the z12 view's active tiles are the z13 view's retained ancestors, so one
  view's never-blank retention is another's free coverage insurance.

### 13.3 Acceptance benchmark (R1.5 exit)

Four-view synchronized zoom sweep, z8→z16→z8 continuous, on RK3566:

- frame budget held on every tick (§11.2 budget counters);
- coverage completeness: a walker over every frame of the sweep proves the viewport fully
  tile-covered — zero uncovered frames;
- zero symbol pops (fade-only transitions);
- bounded ring occupancy through simultaneous crossings;
- §9.3 flatness: fetches, decodes, bucket builds, shaped labels, atlas uploads, material
  compilations flat in view count for overlapping covers.

---

## 14. Decision records

- **DR-1 Ring-only transport.** FrameSink trait dropped from production; callback model
  survives only in the oracle probe. Driven by the Fluorite tick pull model (§3.2).
- **DR-2 Single DSO, Rust staticlib + C++ mirror half.** Driven by fluorite_ffi.h Filament
  re-export rule and hidden-visibility seam (§3.1).
- **DR-3 Teardown order** stop-signal → Filament destroy → join (§3.3).
- **DR-4 ABI rev 2**: ownership explicit (slab handles, copy-on-emit), geometry/view
  namespace split, FrameOrder → CameraUpdate + OrderUpdate with orderEpoch, texture rect
  lists, contentHash retired. Rev 1 semantics preserved per §2.2.
- **DR-5 Shared stores are R0 architecture**, not a multi-view feature (§5).
- **DR-6 Generated shader data.** Attribute tables and UBO layouts generated from
  `shaders/*.hpp` with layout asserts; never hand-maintained.
- **DR-7 No async runtime.** Threads + channels, mbgl actor style; dedicated worker pool with
  priority classes (§5.4).
- **DR-8 Zero-traffic-when-parked is a protocol guarantee** with CI counters (§6.5, §9.3).
- **DR-9 Camera ownership inversion.** Interactive views run consumer-camera mode: the
  Fluorite ECS camera is authoritative, the producer emits tile-local transforms in shared
  world space, and reads the camera back over the reverse channel. Producer-camera mode
  remains for non-interactive views. Per-view, declared at ViewDeclare (DR-18). Lands before
  R0 — it
  fixes the world-space convention the consumer projects (§11.1).
- **DR-10 Reverse channel.** Consumer→producer atomics strip in `tessella-capture-abi`:
  last-consumed epoch, per-view camera, per-view viewport/visibility. Producer pacing,
  source-side visibility gating, symmetric parked path (§11.4).
- **DR-11 Expression classification + bytecode VM.** Constant folded at parse; camera-only
  evaluated once per (layer, zoom interval) process-wide; data-driven compiled to flat
  bytecode, evaluated columnar per tile. JIT rejected for embedded code size (§12.1).
  *Amended:* classification, folding and the direct evaluator are done; the VM was built and
  measured slower than the walk it replaced, because `Value` has a destructor and an operand
  frame therefore is not free. A compact `Copy` runtime value comes first — §12.1 has the
  numbers.
- **DR-12 Build posture.** panic=abort, fat LTO, opt-level=s on non-hot crates, dyn boundary
  at style parse; binary size tracked per target in CI (§12.9).
- **DR-13 Consumer-neutral ABI, proved by two mirrors.** The stream must contain nothing
  accidentally Filament-shaped; the impeller-rs mirror (§3.6) is the conformance instrument,
  and consumer-specific needs are met in §11.7 obligations, never in envelope shape.
- **DR-14 impeller-rs integration at entity/HAL level.** Canvas-level consumption is
  rejected (per-frame vertex rewrites violate the §13.1 damage invariant); mbgl shader
  families port into impeller-shaders as AOT pipelines; text divides at the
  coverage/packing seam (§3.6).
- **DR-15 Name: tessella.** A tessella is the small tile of a mosaic — tiles without the
  picture, which is the architecture. Independent of the MapLibre mark: the repo does not
  lead with "maplibre" or the `mln` namespace (maplibre-native's own C++ namespace);
  compatibility is claimed in the README as "a Rust frontend for the MapLibre style spec,
  emitting a renderer-agnostic capture stream." crates.io prefix `tessella-*`; bare
  `tessella` reserved with a stub publish.
- **DR-16 Uniform transport: SSBO-only, Vulkan-first (resolves R-12).** One path:
  consolidated buffer per (view, layer), `uboIndex` indexing, no length ceiling. Support
  statement is capability-based: maps require an SSBO-capable backend — Vulkan today, GLES
  3.1+ if a consumer ever implements one (impeller-rs's GLES HAL floors at 3.0 and
  composites only). Mode bit reserved, batch-splitting allowance documented-but-dormant;
  no fallback path exists, no GLES map-drawing CI lane. Consequences: the impeller-rs
  mirror exercises the Vulkan HAL only and lands beside the R0 stub; VisionFive 2 is
  producer/soak/cross-compile only, with a rendering path arriving only if the Mesa pvr
  Vulkan driver matures — at zero cost and zero breakage to this design either way.

- **DR-17 Toolchain pinned to the target Yocto release.** `rust-toolchain.toml` pins the
  compiler to the Rust oe-core ships — 1.94.1 for wrynose (Yocto 6.0) — and `rust-version`
  follows it. The pin tracks the distro, not upstream Rust: building against a compiler the
  board does not have moves MSRV surprises from CI onto the target, and it is the target that
  is expensive to debug. Bumps happen when the target Yocto release bumps. CI carries an
  advisory `stable` lane as early warning for that day; it does not gate a merge. Dependency
  floors are subordinate — fontdue's `integer_sign_cast` (1.87) and edition 2024 (1.85) both
  sit below the pin, and if a dependency ever demands more than the distro offers, the
  dependency is what changes.

- **DR-18 View declaration is its own envelope.** DR-9 originally declared camera mode at
  `ViewUse`, but `ViewUse` is per (view, geometry) while the mode is per view: the mode would
  be repeated on every use, every copy would have to agree, and a consumer seeing disagreement
  would have no principled response — it cannot know which copy is current, and treating a
  later one as a mode change would swap the world-space convention mid-frame. `ViewDeclare`
  and `ViewUndeclare` carry per-view state once, ordered ahead of any `ViewUse` naming the
  view. The pair also gives per-view configuration a home before the ABI freezes: the §5.4
  per-view `maxzoom` clamp and view class ride in reserved bytes rather than needing an
  envelope added after R0 exit.

- **DR-19 GeoJSON polygon vertex order is wagyu's, and wagyu is not ported.** mbgl passes every
  GeoJSON polygon through `fixupPolygons` before it reaches a bucket — unconditionally, citing
  geojson-vt-cpp issue 44 — which takes a wagyu union of the rings. Wagyu rebuilds each ring from
  its own sweep and chooses its own starting vertex, so the oracle's ring is a *rotation* of the
  one geojson-vt's clip produces. The clip itself, the axis order, the significance filter and
  the twenty-six-clip tiling pyramid were each tested and cleared; the pyramid simulation is a
  test in `tessella-source::clip`. Porting wagyu would buy a vertex order and not a different
  polygon: on well-formed input its union is geometrically an identity — same rings, winding,
  area, and triangulation up to a permutation. mbgl runs it because GeoJSON may be
  self-intersecting or wrongly wound. Consequence for §9.1: for GeoJSON polygon sources the
  vertex-buffer diff compares rings as cycles rather than sequences, which still catches a wrong
  coordinate, a missing vertex or a reversed winding. Revisit if a style appears whose geometry
  makes the union non-trivial — self-intersecting rings are where it would show, because there
  wagyu genuinely changes the polygon and a cycle comparison stops being enough. Vector tiles are
  mostly unaffected — mbgl runs `fixupPolygons` on them only for spec version 1, which is
  effectively extinct — so R1's diff against a real style can compare vertex sequences directly,
  and a v1 tile is the one case where it would have to fall back to cycles.
  Confirmed from the other side by the line layer: `fixupPolygons` takes polygons only, so a
  LineString reaches the bucket in source order, and the line path's vertex *and* index buffers
  match the oracle's own FNV hashes byte for byte across all six tiles of the hermetic style.
  That is the whole chain — projection, clip, rounding, join selection, extrusion, bit-packing —
  compared as sequences, and it is what says the rotation is wagyu's alone and not something
  upstream of it that the fill path's cycle comparison was hiding.

- **DR-20 Sprites and raster decode PNG, JPEG and WebP; compressed textures are a separate question.**
  KTX2 with a Basis or block-compressed payload is genuinely cheaper than RGBA8 where it counts
  — a 1024-square sprite sheet is 4 MB decoded and roughly 1 MB as ETC2 or ASTC, and on an
  RK3566 that is shared memory and shared bandwidth. It is the same argument §12.4 already makes
  for R8 glyph atlases, and §12.4's "the C++ formats are the floor, not the target" invites it.
  It still cannot replace PNG here, for three reasons that are not about the codec.
  **The format is not ours to choose.** A style-spec sprite is `sprite.json` plus `sprite.png`,
  and every style in the wild — Protomaps, MapTiler, OpenMapTiles — serves exactly that. Raster
  tiles are the same: the origin decides, and it decides *JPEG* for satellite imagery, because a
  photograph stored losslessly is several times the bytes for a difference nobody looking at a
  map can see. So both are read and the format is sniffed from the bytes. A build that reads
  only KTX2 loads no existing style; one that reads only PNG draws no satellite basemap.
  WebP is the one mbgl reads and this does not yet, and it is a real gap rather than a
  hypothetical: MapTiler and Mapbox both serve `.webp` variants. It is refused by name so the
  answer is legible, and `image-webp` is the pure-Rust decoder to add behind the same feature.
  **It would cost the oracle.** mbgl decodes PNG, and the capture's texture hash is over decoded
  pixels. Reading different bytes than the probe reads leaves nothing to diff, which is the one
  thing that makes any of this checkable.
  **The wire has no word for it.** `TexturePixelType` is generated from `mln::TexturePixelType`
  under DR-6 — RGBA, Alpha, Stencil, Depth, Luminance — so a compressed upload means either
  diverging from a generated table or adding a value mbgl does not have, against an ABI frozen
  at R0 exit.
  Decode *cost* is not the reason either way. A sprite sheet is decoded once per style, against
  a cold start measured at about 3 ms in total; it is not on a hot path. Raster tiles are the
  case where continuous decode would matter, and there the format is the origin's anyway.
  Where compression does pay is later and elsewhere, in two places. **The offline cache**: a
  region's resources are already downloaded and pinned, so transcoding a sheet once at download
  time costs nothing per session and saves the residency every session after — the origin still
  serves PNG and only our cache changes. **The consumer**: Filament is what uploads to the GPU,
  and compressing at the upload needs no producer change at all. Both still need a compressed
  pixel type on the wire to be visible across the seam, so either way the decision is an ABI one
  rather than a decoder one, and it wants a measurement first: raster tile decode on RK3566,
  against the frame budget §13.3 already has a harness for.
- **DR-21 Geometry retention is generational slabs: a buffer holds many geometries, and a
  geometry is a sub-range of one.** §5.3 says "one Filament VertexBuffer/IndexBuffer per shared
  geometry", and the batching work needs the opposite — one draw call reads one vertex buffer,
  so a layer's tiles must share a buffer to collapse 588 draws into 12. Both cannot hold, and
  this is which one gives.
  A slab is the buffer and a geometry is a sub-range of it. Geometry appends to the layer's
  currently open slab; slabs are refcounted, which `Arc<Slab>` already is; when a slab's live
  fraction falls below a threshold its survivors are re-emitted into the current one and it is
  freed. So a layer's live geometry sits in one or two slabs at a time, and a draw is one or two
  multi-draws rather than one.
  **The three that were weighed.** *Re-emit a layer whole when its cover changes* keeps batching
  perfect and frees promptly, and is the simplest thing a consumer can be asked — "replace this
  buffer". It re-uploads the layer's entire cover to change one tile: measured, 20.8 MB to
  replace roughly half a megabyte, and cover changes are constant under nav. *Per-tile slabs*
  honour §5.3 exactly and free promptly, and cost the batching win — a draw would have to bind
  several vertex buffers, which Vulkan permits and Unity's `BatchRendererGroup` and UE5's
  `FPrimitiveSceneProxy` do not express. It is also the most buffer churn of the three, which
  §11.5 already names as one of the four seam costs. *Generational slabs* is the third and is
  what this record chooses.
  **The consumers decided it.** Every engine in view penalises many short-lived buffers and
  rewards few long-lived ones: Unity's BRG is built on exactly that shape, UE5's RHI wants to own
  its buffers and fights churn, Filament's are driver objects. All of them can update a
  sub-range — `BufferObject::setBuffer`, `GraphicsBuffer.SetData`, `RenderingDevice::buffer_update`,
  RHI lock-with-range — so a large buffer partly rewritten is a shape every one of them takes.
  And the obligation this puts on a mirror is the ABI as already designed: hold buffers, bind
  sub-ranges, free on `GeometryRemove`. Nothing new is asked, which matters most for UE5, the
  hardest of the five.
  mbgl reached the same shape for images without saying so: a process-wide `DynamicTextureAtlas`
  with refcounted slots and repacking, which the pattern capture showed directly — every tile
  binds the same texture and only the position map is per tile.
  **What it costs.** §5.3's per-geometry buffer language becomes per-slab, with a geometry as a
  sub-range. That is a correction of the same kind as `GeometryId`'s: the refcount-and-release
  model §5.3 describes survives intact, and only the granularity of the buffer changes.
  **When to revisit.** If a consumer appears that cannot bind a sub-range, per-tile slabs become
  the only option and batching has to be paid for another way. If cover changes turn out to be
  rare in practice — a mostly-static view rather than nav — the whole-layer re-emit is simpler
  and its bandwidth argument evaporates. And if compaction's threshold proves hard to tune, that
  is evidence for the whole-layer re-emit rather than against retention.
- **DR-22 The bar is measurably faster than mbgl and visibly better, and the order of work is
  make-it-work, instrument, re-architect.** Parity with mbgl is what the *stream* is held to, and
  it was never the bar for what the frontend costs to produce it. Where the architecture stands
  in the way of beating it, the architecture goes — but not before a profile says which part.
  **Why the order is that way round.** Three architectural guesses in this document measured
  wrong: §12.7's boundary predictor (cover is 0.10 µs; the predictor costs more than it saves),
  §13.4's horizon cull (four to six of the cheapest tiles on the map), and the pattern-atlas
  design that a capture settled in an afternoon. The cost of building the simple thing first and
  measuring it is one iteration; the cost of choosing an architecture from a guess is finding out
  after it is load-bearing.
  **What is being compared.** Both sides, same style, same camera, same machine, per frame rather
  than averaged: time to first legible frame, idle cost on a settled map, worst-frame cost of an
  integer-zoom and a tile-boundary crossing, bytes on the wire per frame, and frames to settle.
  The probe already runs mbgl headless over a style and a camera, which is most of a two-sided
  harness. A delta smaller than the repeat-to-repeat spread is not a result and the harness has to
  say so, as `sweep_bench.sh` already does.
  **Where the difference is expected, and where it is not.** Idle cost is the one where an order
  of magnitude is plausible, because mbgl re-derives per frame what changes per crossing — a
  cover, a renderables walk, a hash-set allocation and a retain set, per source, whether or not
  anything moved. Steady-state throughput is not: the same tiles decode into the same buckets
  either way, and §12.1's expression work is where that is won or lost. Claiming the first as if
  it were the second is how a benchmark stops meaning anything.
  **Prefetch is owed and is not mbgl's.** mbgl requests one extra level whole (`panTiles` at
  `zoom - prefetchZoomDelta`). What a map wants is progressive refinement — coarse standing in for
  fine while it loads, each layer replaced as it arrives, requested in the order it becomes
  visible. That is the onion, it is a different data structure from a second cover, and it is
  where "visibly better" is most likely to be won: the metric a user perceives is time to a
  legible frame, not frames per second.

## 15. Risk register

- **R-1 Symbol pipeline underestimation.** No ecosystem substitute; placement parity is
  visually judged as well as diffed. Mitigation: R2 isolated, oracle diff on layout half
  (shaping/quads are deterministic), render tests via the mirror for placement.
- **R-2 Screen-space-sized properties break naive sharing.** Line widths / circle billboards /
  symbol sizes evaluate against a view's zoom; two views disagree about one drawable. Geometry
  survives (sizes flow through UBOs, not vertices); mitigation is per-view UBO variants /
  per-view material instances over shared buffers. First symptom of getting it wrong: one
  display's roads at another display's width.
- **R-3 Expression semantics drift** (rounding, coercion, `match`/interpolate edge cases).
  Mitigation: oracle diff + the style-spec expression test corpus run against the evaluator.
- **R-4 Ring stall pathology.** Consumer pause (scene teardown, mode switch) while producer
  churns. Mitigation: coalescing table bounds occupancy for state envelopes; geometry
  backpressure blocks the producer by design; watchdog counter.
- **R-5 orderEpoch consistency bugs** manifest as one-frame flicker under churn. Mitigation:
  hold-camera-until-order rule in the consumer, epoch assert in debug builds.
- **R-6 Cross-target regressions** (riscv64 atomics/alignment in the ring ABI). Mitigation:
  ring ABI uses explicit-width atomics, layout asserts compiled on every target, R4 soak.
- **R-7 Teardown deadlock** if a join lands before Filament destroy or a fetch never wakes.
  Mitigation: DR-3 order, non-blocking stop signal, join timeout with abort-and-log.
- **R-8 Consumer-camera staleness artifacts.** Producer decisions (cover, placement,
  screen-space UBOs) lag the authoritative camera by ≥1 frame; symptoms are edge-of-screen
  tile pop under fast pan and momentarily mis-sized screen-space widths. Mitigation: cover
  padding scaled by camera velocity off the reverse channel; accept UBO lag (imperceptible at
  one frame); pan-to-photon and pop counters in §11.6 tracing.
- **R-9 Renderable collapse vs painter order.** Merging drawables into multi-primitive
  renderables assumes layer-contiguous draw order and stencil-resolved within-layer order;
  translucent layers with cross-tile sort keys (symbol fade, line sort-key) can violate the
  assumption. Mitigation: collapse only within (layer, pass) groups the order proves
  contiguous; symbols excluded from collapse in R2 until measured.
- **R-10 Pre-warm misprediction.** Velocity-based next-level warm-up wastes fetch/decode on
  gesture reversals and burns radio/power if too eager. Mitigation: warm window of one level,
  hysteresis band, lowest priority class, warmed-but-unused counter in tracing with a budget.
- **R-11 Cross-view retain coupling.** Unified retain chains mean one view's zoom behavior
  extends another view's tile lifetimes; a pathological view (rapid full-range zoom cycling)
  can inflate process memory for all. Mitigation: per-view retain budgets on top of the
  shared LRU; eviction pressure sheds cross-view insurance retention first.
- **R-12 UBO floor divergence — RESOLVED by DR-16.** SSBO-only; no fallback path exists.
  Residual risk is only that a future GLES-3.0-only product SKU appears, at which point the
  reserved mode bit and dormant splitting allowance make the fallback addable without a
  flag day.

### 13.4 Globe, and what of it reaches this side

The globe is drawn by bending Mercator geometry per vertex in the consumer's material, so the
producer emits the ordinary flat placement and knows nothing about it. That is true of
*placement*. Tile **selection** is a separate question, and `globe_cover` measures it rather than
arguing it: a flat cull can ask for tiles a sphere has curved out of sight, and each one is a
fetch, a decode, a bucket build and a subdivision spent behind the planet.

Two things come out, and the second is the one that matters.

**The horizon costs little.** Tiles behind the sphere are 33–50% of the cover between z1 and
z2.5 and *nothing* outside it: zero by z3, where a tile spans a few degrees and there is no
horizon left to cut, and zero across the whole of §13.3's z8–z16 sweep. In absolute terms it is
four to six of the cheapest tiles on the map. Not worth a spherical cull in the producer.

**World copies are wrong rather than wasteful.** A Mercator plane repeats horizontally, so a
low-zoom cover legitimately holds the same tile at several `wrap` values and the map draws each.
A sphere has no copies: every wrap of a tile bends to the *same* patch. At z0 four of five cover
tiles are copies and at z1 four of eight — so a globe view drawing a flat cover draws those
patches twice, z-fighting on the surface and paying subdivision twice at the zooms where
subdivision is dearest (a z1 tile splits to ninety segments an edge).

So the producer's part in the globe is one policy and not an algorithm: **a globe view asks for
one world copy**, which is a per-view parameter beside its cover and its camera rather than a
change to how covering works. That is `cover::WorldCopies`, a parameter of the *request* — the
surface the tiles are drawn on is not something the camera knows. It folds the cover onto the
near copy rather than filtering to it, which is the difference between the policy working and
leaving a hole: a view centred on the antimeridian sees patches whose only entry has a non-zero
wrap, and filtering would drop exactly those. The horizon is the consumer's to skip, one dot product per tile
before it subdivides, which removes the draw as well.

Four views change none of this. They want the same tiles at these zooms and the shared store
builds them once — so the waste is four to six tiles for the cluster, not per view.

`benches/globe_sweep` is the measurement, and it sweeps z0–z8 rather than §13.3's z8–z16 for a
reason worth stating: above z3 a globe and a plane ask for *identical* tiles, so the acceptance
sweep measures the globe by measuring nothing about it. Over the low sweep the policy removes 6%
of tile requests, which is the wrong number to quote on its own — the copies are not spread over
the sweep, they are concentrated in the frames below z2 where they are two thirds of the cover.
The worst frame is z0, where eight of twelve requests are copies.

The **count** is the result there and the sweep's clock is not, which is worth being explicit
about: the two policies differ by tens of microseconds over a sweep whose own repeat-to-repeat
spread is wider than that, so the timing columns say which frames are expensive and nothing
about the difference between the rows. The count is exact — the same cameras give the same
covers every run — and it is turned into a time separately, by measuring what a cover entry
costs against the nine vendored Protomaps tiles and multiplying. That median is 522 µs, so the
eighty removed requests are about 42 ms of producer work over the sweep, each of which was also
a subdivision and a draw the consumer no longer makes.

## 16. Open questions (rev 0.4 targets)

- ~~PMTiles in tessella-storage~~ closed: `tessella-storage/pmtiles` reads a v3 archive in
  place, byte-identical to `pmtiles serve` across zoom 0 to 15. It was cheap in Rust, as this
  said. MBTiles is still open, and is a different shape — SQLite rather than a directory format,
  so it lands on the `cache` feature's dependency rather than needing one of its own.
- **Where a cold start's wait goes: `tessella_create`'s blocking boot, and who fetches.** Two
  decisions that land together or not at all, because a non-blocking create without a fetch loop
  is a map that never loads anything past its first cover.
  The cost being placed: `cold_start` measures 22 ms for a nine-tile cover against a *local*
  extract, 6.7 ms of that to first geometry. Over a network it is hundreds of milliseconds, and
  it scales with the cover. Today `tessella_create` blocks the caller for all of it.
  Noticed by asking what mbgl gains from rendering twenty-seven frames of nothing while its style
  loads — nothing, but the comparison is not flattering either way: mbgl spins its render loop
  and stays responsive showing an empty map, and this blocks the caller instead. On a UI thread
  that is the worse of the two, and it is a defect here rather than a virtue.
  **What `create` does.** *(1)* Block through the full boot, as now: a returned handle is a map
  you can draw, and every failure surfaces at one call — against a stall that scales with cover
  size and latency, neither of which the caller controls. *(2)* Block through the style parse and
  the source manifests only, tiles arriving through ticks: bounded work, style and origin errors
  still fail where they are actionable, and the map goes coarse-then-sharp through the
  substitution path — against one network round trip still on the calling thread, and two-phase
  failure reporting. *(3)* Parse the style and nothing else: never stalls, every entry point
  uniformly cheap — against a bad style URL that cannot fail at create, so the consumer holds a
  handle, sees an empty map, and needs a status channel to learn why. That failure mode is
  "silently blank", which this document has caught three times already.
  **Who fetches.** *(a)* The FFI owns a loop over `wanted()` — self-contained, and it puts
  network and priority policy in the binding layer, which §5.5 places outside a view; the same
  mistake as putting orchestration there. *(b)* The consumer drives it, taking `wanted()` and
  handing tiles back — matches §5.5 exactly, and every consumer reimplements it while the C API
  widens. *(c)* A process-scoped tile source implementing `Tiles`, shared across views, fed
  `wanted()` — which is what `Coalescing` + `TileCache` + `Pool` already are, and what `boot`
  constructs and then discards. One fetch for two views over one tile, which is §9.3's flatness;
  the cost is a source handle distinct from a map handle in the C API.
  **Leaning: (2) with (c)**, which keeps each layer where this document puts it and gives the
  onion visible work — the first tick draws the coarse ancestor and later ticks sharpen. The
  argument against it is its own: (2) still blocks on a round trip, so a hard frame budget on the
  calling thread makes (3) plus an explicit error-status call the honest choice, and the extra API
  is what never stalling costs.
- Style-revision transition policy for live restyle across N views (atomic repoint vs
  per-view staggering).
- Whether OrderUpdate should delta (splice ops) rather than snapshot — snapshot chosen for
  0.1; delta only if churn-time bandwidth measures poorly.
- emb manifest entries for the workspace. The Rust pin itself is closed by DR-17
  (rust-toolchain.toml, tracking the target Yocto release); what remains is the emb-side
  manifest wiring and the cross C toolchains the deferred deps (rusqlite, ureq) will need.
- Hysteresis band width and pre-warm trigger threshold: fixed constants vs tuned per view
  class; needs the §13.3 rig before choosing.
- Compiled-style cache format (§12.5): bespoke vs rkyv-class zero-copy archive; invalidation
  keyed by style etag + plan ABI rev.
- ~~Little/big core affinity policy (§5.5): explicit pinning vs scheduler hints, per target~~
  closed: the part is asked rather than assumed, so it is one policy rather than one per target.
  `orchestrate::topology` reads the kernel's own capacity numbers and `Affinity` says what to
  make of them, defaulting to scheduler hints. See §5.4.
- ~~Second-consumer sequencing~~ closed by DR-16: the impeller-rs mirror (Vulkan HAL) lands
  beside the R0 stub.
- ~~UBO floor~~ closed by DR-16: SSBO-only, Vulkan-first.
- ~~Reserve `tessella` on crates.io and GitHub~~ closed: `tessella` 0.0.0 published as a
  dependency-free stub, `github.com/jwinarske/tessella` public, workspace scaffolded to §7
  with the nine `tessella-*` members held at `publish = false` until they carry content.
- Direct-scanout product shape: tessella-* + impeller-rs single-binary cluster map over a leased
  DRM connector (wayland-leased-drm/DLM alignment); scope as its own plan doc if pursued.
