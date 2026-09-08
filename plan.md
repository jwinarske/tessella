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

It reaches a consumer as `tessella_set_world_copies`, a toggle rather than a mode a map is
created in — MapLibre switches projection at runtime and so does this. Nothing invalidates: the
cover is recomputed every frame anyway, so the frame after the switch reports `Changed` by the
path a pan takes and everything downstream releases the copies it was drawing. The policy applies
to every cover a view asks for and not only its own zoom — the raster walk that addresses a zoom
of its own, and the per-tile background — since those are tiles on the same surface, and a globe
that folded its vector cover and not its raster one would draw the imagery twice at exactly the
levels the fold exists to fix.

What is not here is the consumer's half: the vertex bend, the subdivision, the horizon cull, and
symbol placement on a sphere, which this section does not cover and is the hard part of the
three.

It is also the part with no oracle. MapLibre Native has no globe projection -- the two "globe"
hits in its tree are a doc comment about wrapping horizontally and a Metal widevector shader --
so `mbgl-render` cannot be rendered against for any of it, and the parity metric that has decided
every other question here says nothing. The reference that does exist is MapLibre GL JS, a
different codebase in a different language, and porting to it is a different kind of work than
transcribing mbgl with its own expectations to check against. That does not make the globe wrong
to build; it makes it the one piece whose correctness has to be argued rather than measured, and
worth starting only when the Mercator quad it sits beside is finished.

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
- ~~**Where a cold start's wait goes: `tessella_create`'s blocking boot, and who fetches.**~~
  **Closed and built**, as decided below: `create` parses the style and stops, `tessella_status`
  reports what a blank map is waiting for, and `orchestrate::source::TileSource` is the
  process-scoped source that resolves once and builds per tile off `wanted()`. What follows is
  the reasoning that got there, kept because the recommendation lost to its own counter-case. Two
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
  **Recommended: (2) with (c).** It keeps each layer where this document puts it — policy
  process-scoped, orchestration in `map`, the binding thin — and it gives the onion visible work,
  since the first tick draws the coarse ancestor and later ticks sharpen. It also keeps the
  failure that matters most, a style that will not load, at the call that can report it.
  **Recommended against (1)** because the stall it imposes is unbounded in the caller's terms: it
  grows with the cover and with the network, and a consumer has no way to cap it.
  **Recommended against (a)** for the reason orchestration was moved out of the FFI earlier in
  this document — a binding layer that owns policy becomes a second place that policy lives.
  **The case against the recommendation**, which is real: (2) still blocks on one round trip. If
  the calling thread has a hard frame budget — a UI thread, or a compositor tick — then (3) with
  an explicit error-status call is the honest choice, and the extra API surface is simply what
  never stalling costs. The deciding question is not which is tidier but whether any blocking
  call is acceptable on the thread that will make it, and that is a property of the consumer
  rather than of this design. **Fluorite's answer decides it**, and it is now asked.
  **Closed: (3) with (c), and an explicit error-status call.** Fluorite's answer is that no
  blocking call is acceptable. Three things say so, and they agree. Its Dart bindings are
  `@Native`, which binds the symbol directly with no isolate hop, and `attachMap` is a plain
  synchronous method rather than a `Future` — so the call runs on the calling isolate, and in
  Dart `async` would not have changed that, since awaiting does not move work off an isolate.
  There is no `Isolate.run`, no `compute`, nowhere in `lib/`. And the existing product already
  refuses this on its own account: `attachMapToFluorite` reads material packages from local disk,
  registers a tick callback and returns, doing no network at all, while mbgl's style load happens
  later against a render loop that keeps spinning. `prepare()` says as much in its own comment —
  it runs on "whichever thread called attachMapToFluorite", so it deliberately does nothing that
  belongs elsewhere. Blocking the UI isolate on a network round trip freezes the whole
  application rather than the map, which is a worse failure than the one (3) is charged with.
  So the extra API surface is paid for: `create` parses the style and nothing else, and the
  "silently blank" hazard is answered by the status call rather than by a stall. What (2) was
  protecting — a style error surfacing where it is actionable — is kept by making that status
  call the thing a consumer must read before it draws, which §11.7 can require of a consumer in
  a way it cannot require of a thread.
- **Whether decode should run on Fluorite's job system rather than tessella's own pool.** Asked
  because the consumer already owns a work-stealing pool and this process would then have one
  scheduler rather than two. Two answers, because the work splits in two.
  **Not for fetching, and it is the same mistake as (a) above.** Filament sizes its pool
  `hardware_concurrency - 1`, halved first where hyper-threading is present and capped at 32 —
  HT excluded deliberately, "to simplify profiling". Those threads are frame-critical: Filament's
  own `parallel_for` over renderables, culling and the shadow cascades all draw from them.
  `pool.rs` already names the hazard from this side, in the paragraph explaining why the count
  here is four rather than mbgl's three — "a worker here does the fetch too, so a blocked worker
  is not merely idle — it is holding a slot that has no CPU work to do". Blocking work-stealing
  render workers on sockets starves the renderer during precisely the frames the map is loading,
  which is what the onion exists to make good rather than worse.
  **The right shape for decode, and not yet available for it.** MVT parse, tessellation and glyph
  raster are bounded, fork/join and non-blocking, which is what a work-stealing dequeue is for.
  Two things stand in the way. There is nothing to hand over yet: jobs here interleave fetch and
  decode in one closure — `boot` fetches the sprite sheet and decodes it in the same submission —
  so the split has to exist before it can be scheduled anywhere. And DR-14 makes impeller-rs the
  second consumer, which has no job system; a core that requires Filament's cannot serve it.
  **Blocked mechanically today regardless.** `filament::Engine::getJobSystem()` is exported from
  `libfluorite_core_ffi.so` — 2175 symbols, 493 of them `utils::` — but the count of exported
  `utils::JobSystem::` methods is zero. The reference can be obtained and nothing can be called
  on it: no `createJob`, no `run`, no `runAndWait`, no `adopt`. The mechanism is fluorite's
  `CMAKE_CXX_VISIBILITY_PRESET hidden` plus explicit `FLUORITE_API` marking, not a version script
  as this first said — so reaching it means marking what should escape, which is a change to
  fluorite rather than to this. Measured against a prebuilt `libfluorite_core_ffi.so` rather than
  one built from the checkout in hand; the counts should be retaken against a fresh build before
  anyone acts on them, and the same question applies to `filamat`, which fluorite links (so a
  runtime material compiler is in the process) but does not obviously export.
  **What the question is really about is oversubscription, and it is unmeasured.** Four workers
  here plus Filament's `N-1` on a four-core part is about twice the cores, with the OS scheduler
  mediating two pools that cannot see each other. That is a plausible source of frame-pacing
  jitter and it is exactly the kind of claim this document has been wrong about when it reasoned
  instead of measuring. DR-22's order applies: make it work, instrument, then re-architect on the
  numbers.
  **If it does measure badly, the answer is not to call `getJobSystem()` from here.** It is to let
  the consumer supply the pool: `Pool` grows a submit trait, `tessella_fluorite` implements it
  over a C++ shim onto the job system, impeller-rs implements it over whatever it has. That keeps
  policy process-scoped per §5.5, keeps the binding thin, and satisfies DR-14. It costs nothing
  to leave open, since `Pool` is already behind an interface at every call site.
- **A style whose layers draw from no source paints nothing.** Found while joining the consumer:
  a background-only style creates, resolves, reaches ready, emits a frame — and draws no geometry
  at all. `Map::tick` builds its buckets from the tiles in `drawn`, and `drawn` comes from the
  substitution pass over tiles that *have* buckets, so a style with no sources has an empty drawn
  set and the sourceless layers behind it are never reached. mbgl draws background as a
  full-viewport layer rather than per tile, which is why the question does not arise there.
  Whether this matters is a §13.2 question rather than a bug report: never-blank says a view
  shows something at every moment, and the moment before any tile has landed is exactly when a
  background is the only thing there is to show. The options are to draw sourceless layers over
  the cover regardless of what has buckets, or to accept that a map with no data is blank and
  say so. Not decided here because it changes what every first frame emits, and the measurement
  that should decide it — what mbgl puts on screen in those frames versus what this does — is
  the one §12.10 already asks for.
- **What the Filament backend must author, measured.** The blocking unknown for pixels was how
  many materials a backend needs, and the ABI's thirty-five shader families is the wrong answer —
  it is the worst case, not the case. The host probe reports the distinct
  `(builtin_shader, permutation)` pairs a real frame asks for, so the number is per style and
  measured rather than guessed. A style with a background, a fill, a fill with an outline, a line
  and a circle over one GeoJSON source needs **five families and five permutations**, drawn as
  eight batches over thirty-nine entries from thirty geometries: background (3), circle (5),
  fill (11), fill outline (12), line (25). Nothing about that is a ceiling — a real basemap adds
  symbols, patterns and raster — but it does mean the backend can be brought up one family at a
  time against a style that exercises exactly that family, and `host_join` asserts the set by id
  so a family appearing or disappearing is not a quiet change. What the count does not settle is how
  many *materials* that is, because a family is not a material — see the next entry.
- **Materials vary with the style, and are fixed the moment it parses.** Measured, against a
  fluorite built from `origin/main` (`ec0a56bd`) rather than a stale artefact. Shader identity
  on the wire is the pair (family, permutation). The family is static: it follows the layer type
  and the ABI freezes thirty-five of them. The permutation is not — it is a bitmask over the
  family's attribute ids saying which paint properties reach the shader as *uniforms* rather
  than as vertex attributes, so making a property data-driven clears its bit. One fill layer,
  three styles, three keys: constant `fill-color` gives 62, a data-driven `fill-color` gives 52,
  and data-driven `fill-color` and `fill-opacity` together give 48.
  **What that means is better than it sounds.** The key is a function of the layer's paint
  expressions alone — not of the tile data, not of the camera — so a style's whole permutation
  set is known when the style parses, before a single tile arrives. It can be enumerated up
  front.
  **Precompiling the whole space is out.** The ceiling is 2^(attributes in the family), and the
  families are LINE 11, SYMBOL 10, FILL_EXTRUSION 8, CIRCLE 8, FILL 6 — about 3700 materials
  across all of them, which DR-12 rules out on size before anyone asks what `matc` would cost.
  **So the options are:** *(a)* a per-style bundle compiled ahead with `matc`, which works
  because the set is knowable at parse time and small in practice — this style needs five;
  *(b)* run-time compilation through `filamat`, which fluorite links into the process but does
  not export (`filamat::MaterialBuilder` symbols exported: zero — the same visibility question
  as the job system); *(c)* one material per family with the data-driven properties behind
  specialization constants or dynamic branching, trading permutation count for shader cost.
  Not decided. What is settled is that the Filament side is reachable either way:
  `Material::Builder::package` is exported, as are 98 `MaterialInstance` methods and 36
  vertex/index buffer builder methods.
- ~~**One bad layer loses the whole tile.**~~ Closed by matching the oracle: a value the slot
  cannot type now takes the property's default rather than refusing the feature.
  `PropertyExpression::evaluate` falls back when the expression fails *or* when
  `fromExpressionValue` cannot type what it returned, and the second case is the ordinary one --
  `["get", "render_min_height"]` over an extract where most buildings do not carry that property
  returns null for most of them. The entry below is what it looked like before that was
  understood, kept because the *shape* of the hazard is real even though this instance is gone:
  a build job still fails as a unit, so any future per-feature refusal still costs the tile.
- **What liberty needs, measured.** With the fallback and the ring fix below, OpenFreeMap's
  `liberty` draws at Berlin z14: 653 geometries, 1,560,280 vertices, 735 batches over 1,882
  entries, and **nine shader families** — background (3), fill (11), fill outline (12), fill
  pattern (13), fill outline pattern (14), fill extrusion (16), fill extrusion instanced (17),
  line (25), symbol SDF (33). That is the material inventory for a production basemap, against
  five for a synthetic style and four for demotiles, and it is the number the `matc` bundle has
  to cover.
- ~~**A frame that does not fit the ring is never retried.**~~ Fixed. `Map::tick` fails without
  emitting when the frame will not fit, and the attempt had already *spent* the damage gate, so
  the next tick saw nothing to redraw and the map went idle for ever holding a frame it never
  sent. Marking dirty again on the error is the whole of the retry. Found on liberty, whose first
  frame is 1.5 million vertices and does not fit the 4 MB ring the probe defaults to: it reported
  a ready map, every tile landed, `wanted` down to zero, and six records on the wire. It now
  reports `RingFull` every tick instead, which is the honest answer -- the consumer is behind and
  can be given a larger ring or drained faster, and either way it is told.
- **Ring sizing is a style property, not a constant.** liberty's first frame needs more than four
  megabytes; demotiles needs far less. Nothing currently helps a consumer choose, and the failure
  mode before the fix above was silence. Worth a recommended minimum derived from the cover, or a
  counter a consumer can watch.
- **One bad layer loses the whole tile, and real styles have bad layers.** Found by pointing the
  host probe at OpenFreeMap's `liberty` basemap: 13 tiles failed with "layer `building-3d`:
  property `fill-extrusion-base` evaluated to a value that is not a number", and because a build
  job fails as a unit the map drew *nothing at all* — not the roads, not the water, not the
  layers that compiled perfectly. mbgl in the same position draws everything else. This is a
  robustness gap rather than an expression bug to fix and forget: the expression bug is real and
  worth fixing, but a style will always eventually contain a layer this build mishandles, and
  losing the tile for it is the wrong response. The fix is per-layer isolation inside
  `build_job` — a layer that will not build is dropped the way `reject_uncompilable` drops one
  that will not compile, and counted. Not done here because it changes what a partly-broken tile
  emits, which wants deciding rather than assuming.
- **A ready, empty map now says why.** Three states could produce it and none was reported: a
  source that would not resolve (already covered by `Readiness::Failed`), tiles that would not
  build, and layers refused as uncompilable before a tile was ever asked for. The last two are
  now counted with their first reason and surface through `tessella_status`, which is what turned
  both of the findings above from "it draws nothing" into a sentence naming the layer and the
  property. Silently blank has been caught four times in this document; this is the third place
  the answer was to report rather than to guess.
- **Nothing drew above a source's maxzoom.** *Fixed.* Found rendering liberty, whose
  `openmaptiles` source stops at 14: z13 drew 727 primitives and z14 drew 510, while z15 and z16
  drew *zero* — readiness `Ready`, no failed tiles, no rejected layers, nothing on the wire. The
  ordinary case of a user zooming in on a city, and precisely what §13.2's never-blank rule exists
  to forbid.

  The cause was two coordinates for one tile. Above a source's maxzoom the cover asks for z16 and
  the data is `overscaled(14, x>>2, y>>2, 16)`; `TileSource` stored what landed under the *data*
  tile while the frame loop looked it up by the *cover*. `TileId` compares all four fields, so
  inside a source's range the two are equal and everything worked, and outside it no lookup could
  ever match.

  Re-keying the store on the cover was the obvious fix and was wrong: the key is shared across
  sources, a raster source covers at its own zoom, and re-keying merged buckets that belong apart
  — it regressed *every* zoom to zero. What works is a second index, cover to data tile, consulted
  only when the direct lookup misses, which leaves the working path untouched.

  The second half is where it is recorded. Written as tiles land, only one coordinate per tile got
  an entry — the dedup keeps a single job for a data tile that serves sixteen z16 coordinates, so
  fifteen of every sixteen stayed empty and the frame came back mostly black. An alias is
  arithmetic, not a result: it is known when the job is *planned*, and recording it there is what
  fills the cover. Both halves are regression-tested in `source.rs`.
- **The camera diverged from mbgl above z13.** *Fixed -- three separate faults, found by
  comparing landmarks against `mbgl-render` rather than aggregate colour counts.*

  **The frame was drawn upside down.** Flipping our z14 render collapsed the water centroid's
  vertical error from +270 px to -10 px, which said mirrored rather than translated. An
  orientation probe -- a quad at clip y +0.2..+0.9, the top of the screen in the OpenGL
  convention mbgl's matrices are written for -- landed in readback rows 3..25 instead of 38..61,
  the exact mirror. Filament's clip space has +Y down, and identically on its Vulkan and OpenGL
  backends, so this is Filament's own convention and not a backend's NDC. The camera's projection
  now carries that flip, which is the whole of the difference between the two conventions.

  **The scissor was computed in the other space.** It is derived in C++ from the tile's matrix, so
  unlike the geometry it did not pick the flip up, leaving a rectangle that was the mirror of what
  it should bound. Each tile was clipped to where its own reflection overlapped it: a band across
  the middle, 505 of 700 rows, with black above and below.

  **Overscaled tiles were placed by the coordinate that asked for them.** Buckets were keyed by
  the cover, so a z14 tile's 0..8192 geometry was placed by a z16 tile's matrix -- a sixteenth of
  the size, sixteen times too much world on screen, and drawn once per cover tile that resolved to
  it. The serving tile's own coordinate now places it, clips it and identifies it, and the tile is
  drawn once however many coordinates it answers. This is what mbgl's clamped cover does: above a
  source's maxzoom its cover *is* a handful of canonical tiles, not a fan of overscaled ones.

  Against mbgl afterwards, on water area and centroid: z13 1.02 at (+1, -1), z14 0.98 at
  (-16, -10), z15 0.99 at (+7, -19). z16 is the same ground at the same scale, confirmed by
  matching it against the centre quarter of both z14 renders.

  The lesson is in how long it hid. Green, water and grey pixel counts were matching mbgl to a few
  percent and that was read as the frame being right; a histogram cannot see a translation, still
  less a reflection, because a mirrored view of the same city has nearly the same one. Landmark
  position is the test that separates them, and it is what the mbgl comparison should assert on.
- **Fill-extrusion drew its faces far too dark.** *Fixed.* At z16 liberty's `building` fill is
  past its maxzoom of 14 and the buildings come from `building-3d`, painted `hsl(35,8%,85%)` --
  a light beige -- which we rendered around `rgb(98,97,96)`.

  Two faults, both from misreading which shader this family is. DR-16 settles the build on Vulkan,
  where mbgl defines `MLN_USE_FILL_EXTRUSION_INSTANCING`: roofs go through this shader and walls
  through the instanced one as their own family, and in that branch attribute slot 2 is
  `outline_pos` rather than `normal2d`. Our material read a normal out of that slot -- which holds
  `decimals_ed`, a packed coordinate -- and computed a facing from noise. mbgl's instanced roof
  shader hardcodes `vec3(0, 0, 1)`, because every vertex it sees is on a horizontal surface.

  The second was the lighting itself, invented rather than ported. mbgl's directional term
  *brightens* a lit surface: at full incidence it is `max(1 - luminance + intensity, 1)`, never
  below one, and only a surface facing away is scaled, by `1 - intensity`. Multiplying the colour
  by the intensity is what made every building near-black. The term is mbgl's now, including the
  light position, which was not being passed at all.

  Buildings land at `#e3e0dd` against the oracle's `#e7e4e1`, over 130,846 pixels against its
  132,816. Background and landcover match exactly. The residual 4/255 is unexplained and small
  enough to be a light-parameter or rounding difference; worth a look when the walls land.
- **Fill-extrusion heights work.** *The two entries above this one claimed they did not, and both
  were wrong.* Recorded because the mistake is more instructive than the fix.

  The chain is sound at every step, each verified against a real z14 Berlin tile rather than
  reasoned about: the MVT decoder reads `render_height` on all 236 building features with a
  believable spread, 0 to 103 metres; `resolve_paint` binds it as an `Attribute` carrying
  `Get { key: "render_height" }`; the expression evaluates against a real feature; `PaintBinder`
  writes 21,051 nonzero entries out of 21,112; and the wire carries them, base and height
  interleaved at one 8-byte stride. Forcing a 500-metre height moves 160,440 pixels, so altitude
  reaches clip space as well.

  What went wrong was the sampling. The first roof drawable a frame uploads is the *prefetched
  z13 ancestor*, and openmaptiles carries no `render_height` at z13 -- the layer's own minzoom is
  13 but the field arrives at 14. Its 14,613 vertices are genuinely all zero. I dumped that one
  drawable, saw zeros, and wrote up a producer bug that did not exist. Printing the tile id beside
  the values showed z14/8800/5373 with 21,051 nonzero and a 103-metre maximum, sitting right
  underneath.

  This is the same error as the four before it, and the pattern is now specific enough to name:
  **a per-tile quantity sampled from one tile is not a measurement.** The frame holds tiles from
  several zooms at once, deliberately, and the first to arrive is the coarsest -- so the first
  sample is systematically the least representative one. Anything read out of a drawable must
  print its tile id beside it, which is the whole of what turned this around.

  The building coverage that looked excessive -- 158,842 pixels against mbgl's 132,816 -- was not
  a rendering fault either, and neither was the z13 ancestor suspected of it: the frame draws
  `zoom_14 124` and nothing else, so the ancestor is uploaded and correctly not drawn.

  The measurement was wrong. It compared the count of *our* most common colour against the count
  of *mbgl's*, which are two different colours. Classifying pixels instead, at z16 against the
  oracle: the frame differs by +16,178 grey and -13,953 dark, and the dark is mbgl's text. Labels
  sit on top of buildings, so where the oracle has type we have the grey underneath. Excluding the
  pixels mbgl draws a label on drops the grey excess to +3,237.

  What is left after that: green -6,759, road-yellow +2,910, grey +3,237, water -3,019. The water
  is the same cause once more -- most of the oracle's blue at this zoom is transit badges and POI
  markers, not water, and the genuine ponds appear in both. So the visible gap from the oracle at
  z16 is dominated by the symbol family, and the fill and line families are close.

  Twice in one investigation the metric, not the renderer, was what was broken. Both times the fix
  was to classify rather than to count one colour, and to exclude what the oracle draws and we do
  not before comparing what remains.
- **`c_surface` can fail when two `cargo test` runs overlap.** It builds the staticlib itself and
  links a C binary against it, so two workspace runs started back to back race over the same
  artifact -- seen once, passing alone and on both of the next two full runs. Harmless to a human
  running the suite once; worth a guard before CI, which will not be running it once.
- **Labels draw.** *Fixed, two faults, one on each side.*

  **The glyph atlas was published once per font stack to one texture id.** liberty names several
  -- regular, italic, bold -- and each overwrote the one before it, so the frame drew with
  whichever landed last: 21,118 then 5,411 then 1,829 nonzero bytes, the last nearly empty. A
  label's coordinates were right and the pixels under them belonged to another font. Each stack
  now takes its own id from `GLYPH_ATLAS_BASE`, and the symbol drawable names the atlas holding
  its own glyphs; the order the frame publishes them in is passed to the encoder rather than
  recomputed, so the upload and the reference cannot drift apart. A bucket drawing more than one
  stack still takes the first, which is where mbgl splits the bucket and this does not.

  **The fifth vertex attribute did not bind in the consumer.** A symbol carries five, and the
  fade opacity in the last one read as zero however the wire described it -- the wire carries
  255.0, checked. A fade of zero multiplies every glyph away, which is why the labels were
  invisible rather than merely wrong. The projected position and the fade now travel together in
  one `FLOAT4`: three components and one, both per-vertex floats, so the pair costs nothing over
  sending them apart.

  z16 draws 18,992 dark text pixels against the oracle's 13,973 -- more, not fewer, because every
  label is drawn. mbgl's placement pass culls the ones that collide, and that is the next piece.

  Four times in this hunt I measured the wrong population and drew a conclusion from it: a
  histogram truncated to its lowest buckets, twice; a diagnostic that painted the whole screen
  opaquely, whose colour buckets were then mostly map and not glyph; and a probe that multiplied
  the value it was reporting by the coverage, so it reported `fade x alpha` as `fade`. Each looked
  like a finding. The habit that catches all four is the same: say what population is being
  measured, and check the count against what it should be before reading the values.

- **Symbol collision runs, per bucket.** `tessella-place` had the whole thing -- candidates,
  a grid, `place`, fades -- and `ViewSymbols` had `frame`, `write_opacity` and `write_positions`
  wrapping it, tested and never called from the frame path. `lay_out` returns the instances
  placement needs and the encoder was discarding them. Wired now: at z16 one layer offers 142
  labels and places 127, and the frame's dark text goes from 18,992 pixels to 8,665 against the
  oracle's 13,973 -- from more than the oracle to fewer.

  Two things it is not yet:

  **Per bucket, where mbgl is per frame -- and sharing the grid made it worse, which is the
  interesting part.** `ViewSymbols::frame_in` now takes the caller's grid, and threading one grid
  through the encode loop was tried and reverted. The encode loop runs in *painter* order, bottom
  first, and mbgl places in **reverse** render order so the topmost label claims its space before
  the layers under it are offered any. Sharing a grid bottom-first let the lowest label layer --
  2,256 house numbers at z16 -- fill it before a single place name was offered, and the frame came
  back emptier than with no sharing at all. The fix is a placement pass over the symbol buckets in
  reverse order *before* the encode loop, and `frame_in`, `begin` and `settle` are the pieces it
  needs; the retreat is recorded in the code beside the call.

  Frame-unique identities came out of the same attempt and stay: numbering each bucket's labels
  from one collided across buckets the moment they shared a `ViewSymbols`, so the second bucket's
  first label overwrote the first bucket's fade state and every bucket after read another's
  decision.

  **Stepped to rest rather than animated.** A fade is a per-frame animation keyed by cross-tile
  id, and nothing carries that state between frames here, so one step leaves every label at the
  opacity it fades *from* -- zero, drawing nothing, which is what the first attempt did. The loop
  settles it instead. Carrying the state needs the cross-tile index, and that is what would make
  a label fade rather than appear.

  Being *under* the oracle's text now rather than over it is unexplained and worth a look: it may
  be the padding defaults, or the sixteen pitched batches still skipped.

- **The glyph fetch happens once, and now it happens once the tiles have stopped arriving.**
  *Fixed.* `want_glyphs` fires on the first tile to land, and on a cold start that is the
  prefetched ancestor -- a tile the frame does not draw. Measured on liberty at z16 it asked for
  90 codepoints while the two tiles the frame is made of, landing moments later, wanted 451 and
  476. Every letter outside that ninety was missing from every label, which rendered the map's
  captions as alphabet soup, and which ninety you got depended on which tile won the race, so no
  two runs agreed.

  It now waits for the in-flight queue to drain. That is the weakest condition that works: not
  "the cover is complete", which one failing tile would block forever, but "nothing further is
  coming", which a failure satisfies as surely as a success. One fetch still, so the map still
  goes quiet and the map is still handed its fonts exactly once.

  Both symptoms went with it. z14 draws 32,254 pixels of text against the oracle's 28,000-odd, and
  the labels read as words -- "Reichstag Building", "Der Bevölkerung", "Brandenburger Tor",
  umlauts and all. And the frame is reproducible: 32,254 on three consecutive runs, where before
  the same view gave 6,502, 8,665, 6,502, 6,502.

  This also supersedes the incremental-fetch work, which is not needed for a static camera and
  whose second `set_fonts` blanked the map. A camera that moves to ground the first fetch did not
  cover will still want it, and the defect behind it is real and recorded below; it is simply no
  longer on the path to legible text.

- **A layer's `minzoom` and `maxzoom` were never applied.** *Fixed, and it answers two questions
  at once.* `minzoom` appeared nowhere in orchestrate or layout: every layer of a style was built
  for every tile at every zoom. liberty's POI layers start at 15, 16 and 17, and at z14 they were
  filtered, shaped, placed, encoded and drawn -- one of them, `poi_r20` at minzoom 17, offered
  about nine thousand labels across four tiles.

  That is why the map was captioned with shop names the oracle does not show, and it is also why
  the type looked the wrong *size*: a POI label is set larger than a street label, so drawing the
  wrong layers changes the apparent size of the text as much as the amount of it. The size
  arithmetic was never wrong -- `fontScale` measures 0.66 on glyph pixels, which is 16/24 with a
  perspective ratio of one, exactly mbgl's formula.

  The rule is mbgl's, and the asymmetry is the point: `minzoom` inclusive, `maxzoom` exclusive, so
  a layer with `maxzoom: 14` is the last thing drawn at 13.9 and gone at 14. Applied against the
  tile's `overscaled_z`, which is the cover's zoom and so the camera's.

  **What it exposes.** z14 goes from 32,254 pixels of text to 3,917 against the oracle's 28,000 --
  from too much of the wrong text to too little of the right. The fourteen symbol layers liberty
  enables at z14 are mostly road names, placed along lines, and 17 of those batches are skipped by
  the consumer because a line label lays out in the map's plane rather than the viewport's and
  that arrangement is not written yet. So the previous number was not close to the oracle's, it
  was wrong twice over in opposite directions, and this is the honest one.

  Line labels are now the whole remaining gap in text.

- **Line labels walk their roads.** `write_line_positions` steps each glyph of a line-placed
  label along the projected road and writes where it landed, with the angle of the segment it
  landed on -- which is what makes a street name bend with its street rather than sit as one
  rotated block. The arithmetic was all in `project.rs` and exercised only by tests; `place_upright`
  had no caller outside them, and `symbol_preview.rs` showed the intended sequence.

  Into the label plane, which for a label lying flat on the map is pixels relative to the tile:
  `pixels_to_tile_units` is tile units per pixel, so dividing by it is exactly that conversion. The
  glyph distances layout records are screen pixels, so walking a line in tile units with them
  would misplace every glyph by the scale factor.

  This is why the drawable's label-plane matrix is the identity for a walked label, and the
  consumer no longer skips those batches. An earlier attempt went the other way -- give the walked
  label a real plane matrix so the existing shader could place it -- and broke
  `symbol_tiles::the_alignments_decide_the_drawables_matrices`, which asserts mbgl's rule outright:
  a walked label gets the identity because the walk *is* the projection. The test was right and the
  change was a divergence from the oracle dressed as a fix; the walk is what the identity was
  always waiting for.

  z14 goes from 3,917 pixels of text to 5,530 against the oracle's 28,000, and the labels visibly
  follow their roads -- "S p r e e" along the river where mbgl puts it, "Tiergarten" and
  "Spreebogen" along theirs. The gap that remains is mostly highway shields, which render their
  first letter and no number because the icon family has no material, and label count.

- **Icons are laid out, bound and encoded; the shields still do not show.** `lay_out_icons` had
  no caller outside its tests, so a symbol's sprite half never existed: a highway shield drew its
  letter and no shield. It now runs, and the pieces behind it are in place -- a symbol bucket
  declares two drawables when its layout resolved any sprite (`SymbolLayout::has_icons`, asked
  before shaping because the count must settle before anything is encoded), `bindings_for` emits
  the second at sub-layer 1, `part_of` maps that to the icon record, and the encoder returns both.
  Family 32 has a material, derived from the SDF one: same vertex stage, because where a symbol
  goes has nothing to do with what fills it, and a fragment that samples rather than resolving a
  distance field.

  Measured: icons resolve (layer 98 gives `road_4`, `road_3`, the shield sprites; layer 94 gives
  `bus`), the batches reach the consumer, find a material and find their meshes. What does not
  follow is any change on screen, and the icon batches seen arriving are layer 94's transit
  markers rather than layer 98's shields.

  **The icon drawable had no matrix slot.** Symbol drawable UBOs were packed from `matrices(0)`
  alone -- the glyph half -- so the icon drawable's `ubo_index`, assigned by the order, pointed one
  slot past the end of the layer's buffer. The consumer counted it `unplaced` and skipped it, which
  is why the batches arrived, found a material, found a mesh, and produced no renderable. Both
  halves are packed now, in sub-layer order, the way a fill packs its triangles and its outline.
  z14 goes from 550 primitives to 564 and `unplaced` from 32 to 18.

  **And the sheet size was zero.** `SymbolDrawableEntry` was built with `[0.0, 0.0]` where
  `texsize_icon` goes. An icon's texture coordinates are sheet pixels and the shader divides by
  that size to get them into 0..1, so a zero is a division the consumer has to guard -- and
  guarding it with one leaves the coordinates in the hundreds, wrapping the sampler round to
  whatever sits at the origin. That is why an icon drew as a flat black square. With the sprite
  sheet's real dimensions the sprites appear: the blue transit markers draw with their own
  artwork.

  What is left is where they sit. mbgl puts a transit icon *before* its label; ours draws it at
  the anchor, on top of the text. That is `icon-text-fit` and the text offset that goes with it,
  and it is a layout question rather than a rendering one.

  Two things were fixed on the way and are worth keeping separate from that. An icon was being
  handed the *glyph* atlas's dimensions -- the block carries `texsize` and `texsize_icon` because
  one shader can sample both sheets, and a drawable sampling one still has to be told which. And
  the claim that only layer 94's icon batches arrived was wrong: it came from a log capped at four
  entries that caught only 94's, the same truncated-sample mistake recorded twice above.

- **Label count: we now draw too few, not too many, and the walk is where they go.** With the
  layer zoom ranges applied the population is finally the oracle's, and z14 sits at 6,518 pixels
  of text against mbgl's 28,000 -- a quarter. Measured rather than guessed: collision is not the
  bottleneck. Layers 97 and 98, the road names, offer hundreds of line labels each and placement
  draws most of them -- 173 of 321, 185 of 258, 136 of 249. What loses them is the *walk*: 1,204
  succeed and 1,501 answer `NotEnoughRoom`, the road running out before the last glyph does.

  A failed walk now hides its label. It used to leave whatever `lay_out` put in the dynamic
  buffer, which is the anchor in *tile* units, and the shader reads that buffer as label-plane
  coordinates -- so the label drew some thousands of pixels from where it belonged. Hiding them
  removed 224 pixels of stray marks and, more usefully, made the frame exactly reproducible:
  6,518 twice, where it had been 6,742 and 6,756.

  Whether 55% is the right failure rate is the open question. mbgl drops labels that do not fit
  too, and `get_anchors` is supposed to place anchors where they *will* fit, so a rate that high
  suggests either the anchors are spaced for a different scale than the walk uses or
  `LineOffsets::default` is not what the layer asks for -- `font_scale` is 1.0 there and nothing
  builds one from the layer's own `text-size`. That is the next thing to establish, and it is
  worth establishing before touching collision again.

- **Empty shields strung along a road: icons never compete for space.** Seen in the render as
  chains of small blank boxes following a street where the oracle draws one shield with a number
  in it. Two causes, one fixed.

  **Fixed.** A line label whose glyphs found no room had its text hidden and its *icon* left
  drawing, because the icon's opacity is written from the text's placement decision and the walk
  fails after that. A shield exists to carry a number; without one it is an empty box, and at
  every anchor along a road it is worse than nothing. `write_line_positions` now reports the
  labels that found no room and their icons are hidden with them.

  **Two more fixed, and the chain is still there.** The reordering turned out to cost nothing:
  `lay_out_icons` needs only the text's *instances*, which exist the moment `lay_out` returns, so
  icons are shaped before placement now and `FrameLabel::icon` carries a real box. And
  `write_opacity` was writing the *text's* channel into the icon's buffer, throwing the icon's own
  decision away -- `write_icon_opacity` reads `state.icon`, which is what the two channels are for.

  Neither removed the chain. Turning the icon family off in the consumer removes it outright and
  leaves a map that reads well, so the boxes are certainly the icons; what keeps them is that a
  line-placed symbol has one instance per anchor, the *text* at most of those anchors fails its
  walk and is hidden, and the icon has no walk to fail. Hiding the icons whose text found no room
  is implemented and does not account for all of them, so either the anchors are far denser than
  `symbol-spacing` should give or the icon boxes are not overlapping each other in the grid the
  way they visibly overlap on screen. That is the next thing to measure -- anchor spacing first,
  since it is one number.

  z14 sits at 5,295 pixels of text with icons on, 4,683 with them off; reproducible either way.

  z14 sits at 5,971 pixels of text, reproducible across runs.

- **`symbol-spacing` was stored in pixels in a field documented as tile units.** *Fixed, and it
  is what strung the shields along the roads.* `LineOptions::spacing` says "in tile units" and
  `get_anchors` walks a line whose coordinates are tile units; the style's number is *pixels*, 250
  of them by default, and it was being stored unconverted. A tile is 8192 units across the 512
  pixels it is drawn at, so an anchor landed every 250 units where the style asked for every
  4000 -- sixteen times the labels, which on a map is a road wearing a shield every few pixels of
  its length. mbgl calls the factor `tilePixelRatio`.

  Converted where the field is filled, which is what makes the code agree with its own
  documentation and leaves every caller that already passes tile units correct. Doing it at the
  `get_anchors` call instead -- which is where mbgl multiplies -- would mean the field holds
  pixels, and eleven layout tests pass it tile units.

  The chain is gone. z14 draws 3,397 pixels of text against the oracle's 28,000, reproducible.
  Fewer than before because there are now the right number of anchors rather than sixteen times
  too many.

  Two test fixtures were recalibrated rather than relaxed: they had been written against the old
  behaviour, and at the corrected spacing their roads are no longer long enough to repeat a label
  at all, which is the premise both tests rest on. They now state small pixel spacings and say
  why.

- **`text-size` reached nothing that needed it.** *Fixed in three places, one cause.* Every
  label in every frame drew at 16 pixels whatever the style asked for, was spaced along its road
  as though its glyphs were 24 pixels tall, and was anchored as though its name were a fortieth of
  its real length. The three are one thing seen three times: the size the style states never got
  to the code that had to scale by it.

  1. The frame read `text-size` through `resolve_layout`, which answers from the per-kind spec
     tables and has no symbol table -- it returns an *empty map* for a symbol layer, so the read
     always fell through to its own `unwrap_or(16.0)`. Silent, and the wrong kind of silent: a
     missing table and a style that says 16 are indistinguishable. The evaluation the layout
     already does now lives in `tessella_style::property::layout_value` and both sides call it,
     which is also what stops the two drifting again -- the shader scales a glyph's corners by
     `size / 24`, so a size derived twice by two routes is a label whose quads and whose spacing
     disagree.

  2. The walk that spaces a label's glyphs along its road used `LineOffsets::default()`, whose
     `font_scale` is one. Shaping works at one em -- 24 units to the em, whatever the label is
     finally set at -- so the distances it records are ems and the walk has to scale them the way
     the shader scales the corners. It did not, so a 12-pixel road name was stepped out at 24
     pixels: the letters stood apart with gaps between them, which is what "Karl-Marx-Allee" read
     like before this. mbgl's `fontScale` in `reprojectLineLabels` is this number.

  3. `get_anchors` was handed a `box_scale` of one, so a label's em-space extent was compared
     against a line measured in tile units -- forty times too short, so every road looked long
     enough for any name. The anchor then landed a few pixels from where the line stopped, close
     enough to accept and far too close for the label to fit, and the walk found no room and
     dropped it. Labels placed and thrown away rather than placed where they fit. mbgl's
     `textMaxBoxScale` is `tilePixelRatio * text-size / 24` with the size taken at zoom 18, which
     is deliberate: one size for every zoom is what stops labels jumping as the map zooms.

  Measured, not reasoned. Eight lines at eight angles, equal length, `symbol-placement: line`:
  none of the eight placed a label before, all eight after. The same word point-placed and
  line-placed now measure the same letter pitch -- 28.0 to 28.5 pixels at `text-size` 40, against
  28.33 predicted from the font's own 17-unit advance for `H` -- where before the point label
  measured 11.4, which is 17 at a size of 16.

  Two fixtures moved rather than relaxed. `road()` was 4,000 tile units, from when a label was
  measured forty times short and any road looked long enough; it is the full 8,192 now. And the
  spacing comparison ran 25 pixels against 6.25, which no longer differs by much because a label
  cannot repeat closer than its own length -- it runs 200 against 12.5, where spacing is still
  what decides.

- **Symbols were clipped to their tile, and are not.** *Fixed on both sides of the wire.* A label
  is drawn from an anchor and its glyphs overhang the tile that owns it, so clipping it cuts a
  road name in half at every tile edge it crosses: a horizontal slice through the letters where
  the edge runs across the label, the leading glyphs missing where it runs down through them.

  The producer emitted symbol drawables with `ENABLE_STENCIL`, on a comment claiming the oracle
  showed them carrying a fill's flags. The dump it cites says the opposite -- `sh0033` and
  `sh0034` carry `flags=0011` where every fill and line carries `0111` -- and mbgl's
  `RenderSymbolLayer` never calls `setEnableStencil`, whose default is false. The guess that
  comment talked itself out of was the right one.

  The consumer scissored every drawable regardless, which is the half that was actually cutting
  the glyphs: `TSF_NO_SCISSOR` restored them. §11.7's clip obligation is per drawable and the flag
  is how it is stated, so the flag now travels with the geometry and gates both the scissor and
  the stencil test.

- **Painter order was never stated to Filament.** *Fixed in the consumer.* The style's layer
  order reaches it intact -- the producer sends the frame in painter order and `DrawList` never
  reorders -- and then nothing said so. Renderables were banded by `layerIndex / 32` into one of
  Filament's three priority bits, so every layer of any style under thirty-two layers shared a
  band, and within a band Filament sorts blended primitives as it sees fit. Roads painted over
  the labels naming them, cutting the glyphs wherever a road crossed: it reads as dropped letters
  rather than as a layer in the wrong place, which is why it was filed under missing glyphs for
  as long as it was.

  The band carries the render pass now -- the coarse half of painter order, since mbgl draws the
  opaque pass and then the translucent one -- and `blendOrder`, fifteen bits made global, carries
  the fine half from the count of renderables issued this frame. Priority takes precedence, so
  the two nest rather than compete.

  The pass half is not incidental. A background emits into both passes and the producer sends its
  opaque-pass drawable *after* every translucent one, so stating the order without grouping by
  pass pinned the background last, over the whole map. That looked like an inverted sort key and
  was read as one -- counting the blend order down instead, which changed nothing, because the
  background was never the thing being ordered. `TSF_ORDER_LOG` is what settled it.

  Berlin z14 goes from 885 pixels of text to 2,050, reproducible, and every label reads whole.

- **A label reserved its pitch padding as though the label covered it.** *Fixed, and it is why a
  road carried its name once where the oracle carried it four times.* A line label collides as a
  run of circles, and the run reaches about twice the label's length: half a run of *pitch
  padding* on each side, which mbgl adds so collision still works for a label a pitched camera
  stretches into the distance.

  mbgl builds the same padded run and then does not test it. `placeLineFeature` skips every circle
  outside the placed first and last glyph, which at pitch zero is the label's own extent. We
  tested the whole run, so a label 114 pixels wide reserved 272, and two of them 250 pixels apart
  -- which is exactly what `symbol-spacing` asks for -- collided with a neighbour they never
  touched.

  Measured on one straight road with one name, both renderers: mbgl draws four labels 250 pixels
  apart, we drew two at 500. Now four at 250. A 600-pixel road drew none and now draws two, as the
  oracle does.

  Against the *slackened* distance, because that is the value mbgl compares:
  `signedDistanceFromAnchor` is stored with a fifth already taken off. Comparing the raw distance
  instead is a quarter narrower and overshoots the oracle -- Berlin 3,176 against 3,005 rather
  than 3,041.

  Four explanations were falsified first, each by measurement: the anchors (right, and
  `get_anchors` agrees with mbgl line for line), the collision machinery (the same candidates place
  in isolation), a missing collision run, and stale encoding. The one that found it was
  reproducing the two labels in-process and printing the circle runs -- 3..275 and 217..507 for
  labels 114 wide.

- **A name is not printed twice next to itself.** *Added.* mbgl's `anchorIsTooClose` rejects an
  anchor within `symbol-spacing / 2` of another anchor carrying the same text, across the whole
  layout rather than one feature -- a street is usually many features sharing a name, and without
  it each of them labels itself. We had no such filter.

  Berlin goes 3,041 to 2,958 against the oracle's 3,005, and Washington 2,353 to 2,309 against
  1,931. Kept because it is mbgl's behaviour and was missing, not because of the numbers: it moved
  Berlin from one per cent over to two under, which is noise at this distance.

- **All five remaining shader families reach the screen now.** Every parity number before this
  came from a style with a background, a fill, a line and symbols -- a quarter of the renderer.
  One layer per family, against the same oracle, found three that drew nothing; all three are
  fixed, each measured on its own:

  | family | against the oracle |
  |---|---|
  | fill-extrusion | drew already |
  | symbol icons | drew already -- 2,009 pixels of icon in the combined frame against 2,029 alone |
  | circle | 99.91% of pixels exact, 100% within 24/255 |
  | fill-pattern | 44% exact, **100% within 24/255** |
  | raster | 72% exact, 89% within 24/255 |

  The whole style together is 60% within 24/255, which is lower than any of its parts and says
  where the next work is: the raster covers every pixel, so its filtering difference is added to
  everything under it. Mean brightness agrees to within four of 255, so it is not opacity or
  ordering -- the draw order carries background, raster, fill, pattern, line, extrusion, circle,
  icon, label, which is the style's own.


- **A raster source's tiles were fetched, decoded, stored, and never looked up.** *Fixed.* A
  256-pixel raster source covers the screen at one zoom *more* than a vector one -- mbgl's
  `coveringZoomLevel` shifts by `log2(512 / tileSize)` -- and `plan` knows that and keeps a cover
  of its own for it. The frame then walked only the view's cover, so those tiles sat at
  coordinates nothing ever asked about.

  It looked like an interaction between sources, because a raster source alone drew normally.
  It is not: the cover addresses one zoom, and a source at any other zoom is invisible whatever
  else is in the style. Proved by giving the source `tileSize: 512` so its covering zoom matches
  the view's -- the same style, the same two layers, and the raster draws.

  The frame walks those covers now. `Tiles::extra_zooms` reports the zooms a store holds tiles at
  that the view's cover does not address, empty for every vector source; the walk dedups against
  the same `served` set, so a tile already drawn is not drawn twice.

  Against the oracle on a background, one vector fill and a 256-pixel raster source: 72% of pixels
  exact and 89% within 24/255, with the water drawn over the imagery in both. The residue is
  texture filtering on a synthetic gradient, which is its own question.

- **A fill layer with `fill-pattern` draws nothing, and does not fall back either.** *Fixed.* The
  entry below narrowed it to "the consumer's binding or `fill_pattern.mat` itself: the pattern
  rectangles in the drawable UBO, or the samplers", and the first of those was right: the producer
  wrote the *plain fill's* drawable layout for a patterned layer, so `tile_ratio` arrived as zero
  and every fragment sampled one point of the sprite. See the entry on the patterned fill's own
  drawable layout for the whole of it. The layer now draws at 92.9% of pixels exact against the
  oracle with no gross pixels.

- **An extrusion drew its depth pass and threw its colour pass away.** *Fixed, both halves.* The
  producer packs one drawable-UBO entry per drawable, and `ubo_index` is numbered per *layer*
  across every sub-layer the layer emits. An extrusion emits four when it needs a depth pass -- 0
  and 1 draw depth, 2 and 3 draw colour -- and the packing covered only the first two. The colour
  pass therefore indexed past the end of its own buffer, where the consumer counts it `unplaced`
  and skips it: eighteen drawables of thirty-six on a twelve-tile frame.

  What was on screen was the *depth* pass, drawn with colour because the consumer did not honour
  `ENABLE_COLOR` either. That is why a building was a flat footprint in the roof's shade: it was
  geometry that exists to fill a depth buffer, painted as though it were the building.

  Both halves needed fixing together. The producer now packs all four sub-layers -- `matrices`
  yields nothing for one with no bindings, so it is right for an opaque extrusion too, which emits
  2 and 3 alone. The consumer honours `ENABLE_COLOR`, and gives the extrusion families the depth
  buffer they need.

  Depth is scoped to those families rather than read from `ENABLE_DEPTH`, which a background
  carries too: a background is a viewport quad at clip z zero, and zero is the *near* plane in the
  producer's convention, so a background taking part in depth stands in front of every building.
  mbgl draws it `DepthMaskType::ReadOnly` for that reason, a distinction the single wire bit cannot
  carry.

  `unplaced` goes from 18 to 0 and the extrusion frame from 98.1% of pixels within 24/255 to
  **99.9%**. Berlin and Washington are unchanged at 2,958 and 2,309 pixels of text.

  Three attempts failed before the probe existed, each on a full scene where a blank layer could
  mean clipped, failed the test, or never drawn. `depth_probe` separated those, and then the thing
  it could not explain -- the same settings working there and not here -- was the real bug all
  along.

- **A translucent extrusion blends some surfaces twice.** *Fixed.* On the side of a stacked
  building a lighter wedge was composited into an otherwise uniform wall -- a second surface at the
  depth the first just wrote, passing the test and blending over it. Setting `fill-extrusion-opacity`
  to 1 removed it, which is what identified it as blending rather than geometry.

  Two bugs, and neither was visible while the other stood.

  **The consumer drew the colour pass before the depth pass.** `endFrame` reverses the frame's
  batches, because the producer sends front-to-back and a translucent pass with no depth buffer has
  to blend bottom-up. That reversal also turned an extrusion inside out: the producer emits the
  depth-only pass at sub-layers 0 and 1 and the colour pass at 2 and 3, and reversed, the colour
  pass ran first against a cleared buffer. The measurement that proves it: a read-only colour pass
  *with* the prepass and one with **no depth buffer at all** lost the same 6,857 wall pixels, which
  can only hold if nothing ever read what the prepass wrote. It is also why the prepass looked
  redundant -- dropping it rendered pixel-identically. Fixed by reversing at layer granularity and
  restoring the producer's order inside a layer that resolves in depth.

  **The extrusion matrix carried the flat-layer sublayer nudge.** `for_tile_with` applies mbgl's
  `depthModeForSublayer`, which divides a flat layer's depth range so a fill's outline does not
  z-fight the fill it outlines. mbgl draws a fill-extrusion under `depthModeFor3D` instead -- the
  whole range, no sublayer term. One step of `DEPTH_EPSILON` is 9.3e-7 of clip depth after the
  divide, against the 2e-4 a 150-metre building spans in total, and `depth_probe`'s third phase puts
  the tolerance below that: at 1e-6 apart the colour pass is rejected outright. Fixed with
  `DrawableEntry::for_tile_3d`, and pinned by a test, which nothing did before.

  Removing the nudge alone had been tried and reverted as useless -- correctly, on the evidence
  available then. With the order still inverted the colour pass was never tested against anything,
  so no depth change could show. Each fix needs the other to mean anything.

  **mbgl's read-only colour pass is not reproducible here, and does not need to be.** It exists so
  each pixel blends exactly once, and it works there because both passes draw the same drawables
  through the same shaders, making the depths bit-identical. Ours are not: the roof and the walls
  are separate drawables on separate shaders, the walls expanded from outlines and reconstructing
  position from instance attributes. Swept over every comparison function, a read-only colour pass
  scores MAE 5.46 against the oracle where a writing one scores 2.05, and the disagreement shows as
  whole triangles of building where neither surface won.

  So the prepass is skipped at the consumer and the single colour pass writes depth. It resolves
  roof against wall and building against building, which is what the prepass was for. Against the
  oracle: **MAE 2.05 and 164 gross pixels, from 2.08 and 342**, with 36 renderables instead of 54.
  Skipped at the consumer rather than dropped from the stream -- the producer's order is measured
  against mbgl's own capture and has to keep saying what mbgl says; how a backend satisfies it is
  §11.7's business.

  Two things followed from measuring this on controlled styles rather than on the families scene.

  **The walls were flat because a boolean paint property resolved to zero.** Fixed -- see the
  commit. Berlin's buildings, opaque, went from 93.1% of pixels exact against `mbgl-render` to
  **100.0%, MAE 0.00**. Geometry, lighting, gradient and depth are all exactly right; anything
  still visible in a translucent extrusion is the blend and nothing else.

- **A translucent extrusion was drawn opaque.** *Fixed.* The consumer read a layer's opacity out
  of its evaluated-paint block through `opacityOffset`, which had cases for fill, fill-outline and
  background and a default that answers "past the end" so the value stays at one. Neither extrusion
  family was listed, so `fill-extrusion-opacity` never reached the shader and a building at 0.9 was
  composited at 1.

  It hid behind a coincidence: at an opacity of one, not blending *is* the right answer, so the
  layer matched the oracle exactly and every measurement taken on an opaque style said the pipeline
  was correct. What it cost was 3 of 255 on a roof and 14 on a wall -- the roof came out at the lit
  colour instead of nine parts lit to one part what was behind it -- across more than a third of
  the frame.

  Berlin at `fill-extrusion-opacity: 0.9`: **63.1% of pixels exact and MAE 2.44 before, 91.8% and
  MAE 0.45 after.** Opaque styles stay at 100.0% and MAE 0.00.

  The same nesting cost every other family too. The opacity read sat *inside* the branch that sets
  a shared colour, which excludes raster, the symbols and the two pattern variants by design --
  opacity is not a property of having a shared colour. Lifted out, and line, circle and the pattern
  fills are covered by name.

  This is also why the earlier depth work read the way it did. Every prepass and read-only
  measurement was taken with opacity stuck at one, which is the value at which the whole question
  is moot -- so those numbers were comparing an opaque render against itself and none of them meant
  what they appeared to.

- **A translucent extrusion blended overlapping surfaces twice.** *Fixed.* On a stacked building
  an upper block's wall projects over the lower block's roof. Both were blended, and the overlap
  reads as a lighter rectangle let into the wall -- the anomaly on the side of the stacked
  buildings. Two causes, both ported wrong.

  **The wall template used mbgl's `quadTriangleIndices`, not its `fillExtrusionTriangleIndices`.**
  The generic quad winds `0,1,2` then `1,2,3`, and those two triangles turn opposite ways. The
  extrusion has its own set for exactly this reason, and mbgl's comment on it says so --
  "Counter-Clockwise winding order" -- flipping the first to `0,2,1`. Nothing reads the winding
  until something culls by it, and then it is not subtle: back-face culling takes one triangle of
  every quad and leaves the other, so each wall loses a diagonal half. This is why culling looked
  destructive and was written off.

  **Back-face culling was off, where mbgl sets `backCCW` on all four builders.** With the winding
  corrected it is verifiable: at `fill-extrusion-opacity: 1` enabling it is a *no-op* -- 100.0% of
  pixels exact either way, because a building's far walls are hidden anyway -- which is the proof
  that the right faces are going. Translucent, those far walls were being blended through the near
  ones. `front` and not `back`, because Filament's front-facing winding is opposite to mbgl's here:
  culling `front` on the roof is a no-op and culling `back` removes every roof triangle.

  **And the depth prepass is drawn again.** Depth alone cannot prevent a double blend -- the test
  rejects a farther fragment that arrives second, but nothing stops it arriving first -- so the
  buffer has to be filled before any colour is blended, which is what mbgl's prepass is for. It was
  being skipped on the strength of measurements taken while opacity was stuck at one, where the
  question does not arise.

  Berlin at 0.9, against `mbgl-render`: **92.0% of pixels exact with the single pass, 94.9% with
  the prepass**, and the wall is uniform where mbgl's is uniform. Opaque styles stay at 100.0% and
  MAE 0.00.

  **And an extrusion is not clipped to its tile, in either pass.** This was the anomaly, and it
  took three tries to read correctly. The producer marks the colour pass `ENABLE_STENCIL` and the
  depth pass not, which is what mbgl does -- `setEnableStencil(doDepthPass)` on the colour builder,
  the depth builder left at the default of false. There the asymmetry is harmless, because mbgl's
  stencil is what makes exactly one tile paint each pixel and between them the tiles cover
  everything.

  Here a building's geometry runs past its tile's edge by design and nothing paints what the clip
  cuts: the neighbouring tile does not carry its own copy to paint it with. So the clip removes
  wall faces and leaves the background in their place.

  The measurement that settles it is the gross count, not the exact count. Rendering *only* the
  depth drawables, which carry no stencil, gives 8 gross pixels against the colour drawables' 163 --
  identical geometry through an identical shader, differing in one flag.

  Berlin at 0.9, against `mbgl-render`:

  | arrangement | exact | MAE | gross px | worst region |
  |---|---|---|---|---|
  | colour pass only, clipped | 92.0% | 0.41 | 163 | 66 |
  | prepass unclipped, colour clipped | 94.9% | 0.66 | 2,694 | 1,351 |
  | both clipped | 97.8% | 0.13 | 161 | 64 |
  | **neither clipped** | 95.7% | 0.20 | **8** | **1** |

  Clipping only one pass is worse than either extreme: the depth pass writes for the whole building
  and the clipped colour pass cannot paint the part outside the tile, which leaves depth with no
  colour -- a hole rather than a slice. Clipping both trades the holes back for slices and scores
  best on exact and MAE while keeping every visible defect, which is what makes those two numbers
  the wrong ones to steer by here.

  What dropping the clip costs: two tiles' copies of an overlapping building both blend, so 2.1% of
  pixels differ by a shade. That is the 95.7% against 97.8%, and it is not visible; missing wall
  faces are.

  Opaque styles stay at 100.0% and MAE 0.00. The all-families scene improved with it, from 8.4% of
  pixels exact and MAE 31.84 to 16.6% and 20.13.

  One thing deliberately not taken: mbgl leaves its colour pass read-only, and measured here that
  is worse, so the colour pass writes depth too.

- **A background layer needed a vector tile to exist.** *Fixed.* The background was taken off
  whatever tiles a source happened to serve. mbgl does not do that and says so in as many words --
  `// renderTiles is always empty, we use tileCover instead` -- computing `util::tileCover` at the
  integer zoom for this layer alone.

  Taking it from served tiles was wrong twice. A style with no vector source has nothing
  renderable, so substitution records no coordinates and no background was drawn at all: the frame
  came out the clear colour, which is black. A raster-only basemap is exactly that style. And where
  a source *was* present but an ancestor stood in for a missing tile, the background went onto the
  ancestor's coordinate and covered four or sixteen times the ground it should -- which the comment
  beside it already said was wrong ("a background belongs to the coordinate on screen rather than
  to whatever tile happened to serve it") while the code keyed it to the serving tile anyway.

  A background-plus-imagery style against `mbgl-render`: **0.0% of pixels exact and MAE 188.52
  before -- a black frame -- and 48.6% with MAE 10.17 and zero gross pixels after.** The
  all-families scene 86.4% to 87.1%.

- **A building occluded the flat layers drawn after it.** *Fixed.* mbgl draws a flat layer under
  `depthModeForSublayer`, which is a depth *range*: the fragment's depth is remapped into a narrow
  band a few `depthEpsilon` from the near plane, one band per layer and sublayer. Flat layers
  resolve against each other by their band, and every one of them sits in front of anything drawn
  through the whole range -- which is what a fill-extrusion uses.

  The band is reproduced here as a nudge to the projection's `[14]`, because a consumer that binds
  a matrix has nowhere to put a depth range. A nudge translates the depth; it does not compress it.
  So a flat layer kept the depth of the ground it sits on, and once the extrusion started writing
  depth -- which it must, to resolve a building against itself -- a roof was nearer than the circle
  beside its foot and the test threw the circle away.

  Half the POI dots in the all-families scene: **946 pixels of them against the oracle's 1,811**,
  and 1,794 with the buildings taken out of the style. The layer measures 99.9% of pixels exact on
  its own, which is why this was invisible until the scene was measured whole -- the composite had
  1,649 gross pixels where its layers summed to about 819, and the difference was this.

  Flat layers are now not depth-tested at all, rather than tested against a band this cannot
  express. Painter order already puts them in sequence, and a flat layer in mbgl neither writes
  depth nor loses to anything that does. The scene went to **88.2% exact, MAE 0.51, 94 gross**.

- **A symbol layer with icons and no text drew nothing at all.** *Fixed.* `icon-image` with no
  `text-field` is an ordinary way to write a marker or a shield, and ours rendered a **black
  frame** -- no icons and no background -- where `mbgl-render` draws 97 icons.

  Five gates stood between such a layer and the screen, each one enough on its own, and every one
  of them was a test written as "does this symbol have text" where the question is "does this
  symbol have anything to draw":

  1. `Content::is_encodable` -- `fonts || !matches!(self, Self::Symbol(_))` held back every symbol
     layer until glyphs arrived. A layer whose symbols are all icons asks for none, so none were
     ever fetched. Nothing bound, no drawables were emitted, and the background went with them.
  2. `place_symbols` -- `let Some(fonts) = fonts else { return BTreeMap::new() }` prepared *no*
     symbol geometry when the frame held no glyphs.
  3. `write_geometry` -- `if buffers.vertices.is_empty() { continue }` dropped a symbol whose
     *text* shaped nothing, one step before `lay_out_icons` runs. `lay_out` deliberately leaves a
     placeholder entry carrying that symbol's anchor for exactly this case.
  4. `write_layer_state` -- `let Some(fonts) = frame.fonts else { return Ok(()) }` wrote the layer
     **no uniform blocks at all**, and the consumer skips a drawable whose layer has none. This is
     the one that hid the other four: with 1 to 3 opened, the batches arrived with their meshes
     built and their opacity written, and still nothing appeared.

  The glyph atlas size it returned on now falls back to `[1.0, 1.0]`. It is only ever divided into
  a glyph's texture coordinates; a layer with no glyphs has none to divide, and its icons take the
  sprite sheet's size instead. One rather than zero because the shader divides by it.

  The style goes from **0.0% of pixels exact and MAE 233.80 -- a black frame -- to 99.0% and 0.29**,
  drawing 23,580 icon pixels against the oracle's 25,214. No other scene moves.

  What the search cost, recorded because it was avoidable: two false conclusions, both from
  measurement rather than reasoning. `2>&1 >/dev/null | grep` discarded every probe, which read as
  "this code never runs"; and one reading of `write_icon_opacity` looked like a bug until changing
  it drew nothing at all, which is what showed `laid_out` there is already the icon's own entry.

- **What is left of the extrusion is two tiles drawing the same building.** *Understood, and the
  fix is not in the renderer.* Buildings sit at 95.7% of pixels exact, MAE 0.20 and 8 gross, and
  the residual is now fully accounted for.

  It is a double blend, and the arithmetic says so exactly: a roof blended once against the
  background gives 211, twice gives 209, and our frame carries both -- 211 as its most common roof
  colour and 209 across 24,840 pixels, with about 900 wall pixels the same way at a delta of 13.
  The blend itself is right: at opacity 1.0, 0.9 and 0.5 our roof reads 208, 211 and 221, matching
  the oracle at each.

  The doubled pixels are spread over 829 of 900 columns rather than banded at tile edges, and no
  drawable is issued twice -- the final frame has nine roof and nine wall drawables, one colour
  pass each. So the two copies are two *tiles* carrying the same building, which our extrusion no
  longer clips apart.

  **mbgl's read-only colour pass now measures identically** -- 95.7%, MAE 0.20, 8 gross, the same
  to the decimal as writing depth -- where it once scored MAE 6.51. That was the ordering and
  culling bugs, not the technique, and it is worth knowing it is no longer a cost. It is also not a
  cure: two tiles' copies of one building sit at the *same* depth, so no depth test separates them.
  Only a per-tile clip can, which is what mbgl uses and what cuts our walls, because the
  neighbouring tile does not carry the geometry to paint what the clip removes.

  So the remaining work is in what the tiles carry, not in how they are drawn: if a tile's
  extrusion geometry included its neighbours' overhang the way MVT's buffer intends, the clip
  would be lossless and the double blend would go with it.

- **A line label's anchors were walked along the unclipped line.** *Fixed.* mbgl runs
  `util::clipLines(feature.geometry, 0, 0, EXTENT, EXTENT)` and then `getAnchors` once per clipped
  run. This layout did not clip, and the comment saying so reasoned that cutting would "give each
  side its own ends" and put a name at every seam.

  That reasoning was half right and missed the part that matters. `get_anchors` does test each
  candidate against the tile, so clipping is not what decides which anchors survive -- but it is
  what decides where the walk *starts* and how far it has run by any point along the line. Where a
  road leaves the tile and comes back, mbgl gets two runs and two independent walks; uncut, one
  walk carries its spacing straight across the gap and every anchor after it lands somewhere else.

  Ported as a segment clip rather than a polyline clip, which is what mbgl's is: each segment cut
  against the box on its own, dropped when wholly outside, and a new run begun whenever a segment
  does not continue the last. Clipping the polyline properly would join runs mbgl keeps apart.

  **Where the clip goes matters as much as having one.** mbgl's order is merge, then clip, then
  anchors: `mergeLines(features)` closes the constructor and `clipLines` runs per feature in
  `finalizeSymbols`. Clipping at the point where a feature is recorded instead hands `merge_lines`
  the runs rather than the lines, and merging runs re-joins what the clip separated -- undoing it
  for exactly the roads it was meant to cut. Clipped at anchor time, with one walk per run, the
  order is mbgl's.

  Washington, the only scene here with `symbol-placement: line`:

  | | exact | MAE | gross |
  |---|---|---|---|
  | unclipped | 94.6% | 3.07 | 17,227 |
  | clipped at push, merged after | 93.0% | 2.95 | 8,229 |
  | **merged, then clipped at anchor time** | **97.2%** | **1.50** | 8,381 |

  No Berlin scene moves; none of them uses line placement.

  What the remaining gross is, in clusters the size of whole labels, and it is two things:

  - **A different road wins.** Where the oracle sets "11th Street Northwest" down a cross street,
    ours sets "Massachusetts Avenue Northwest" along the diagonal through the same ground. Both are
    plausible; they are competing for one piece of screen and the collision resolves it differently.
  - **The same label sits at a different anchor.** "Pennsylvania Avenue Northwest" is on its road in
    both, ours further along it than the oracle's.

  Anchor placement is not the cause, and both halves of it have now been read against mbgl's:

  - `get_anchors` matches `get_anchors.cpp` line for line -- the `continued_line` test, the
    spacing widening when a label is long relative to it, and both branches of the offset.
  - `resample` matches too -- `markedDistance` starting at `offset - spacing`, the walk per
    segment, the four-part test that the point is inside the tile and the label fits between the
    line's ends, the rounding of the anchor, the angle window, and the middle-anchor fallback for
    a line that placed nothing.

  Nor is it the merge/clip order, which is fixed above. And the evidence agrees: correcting that
  order moved the exact count 4.2 points and halved MAE while leaving the gross clusters *exactly
  where they were*, same positions and same sizes. Those two measure different things here -- the
  ordering fixed the many small disagreements, and the clusters are whole labels.

  Nor is it the merge, or the order symbols are offered in. Both have been read against mbgl's:

  - `merge_lines` mirrors `mergeLines` case for case -- both neighbours, left only, right only --
    and picks the same survivor each time, the earlier feature when merging rightwards and the
    later when merging leftwards. mbgl leaves a merged-away feature in place with empty geometry
    where this compacts it away; the relative order of the survivors is the same either way.
  - mbgl does not sort symbols before placing them here. `getSortedSymbols` runs only when
    `sortFeaturesByY`, which needs `symbol-z-order: viewport-y` *and* one of the allow-overlap or
    ignore-placement flags. The Washington style sets none, so mbgl places in creation order, as
    this does.

  So what is left is the collision machinery, and reading it against mbgl found one fix and one
  clear next step.

  **Fixed:** the backwards walk that starts the circle chain stopped an eighth of a label short of
  mbgl's `paddingStartDistance`. See the entry on it.

  **Next, and specific.** `CollisionIndex::placeLineFeature` skips a circle before it ever reaches
  the density test:

      if (!firstAndLastGlyph || (boxSignedDistanceFromAnchor < -firstTileDistance) ||
          (boxSignedDistanceFromAnchor > lastTileDistance)) {
          previousCirclePlaced = false;
          continue;
      }

  Two behaviours in there, and **one of them is already implemented** -- an earlier reading of this
  same guard put it in. `LineCircle::covered_by_label` is `distance_from_anchor.abs() <=
  label_length / 2`, which is `[-firstTileDistance, lastTileDistance]` at pitch zero, and
  `placement.rs` filters the chain by it before testing. A first pass through this entry said "we
  have neither", which was wrong: the field, its doc comment and the filter were all already there,
  and the claim was made from reading mbgl rather than from reading ours.

  What is genuinely absent is the other half: when the first and last glyph cannot be placed at
  all, mbgl marks every circle unused and the label does not go down. There is no such rejection
  here. How much that is worth is unclear and probably small, because `resample` already refuses an
  anchor unless `marked - half_label >= 0` and `marked + half_label <= length`, so a label that
  does not fit its line never gets an anchor to begin with; mbgl's own `resample` makes the same
  test. At pitch zero the projection that could still fail is close to the identity.

  So the contention difference is *not* accounted for, and this guard is no longer the lead.

  What has been read against mbgl and matches, so that none of it is searched again: the clip, its
  ordering against the merge, `merge_lines` itself, `get_anchors`, `resample`, the order symbols are
  offered in, the circle chain's step, count, padding factor and first-box offset, and the density
  test that thins the chain.

- **A symbol's anchor lands a fraction of a pixel from mbgl's.** *Open, and measured to the
  decimal.* It is what is left of both symbol layers: 811 gross pixels of 630,000 on `poi-labels`
  and 1,351 on the icon-only style, and in each case the split is the same -- about half
  icon-against-icon, which is edge blending, and about half icon-against-background, which is our
  icon covering a pixel mbgl's does not.

  Read off one icon's green channel on the icon-only style, where nothing else is drawn:

  - **Size is exactly right.** Our coverage sums to 17.00 px across and 16.00 down -- 0.407 of a
    pixel at the left edge, sixteen whole ones, 0.593 at the right -- against the sprite's declared
    17x16.
  - **Position is 0.407 px left and 0.174 px down** of mbgl's, whose icon is pixel-aligned: 230
    straight to 144 with no intermediate value on any side, spanning exactly 17 columns.
  - So mbgl's anchor sits at a half pixel and ours 0.4 short of it, and the quad around it is the
    same in both: `shapeIcon` gives +/-8.5 for a 17-wide sprite, plus the one-pixel border, in
    mbgl and here alike.

  Ruled out. Not the quad border -- removing it takes `poi-labels` from 811 gross to 3,363. Not a
  rounding rule in mbgl -- neither `placement.cpp` nor `symbol_projection.cpp` rounds a projected
  anchor. Not a constant bias -- over 54 icons the offset averages +0.02 and -0.01 with a +/-0.45
  spread, so it varies per symbol. Not the projection in general -- `place-labels` is 100.0% of
  pixels exact with zero gross, so anchors on that layer are bit-identical. And not the scaling to
  `EXTENT`: `rings_scaled` rounds, but from a 4096 source to 8192 the factor is exactly two.

  **The tile-unit anchor is eliminated too, and it was the last lead here.** mbgl builds a point
  symbol's anchor as `static_cast<float>(point.x)` over a `GeometryCoordinate`, which is
  `Point<int16_t>` -- an integer in `util::EXTENT` units, since `getGeometries` scales a 4096 tile
  by an integer factor of two. Ours are integers as well: across the berlin fixture at z14, all
  140 `pois` point anchors and all 6 `places` ones have a zero fractional part. The two sides agree
  on the anchor exactly, so the difference is downstream of it, and the question the entry ended on
  -- why `pois` differs from `place` when both are point features -- has no answer at the anchor
  because they do not differ there.

  What that leaves is the step between an agreed anchor and a drawn quad, and the shape of the
  error says which kind: a spread of about half a pixel either way with a mean near zero, varying
  per symbol, is independent rounding rather than a bias. mbgl's icon lands on exact pixel columns
  and ours does not.

  It is worth saying plainly that this is 0.2% of a frame at sub-pixel scale, and every other layer
  is at or below 8 gross pixels; it is the smallest thing on the list rather than the next most
  valuable, and it is not what to pick up next.

- **A raster tile drew a different tile's picture.** *Fixed, in two places.* On the raster fixture
  every tile carries a parity tint, `blue = 190 - ((x + y) % 2) * 40`, so which tile landed where
  is readable off the picture -- and ours did not alternate across a row where mbgl's did.

  Everything about the placement was right, which is what made it look like a covering-zoom
  problem: the tile borders fall on identical columns in both, 114/370/626/882 at a 256-pixel
  pitch, and logging the fixture shows a contiguous z16 block, 35206..35209 by 21491..21494, which
  is what a 900x700 viewport at z15 wants of a 256-pixel source. The right tiles were fetched and
  the quads were in the right places; the wrong texture went on each quad.

  **The uniform blocks were packed in arrival order and read in slot order.** `ubo_index` is
  assigned by walking the *resolved* order -- pass, depth slot, sub-layer, sort key, then tile --
  while `by_layer` collects bindings as the cover is walked. For a vector layer the two coincide,
  because the cover is walked in the order the sort puts it, so nothing ever noticed. A raster
  source is looked up at its own zoom by a second walk with its own traversal, and there they
  diverge: the drawable in slot 1 was tile (35206, 21492) while the matrix in slot 1 belonged to
  (35207, 21491). Fixed by packing from the resolved order, so there is one definition of the slot
  numbering instead of two that have to agree.

  **And the texture id was the tile's position in the frame's bucket list.** `RASTER_TEXTURE_BASE +
  index` is stable only while that list is, and it grows as tiles arrive -- while the textures and
  the drawables naming them live across frames. A later frame handed the same id to a different
  tile, the consumer's map took the new picture at that key, and every drawable still holding the
  id began sampling it. Two tiles reported the same texture, and which two depended on the order
  the network answered in: the same scene scored anywhere from 64.5% to 73.5% of pixels exact
  between runs. Now packed from the tile's own zoom, column, row and world copy, injective by
  construction.

  A background-plus-imagery style against `mbgl-render`: **0.0% of pixels exact and MAE 188.52 at
  the start of the day -- a black frame -- and 88.6% with MAE 0.40 and zero gross pixels now, the
  same to the decimal on every run.** No other scene moved, which is the point: the ordering was
  already right everywhere the cover walk and the sort agreed.

- **A patterned fill was written with the plain fill's drawable layout.** *Fixed.* The two share
  the union's stride, so nothing about the buffer's length says which is meant, and
  `FillPatternDrawableUBO` puts the tile's pixel origin and ratio exactly where `FillDrawableUBO`
  puts its zoom-mix factors. The producer only ever wrote the plain one, so `tile_ratio` arrived as
  zero.

  A zero ratio takes the world position out of `patternPos` -- `(unitsToPixels * pos + offset) /
  size` with both terms zero -- so `mod(patternPos, 1.0)` was `(0, 0)` for *every* fragment and
  each one sampled the same point of the sprite. That point is the pattern rectangle's own corner,
  which in the atlas is the padding around it, at alpha 36 of 255. So the layer drew at a seventh
  of its strength: a wash the shape of the parks rather than a texture in them.

  The layer against `mbgl-render`: **44.1% of pixels exact and MAE 11.31 before, 92.9% and 0.34
  after**. The all-families scene went from 47.2% and MAE 7.39 to **86.4% and 1.45**.

  How it was found, in the order that mattered: the layer had *zero* gross pixels, so the aggregate
  numbers were the only signal it was wrong at all. Forcing the material to a solid colour showed
  the geometry, coverage and blending were all correct, which left the sample. Having the shader
  output its own sampled alpha gave 36/255 -- the exact factor the colours were short by -- and
  having it output the tiling coordinate gave `(0, 0)`, which is only possible if `tile_ratio` is
  zero.

  `PixelOrigin` is now shared with the extrusion, which computes the same three values for the
  same reason and had the only correct copy of the arithmetic.

- **A symbol layer's two halves were laid out as one.** *Fixed.* Two properties, both unread, and
  together they are how a style puts a name under the marker it names.

  `SymbolDrawableEntry::for_tile` hardcoded `is_text: true` and took one size for both drawables.
  The shader computes `fontScale = is_text ? size / 24 : size`, because `text-size` names a size in
  pixels while `icon-size` multiplies a sprite that already has one -- so every icon was scaled by
  the layer's text size over `ONE_EM`. At `text-size` 11 that is 11/24, and a 17x16 marker drew at
  3x4 where the oracle draws it at 17x16.

  And `text-offset` and `text-anchor` were never read at all. `quads::Options` has carried a
  `text_offset` field the whole time with nothing but a unit test setting it, and `anchor_of` was
  called for `icon-anchor` and not for `text-anchor`. Without them a POI label sat on top of its
  own icon rather than below it.

  The layer against `mbgl-render`: **88.0% of pixels exact and 42,979 gross before, 98.2% and 6,150
  after**. The all-families scene went from 39.4% and MAE 13.31 to **47.2% and 7.39**, with gross
  pixels down from 46,660 to 8,472.

  Found by measuring every layer of that style in isolation, which is worth repeating when a scene
  is wrong in several places at once: water, roads, buildings, circles and place labels were all
  already within a pixel or two of the oracle, and the two that were not were the raster and this.

- **A raster layer masked out the vector layers beneath it.** *Fixed.* It was described as the
  imagery painting over what is under it, and hunted in painter order for a long time. Painter
  order was never wrong: the raster is issued at order 9 and the water at 29, under it as it
  should be. The water's drawables were issued too -- nine of them, with the same matrices and
  index counts as when the layer draws correctly. They were being thrown away by the stencil.

  Two things caused it, and both are about a raster source being looked up at *its* zoom. A 256px
  raster needs z16 where the vector layers take z15, so a style with one puts two zooms of tiles
  in the same frame.

  **The raster asked for a stencil.** `RenderRasterLayer` never calls `setEnableStencil` and takes
  the default of false; ours emitted `tiled_flags()`. The clip would be a no-op anyway -- a raster
  drawable is a quad covering exactly its own tile -- but asking for it put those z16 tiles in the
  mask buffer, where they overwrote the z15 masks over the same screen area.

  **And every layer's clip set was built from the whole frame's tile list.** mbgl sets the stencil
  per layer group, `tileLayerGroup->setStencilTiles(renderTiles)`, and the difference only shows
  when two layers draw at different zooms. Ours masked the water with the raster's tiles as well
  as its own.

  Either one alone leaves it broken; the per-layer clip set is what carries the fix. The
  all-families scene went from 16.6% of pixels exact and MAE 20.13 to **39.4% and 13.31**, and the
  river from zero pixels of `#9ec6dd` to 36,328 against the oracle's 35,324.

  What identified it: `TSF_NO_STENCIL=1` restored the water exactly, 75,340 pixels, which is what
  the layer draws when the raster is not in the style at all.

  Also changed, and *not* measurably load-bearing here: the extra-zoom walk now takes only the
  raster buckets off the tiles it finds. On this fixture the store serves vector data only to z15,
  so those z16 tiles carry nothing else and the filter is a no-op. It is kept because the walk
  exists for raster and a source serving vector at the raster's zoom would otherwise have its
  layers drawn twice, at two zooms.

  Still open in that scene: the imagery is tinted more strongly than the oracle's and its tiles
  look larger, which points at the covering zoom; the pattern layer draws no green; and the icons
  are far smaller than mbgl's.

- **Symbol corner cases left standing when this thread was set down.** *Open, and none of them
  blocking.* Recorded together so they are not rediscovered one at a time:

  - An exactly vertical line places its label upside down. `place_upright` decides by comparing
    the first and last glyph's `x`, and on a vertical line that difference is noise. mbgl compares
    the same way for a horizontal-only writing mode, so it is degenerate there too; real roads are
    rarely exact. Seen on a synthetic line, not on a map.
  - Point labels drop the occasional glyph mid-word. Filed under this once before and turned out
    to be roads painting over the text, which is fixed; whatever is left is smaller and has not
    been measured.
  - `continued_line` was dead code, and the precondition it needed now exists. It compares
    `line[0]` against 0 and EXTENT exactly, which only holds for geometry clipped to the tile, and
    this layout did not clip -- 0 of 898 features on a real tile set it. `clipLines` is now ported
    and a cut run starts exactly on a boundary, so the flag can fire; whether it does, and whether
    it changes anything, has not been measured.

- **Washington drew about a fifth more label than the oracle.** *Fixed, by the collision box.* The
  lead recorded here was that the two scenes disagreed -- Berlin near the oracle, Washington 20%
  over -- so whatever was left was something Washington's data had more of. It was not: it was
  every label offset from its anchor reserving the ground at the anchor instead of the ground it
  covers, which Washington's style has more of because more of its labels are offset. Seeding the
  shaping with `text-offset` closed it.

  Re-measured on the same scene and camera: **ours 10,162 label pixels against the oracle's
  10,397**, two per cent *under* where it was twenty per cent over.

  What is left there is placement rather than quantity. Washington sits at 94.6% of pixels exact
  with 17,227 gross, and those pixels are symmetric -- roughly as many where ours has a label and
  mbgl has background as the other way about -- with the two frames carrying nearly the same amount
  of text. So the labels are the right size and about the right number, in different places. Its
  style is the only one here with `symbol-placement: line`, which Berlin's scenes never exercise,
  and that is where to look.

- **The style's placement properties never reached placement.** *Fixed.* `FrameOptions` was built
  with `Rules::default()` and the default paddings whatever the style said, so `text-allow-overlap`,
  `icon-allow-overlap`, `text-optional`, `icon-optional`, `text-ignore-placement`,
  `icon-ignore-placement`, `text-padding` and `icon-padding` were all ignored -- eight layout
  properties read by nothing. A layer asking to overlap competed anyway.

  Found while testing whether a collision hypothesis held: setting `text-allow-overlap` in the
  style changed nothing, which was read as evidence about collision and was really this. The
  experiment that mattered had to force the flag in code instead. Worth recording as a
  measurement error and not only as a bug: an input that silently does nothing turns any
  experiment using it into a false negative.

  One default moved with it. `icon-padding` read as one pixel, under a test asserting that and a
  comment saying the spec's text and icon defaults differ. They do not: mbgl's `IconPadding` and
  `TextPadding` both return 2, and so does the spec. The test now says so.

  Verified through the wire: a road with `text-allow-overlap: true` draws four labels where it
  drew two. Berlin and Washington are unchanged at 2,816 and 1,827, which is what should happen --
  neither style sets any of the eight.

- **A collision box was built from a shaped extent at a scale of one.** *Fixed, and it is most of
  the label gap.* Shaping works at one em -- 24 units to the em, whatever the label is finally set
  at -- so `LaidOut::extent` is in ems, and the box reserved against every other label on the
  screen was built from it unscaled. At the `text-size` of 12 most styles ask for, every label
  claimed twice its width and twice its height: four times its area. mbgl carries the same factor
  into `CollisionFeature` as `textBoxScale`, and the field's own doc comment here said "in
  pixels", which it was not.

  The third of these now -- `symbol-spacing` in pixels, `get_anchors` at a `box_scale` of one, and
  this -- and they are one mistake made three times: a number in em space used where the space is
  something else. Anywhere a shaped extent or a glyph distance crosses out of layout, the scale
  has to cross with it.

  Berlin z14 goes 1,724 to 2,816 pixels of text against the oracle's 3,005; Washington 1,351 to
  1,827 against 1,931. Ninety-four per cent of the oracle on both, from about sixty. Reproducible.

  Anchors were measured first and are not the problem: on a real tile, 740 of 898 line features
  are shorter than 62 pixels and cannot hold a name at all, and `get_anchors` agrees with mbgl's
  line for line -- the same acceptance test, the same spacing adjustment, the same offset. Worth
  recording because the census is cheap and it is the obvious thing to suspect.

- **A frame's labels compete in one grid now.** *Done.* Placement is a decision about the frame --
  a road name and a shop name want the same screen whatever layer or tile each came from -- and it
  was happening inside the encode walk, a grid per bucket, so a layer could only ever compete
  against itself.

  It could not simply share a grid there: that walk visits one bucket at a time and encodes as it
  goes, and a fade takes its direction from the previous frame's decision, so the first bucket's
  opacity depends on the last bucket's placement. So placement is a pass of its own now,
  `place_symbols`, which shapes and places every symbol bucket, settles the fades once, and leaves
  the written buffers for the encode walk to pick up. Shaping happens once, not twice.

  The earlier note here said the pass had to run in *reverse* order. That was wrong, and the
  render said so: reversed, the frame fell to 509 pixels of text from 2,050, with road names
  keeping everything and place names losing almost all of it -- which is the bottom-up failure
  that note was describing. `DrawOrder::resolve` sorts on `depth_slot`, which runs opposite the
  style index, so it is *already* top-layer-first, which is the order mbgl places in. Taken
  forwards it gives 1,724, reproducible.

  Lower than the 2,050 the per-bucket grids gave, and that is the point: those 2,050 included a
  place name printed over a road name, because neither could see the other. The oracle draws 3,005
  on the same frame, so there is still something to find -- mbgl fits more labels into the same
  screen, and tighter collision boxes and its retry at other anchors are the two places to look.

  One thing changed on the way and is worth keeping separate: the line walk now runs *before* the
  label is offered any space. A label whose road runs out before its name does is not drawn, and a
  label that is not drawn must not hold a run of collision circles against the ones that are.
  mbgl decides the two together. It was tried as a fix for the 509 and fixed nothing, which is how
  it is recorded here -- it is right on its own terms, not because it recovered anything.

- **Labels pitched with the map are a second matrix arrangement, not yet written.** Identity plane
  matrix, the tile's projection in the coord matrix, offsets in tile units rather than pixels.
  Sixteen batches at z16, counted and skipped rather than drawn in the wrong space.
- **All nine shader families draw.** `missing_batches` is zero at z14 and z16 and no family is
  reported missing. The last three landed together: raster, fill-pattern and fill-outline-pattern,
  all of which were waiting on the texture path.

  Raster needed two samplers rather than one -- a source crossfades from the parent it is standing
  on while the finer tile loads -- and its own drawable stride, since its block is a bare matrix at
  64 bytes where a fill's is 96. At z5, where liberty's `natural_earth` layer actually draws, the
  coastlines, water, borders and landcover match the oracle.

  The patterned fills anchor their sprite to the *world* rather than the tile, which is what stops
  a pattern sliding when tiles are replaced or seaming where two meet. That is what
  `pixel_coord_upper` and `pixel_coord_lower` carry: the tile's origin in world pixels split across
  two floats, because one float32 runs out of mantissa long before a zoom-22 world runs out of
  pixels. mbgl's triple `mod` folds the high half against the pattern period *before* multiplying
  by 256 twice, so the product stays exact; it is transcribed rather than simplified.

  At z14 the base map -- fills, lines, patterns, extrusions -- is close to the oracle's. The labels
  are not, and the render shows why in the plainest possible way: they come out as scattered
  fragments, "e", "er", "B", "pla", because only about ninety of the six hundred codepoints the
  frame needs were ever fetched. The letters that exist are drawn and the rest are absent. That is
  the glyph-fetch defect above, seen directly rather than inferred.

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

- **The collision grid was the viewport exactly, and mbgl's is bigger than that.** *Fixed, but
  worth much less than expected.* `CollisionIndex`'s constructor builds

      collisionGrid(width + 2 * viewportPadding, height + 2 * viewportPadding, 25)

  with `viewportPaddingDefault = 100`, doubled when the camera is pitched and 1024 for a single
  static tile, and adds that padding to every projected point. Ours was `GridIndex::new(width,
  height, 32)` with no offset, so it differed in three ways at once: no margin, coordinates not
  offset into one, and a cell size of 32 against mbgl's 25.

  The margin is the one that can change an answer. `GridIndex` clamps an out-of-range coordinate
  onto the nearest edge cell rather than dropping it, so without a margin every label hanging off
  the screen collapsed onto the boundary cells and collided there with labels it was nowhere near
  -- and Washington has clusters at x 0-77, x 814-866 and y 617-660, right against those edges.
  Cell size is a broad phase with the exact tests behind it, so it decides how much work a query
  does, not what it answers.

  `project_with` has exactly one call site, the placement pass in `frame.rs`, so the offset could
  go straight into it. The first attempt offset the points and resized only the grid in
  `symbols.rs::frame`, missing the one `frame.rs` builds for the production path; that fired
  everything 100px into an unpadded grid and cost 2,600 gross pixels on Washington and 6,300 on
  the all-families scene. With both grids padded:

  | scene | before | after |
  | --- | --- | --- |
  | Washington | 97.3% / 1.43 / 8,004 | 97.4% / 1.43 / 8,001 |
  | all families | 88.2% / 0.51 / 94 | 88.2% / 0.51 / 90 |
  | poi-labels | 99.4% / 0.17 / 811 | 99.4% / 0.17 / 806 |
  | place-labels | 100% / 0 / 0 | unchanged |

  Every metric moved the right way and none of them moved much. The reading that predicted this
  would matter was right about the mechanism and wrong about the scale: the edge clusters are
  dense enough to be deciding against each other, not against the labels beyond the frame. It goes
  in because it is what mbgl does and because the spurious edge collisions are real, not because
  it closed the contention gap. That gap is still open.

- **The systematic sub-pixel symbol offset is gone.** *Closed, by the fixes either side of it.*
  This was recorded as a constant bias of 0.407px left and 0.174px down, worth about 811 gross
  pixels on poi-labels and 1,500 on the icon-only style. Resampling ours over a grid of shifts and
  taking the minimum gross now puts the optimum at dx -0.10, dy +0.10 on poi-labels -- recovering
  124 of 806 -- and at exactly zero on Washington. The text-offset/anchor fix and the clipLines
  reordering account for it between them. What is left in those scenes is per-label, not a shift.

- **Placement order already matches mbgl, in both axes.** *No change; recorded so it is not
  "fixed" again.* Washington's largest remaining differences are whole line labels: at x 354-364
  we print "14th Street Northwest" where the oracle prints "13th Street Northwest" at x 463-473,
  over an identical y span and identical road geometry. Both streets are labelled in both frames;
  the two renderers just give the second label to different streets. That is placement order, so
  it was worth checking properly, and it was wrong twice on the way:

  - `Placement::placeLayers` walks `crbegin` to `crend` over a list in render order, so the
    topmost symbol layer places first. Reading that and reversing our loop over `order` was a
    regression -- Washington 8,001 gross to 8,754, all-families 90 to 28,945 -- because it also
    reversed the tile axis, which mbgl does not: `placeLayer` walks a layer's tiles forwards.
  - Reversing only the layer axis, grouping `order` by `layer_index` first, was still a
    regression: Washington 15,958, all-families 2,409, poi-labels unchanged at 806 because it has
    a single symbol layer. The reason is that `order` is *already* top layer first --
    `Placed::sort_key` orders by `depth_slot`, and `depth_slot` is `layer_count - 1 -
    layer_index`. Walking it forwards descends the style, which is what mbgl does.
  - Tile order was the other suspicion, from a recollection that `OverscaledTileID` sorts y before
    x. It does not: `std::tie(overscaledZ, wrap, canonical)` over `std::tie(z, x, y)`, which is
    `sort_key`'s `(z, x, y)` exactly.

  Both axes already agreed, and the loop now carries a comment saying so. The label contention is
  something else.

- **A symbol instance carried its feature's whole line, not the run its anchor was found on.**
  *Fixed. Latent here, but wrong.* `getAnchors` walks one clipped run and `Anchor::segment` is an
  index into that run. mbgl keeps the two together -- `createSymbolInstanceSharedData(std::move(
  line))` is handed the very run, and every instance anchored on it shares that line. Here the
  anchors were collected out of `clip_line(...).flat_map(get_anchors)`, the runs dropped on the
  floor, and `frame_labels` read the line back off `pending`, which holds the *unclipped*
  geometry. Wherever the clip actually cut, the segment index then named a different pair of
  vertices and the glyph walk started from the wrong one.

  `LaidOut` now carries the run as an `Arc`, which is mbgl's shared data by another name -- a run
  holds every anchor along it, and a road is a long run. The `line-center` branch keeps the whole
  line, because mbgl's does not clip there either.

  No measured change: Washington, the all-families scene, poi-labels and place-labels are all
  exactly where they were. The reason is the next entry -- our `clip_line` almost never splits,
  because the geometry reaching it has already been flattened, so the run and the line are the
  same array. This is a fix for a bug that the *next* fix would otherwise have switched on.

- **`merge_lines` merges every ring; mbgl merges only `geometry[0]`.** *Found, not yet fixed.*
  Both renderers were instrumented to print, per line-placed feature, the ring count, point count
  and bounds before the clip, and the spacing, box scale, shaped extents and resulting anchors
  after it. On Washington:

  | | records | rings | points |
  | --- | --- | --- | --- |
  | tessella | 348 | 348 | 2,164 |
  | mbgl | 358 | 490 | 2,222 |

  The same points in fewer, longer lines. `getAnchors` itself is exact -- for a line both agree
  on, every input matches to the digit (`spacing=4000.000 boxScale=8.00000 glyphSize=24.0
  left=-118.500 right=118.500`) and so does every anchor, `(3117.0,6192.0,seg2)` on both sides.
  What differs is the geometry handed to it.

  `mergeLines` walks `features` and touches `geometry[0]` and nothing else: `getKey` is taken from
  `geometry[0].front()` and `.back()`, and `mergeFromRight`/`mergeFromLeft` splice into
  `geometry[0]`. A feature's remaining rings are never keyed, never merged, and stay attached to
  it. Here every ring becomes its own `Pending`, so every ring is keyed and every ring can merge
  -- and two rings that mbgl keeps apart get joined into one line whose anchors then fall
  somewhere else entirely. That is what puts "Pennsylvania Avenue Northwest" about ninety pixels
  earlier along its road than the oracle does, on identical road geometry.

  *Fixed.* `Anchoring::Line` holds a feature's lines rather than one of them, `merge_lines` keys
  and splices only the first, and the collection is clipped in one pass as `clipLines` does --
  its `clippedLines` is shared across rings, so the run-continuation test spans ring boundaries.
  `line-center` stayed per ring, which is mbgl's own arm: it loops the geometry and takes a centre
  on each line longer than a point.

  The instrumented counts, after:

  | | records | rings | points | clipped runs |
  | --- | --- | --- | --- | --- |
  | tessella, before | 348 | 348 | 2,164 | -- |
  | tessella, after | 294 | 426 | **2,222** | **390** |
  | mbgl | 358 | 490 | **2,222** | **390** |

  Points and clipped runs are now exact, and the runs are what `getAnchors` is handed. The
  remaining 64 records and 64 rings are husks: `mergeLines` clears a spliced feature's
  `geometry[0]` and leaves the feature in the vector, so mbgl counts 64 features carrying one
  empty ring and no points. The retain here drops them instead. Zero points, zero runs, nothing
  observable -- and keeping them would mean carrying a pending with no geometry through every
  stage downstream.

  Washington goes **8,001 gross pixels to 4,936**, 97.4% of pixels exact to 98.3%, MAE 1.43 to
  0.89. The all-families scene, poi-labels and place-labels are unchanged, which is what should
  happen: they are point-placed or single-ring, so there is nothing for this to regroup.

  It also switched on the fix above it. `clip_line` now sees the geometry mbgl sees, so it splits
  where mbgl splits -- 390 runs against 348 lines -- and the run an anchor was found on is no
  longer the same array as the feature's line.

  One test moved with this. `a_roads_segments_are_joined_before_it_is_labelled` read the street
  fixture as 1,773 road features; it is 28 features carrying 1,699 line strings, one of them a
  single feature with 562 parts, and the 1,773 was a count of rings. Its second-pass assertion --
  that running `merge_lines` again joins more -- was an artefact of the same misreading: 1,699
  ring-pendings contended for one index slot per (text, endpoint), and 28 features do not. The
  assertion is gone and the reasoning is recorded in its place.

- **Everything before the collision test now matches the oracle exactly.** Three fixes, none of
  which moved a pixel on their own, and one conclusion that is worth more than they are.

  With both renderers printing per-line-placed-feature records, and mbgl's writes made atomic --
  its tiles parse on worker threads, and a record split across several `fprintf` calls interleaves
  with another thread's, which invents differences that are not there -- 389 of 390 anchor records
  agreed. The one that did not was a hairpin, a divided road running up one side and back down the
  other, where we placed a label mbgl refuses.

  - **`check_max_angle` exempted the first segment.** *Fixed, and this was the anchor.* mbgl opens
    with `if (!anchor.segment) return true;` and `Anchor::segment` is a `std::optional<size_t>`, so
    that asks whether the anchor names a segment at all -- the point-placed case its comment calls
    out. Reading it as `segment == 0` waved every label anchored on a line's first segment past the
    check. Not a rare position: `getAnchors` offsets the first anchor by half a label plus two
    glyphs, which on any line whose first segment is longer than that lands on segment zero.

  - **`distance` used `hypot` where mbgl uses `sqrt(dx * dx + dy * dy)` over `int16_t` fields.**
    *Fixed.* `dx` and `dy` promote to `int`, so mbgl's sum is exact integer arithmetic and only the
    root is floating point. Ours was `f32` throughout: a tile coordinate reaches 8192, `dx * dx`
    wants 27 bits of mantissa and `f32` has 24, so the squares round before they are added. It did
    not turn out to be what rejected the hairpin, and it is still the wrong arithmetic.

  - **Placement walked each layer's tiles column-major.** *Fixed.* `RenderSymbolLayer::prepare`
    takes `getRenderTilesSortedByYPosition()` and no other layer does; everything else keeps the
    plain render tiles, which come out of a map keyed by `OverscaledTileID` and run x-major, which
    is what `sort_key` already produces. So the draw order stays and only the placement walk is
    resorted. mbgl's comparator is `tie(b.z, par.y, par.x) < tie(a.z, pbr.y, pbr.x)`, with `par`
    from *a* and `pbr` from *b*; mixing the sides looks like a slip, but at one zoom the `z` terms
    are equal and it reduces to ascending `(y, x)` -- row by row, left to right.

  The placement sequence -- every symbol offered to the grid, in order, with its tile and anchor --
  is now **identical to mbgl's, all 166 of them, zero diff lines**. So are the anchors.

  And Washington did not move: 98.3% exact, MAE 0.89, 4,936 gross, before and after. That is the
  useful part. What we place, and the order we place it in, is no longer a candidate explanation
  for anything. The whole remaining gap is in the collision test itself -- the boxes and circles
  put into the grid, their padding, and the projection that places them there. Two clusters show
  it plainly: around (556-656, 442-559) mbgl keeps "F Street Northwest" and drops the two vertical
  street names that cross it, while we keep both verticals and drop F Street; around (478-646,
  57-175) it is the other way about, and we keep "Massachusetts Avenue Northwest" where mbgl keeps
  "11th Street Northwest". Same anchors, same order, opposite outcomes.

- **The circle thinning ran over circles the label does not reach.** *Fixed.* With placement order
  and anchors matching exactly, both renderers were made to print, per symbol offered to the grid,
  the circles it puts up and the verdict it gets. 166 records each, in the same order, on the same
  anchors -- and 58 of the verdicts disagreed, the first at record six.

  mbgl's `projectedBoxes` is sized to the whole run and only the entries it actually tests are
  filled, so the log shows which circles a run used: real circles alternating with default-
  constructed placeholders. Reading ours against that put the divergence in `placeLineFeature`'s
  loop order. mbgl tests the reach *first* --

      if (!firstAndLastGlyph || (boxSignedDistanceFromAnchor < -firstTileDistance) ||
          (boxSignedDistanceFromAnchor > lastTileDistance)) {
          previousCirclePlaced = false;
          continue;
      }

  -- and only then thins what is left, comparing each surviving circle against the last one kept.
  Here the thinning ran over the whole run and `covered_by_label` was applied afterwards as a
  filter, so circles the label never covers still took part in the density decisions, and the kept
  set came out different. The "keep the last one however tight it is" guard differed too: mbgl asks
  whether the *next* circle is one it would test (`atLeastOneMoreCircle` and then the same reach
  test), where this asked only whether the array continues.

  Washington 4,936 gross pixels to 4,089, 98.3% exact to 98.5%, MAE 0.89 to 0.74. The other scenes
  are unchanged. Verdicts that disagree: 58 to 53.

- **The reach itself is still an approximation.** *Next, and precisely located.* On record five of
  Washington the kept sets now differ only at the ends: mbgl keeps circles 4, 6, 8, ... and this
  keeps 3, 4, 6, 8, ..., 15. Circle 3 is one mbgl's reach test rejects.

  `covered_by_label` here is `|signedDistanceFromAnchor| <= label_length / 2` -- symmetric, and
  taken from the label's nominal length. mbgl's bounds are `firstTileDistance` and
  `lastTileDistance`, which come from `placeFirstAndLastGlyph` walking the line in the label plane
  to find where the first and last glyph actually land, then `approximateTileDistance` converting
  each back to tile units through the incidence stretch. They are not symmetric and they are not
  the nominal length. `placeLineFeature` also refuses the label outright when that walk fails
  (`!firstAndLastGlyph`), and requires `inGrid` -- at least one tested circle inside the padded
  grid -- neither of which is expressed here.

  The walk already exists in a different form: `write_line_positions` follows the same line to
  decide whether the name fits, and that is where the first and last glyph's distances would come
  from.

  *Fixed, and it closes Washington.* Logging both sides' per-circle distances settled what the
  quantity is before anything was changed. On the same label they agree exactly once the units are
  reconciled -- mbgl stores tile units and this works in label-plane pixels, and at
  `pixelsToTileUnits = 1/16` its `-1499.520 -1264.000 -1028.480 -792.960 ...` is this frontend's
  `-93.720 -79.000 -64.280 -49.560 ...` to the last digit. Only the bound differed: mbgl kept
  `-47.25 ..= 48.75` where this kept `|d| <= 52.75`.

  Two things in that. The bound is **not** the box: it is the outermost *glyphs*, so it stops
  short of the collision box, which carries the label's padding as well. And it is **asymmetric**
  -- 47.25 against 48.75, a difference of exactly one em -- because the shaping is. Comparing
  against half the box kept one extra circle at each end, which is one extra stretch of road
  reserved per label, and that is enough to change who wins a junction.

  So `FrameLabel` carries `glyph_reach`, the first and last entries of the same
  `glyph_offsets` buffer `write_line_positions` already walks -- mbgl's `glyphOffsets.front()`
  and `.back()` -- and `line_circles` compares against `-first ..= last` scaled by `font_scale`,
  falling back to half the length when there are no glyphs to offer, which is the icon-only case
  mbgl never reaches with.

  | scene | before | after |
  | --- | --- | --- |
  | Washington | 98.5% / 0.74 / 4,089 | **99.7% / 0.01 / 0** |
  | all families | 88.2% / 0.51 / 90 | unchanged |
  | poi-labels | 99.4% / 0.17 / 806 | unchanged |
  | place-labels | 100% / 0 / 0 | unchanged |

  Washington has no differing pixel left above the gross threshold, on the scene that opened this
  session at 17,227. The two Berlin scenes are point-placed, so nothing here touches them.

  Still not expressed, and no longer measurable on these scenes: `placeLineFeature` refuses a
  label outright when the first-and-last-glyph walk fails, and requires `inGrid` -- at least one
  tested circle inside the padded grid. The first is covered in a different place here
  (`write_line_positions` hides a label whose road runs out, and `frame.rs` drops it before it is
  offered); the second has no counterpart.

- **An unscaled icon was sampled with interpolation, and mbgl samples it nearest.** *Fixed, and it
  closes the two Berlin symbol scenes.* The poi-labels difference was 47 small clusters, the
  largest an 18x18 box around a sprite. Cropped, the sprite is there in both and the same colour,
  but ours has soft edges where the oracle's are crisp: mbgl drew a 17x16 block with 268 of its
  272 pixels at full strength, and this drew 18x17 with about 66 partial pixels round the border.

  Weighing the two by ink gives identical mass -- 70,863 either way -- so it is the same picture at
  the same size, and the centroids differ by a fraction of a pixel. Across eight sprites in the
  frame those fractions are all different, between -0.30 and +0.33, which is what rules placement
  out: a misplacement would be constant, and a *sampling* difference is not. mbgl's edges land on
  pixel boundaries because it is not interpolating at all.

  `DrawableAtlasesTweaker` gives the glyph atlas linear filtering always and the icon atlas
  `TextureFilterType::Nearest` unless `linearFilterForIcons`, which `RenderSymbolLayer` computes as
  `iconScaled`: an `icon-size` other than a constant one, or `iconsNeedLinear` -- a data-driven or
  zoom-varying `icon-size`, a non-zero `icon-rotate`, or a sprite whose pixel ratio differs from
  the map's. An icon drawn at its own size has its texels one to a pixel, and interpolating between
  them only smears the edges across two.

  The sampler is a property of the *use* rather than of the texture -- the same sprite sheet is
  sampled linearly by a pattern -- so it travels with the binding. `TextureRef` had a `_pad` word
  documented as "must be zero"; it now carries a `TextureFilter`, and zero is `Linear`, so a
  producer that never sets it and a consumer that never reads it both keep the behaviour they had.
  `SymbolLayout::icons_need_linear` answers the three conditions that live in the style, reading a
  literal as constant and an expression as not -- which is mbgl's `constantOr` and
  `isDataDriven() || !isZoomConstant()` split. The fourth needs the sheet, so the emit site
  applies it against the bucket's own sprites.

  | scene | before | after |
  | --- | --- | --- |
  | poi-labels | 99.4% / 0.17 / 806 | **100.0% / 0.00 / 0** |
  | all families | 88.2% / 0.51 / 90 | **88.8% / 0.39 / 8** |
  | Washington | 99.7% / 0.01 / 0 | unchanged |
  | place-labels | 100% / 0 / 0 | unchanged |

  All four scenes are now at or within eight gross pixels of the oracle.

  Two things worth writing down about how this went. The first attempt measured *no* change,
  because the consumer had failed to compile and the probe ran a stale binary -- the build script
  is `set -e` and the failure was hidden behind a `tail -1`. Check that a build succeeded before
  believing a measurement that says nothing happened. And the consumer's `Mesh` is built with
  positional initialisers, so a field inserted between `texture` and `texture1` silently took the
  next one's value; the new field goes after both, and says so.

## 17. Extensions beyond the oracle

Everything in §1 through §16 is a transcription: mbgl does a thing, this does the same thing, and
`mbgl-render` says whether it does. This section is for the other kind of work -- what this
frontend could do that mbgl does not -- and it exists because the two need different rules.

**The rule.** The oracle is compared pixel by pixel, and three of the four parity scenes are at
zero differing pixels. An extension must therefore be **opt-in and off by default**, and the
default path must stay byte-identical with it compiled in. An extension that cannot be switched
off is not an extension, it is a fork: it takes the measurement away, and after that nothing
distinguishes a deliberate divergence from a defect.

**What qualifies.** Three questions, in this order.

1. *Is it invisible to the oracle?* A storage backend, a transport, a target platform -- these
   change how pixels are produced and not what they are, so parity keeps working unchanged. These
   are cheap and should be preferred.
2. *If it is visible, is it opt-in per source or per layer?* A raster source that transcodes KTX2
   changes its own tiles and nothing else, so a scene that does not use one still measures.
3. *Does it need a second basis?* Anything visible needs something to be right against, and it
   cannot be mbgl. Say what it is measured against before building it, not after.

**Already here, and worth naming**, because they say what kind of thing belongs in this section:

- **The shared store, and more than one view of it.** §5. Every mbgl `Map` owns its own style,
  pyramid, file sources, atlases and workers, so N views cost N fetches and N bucket builds. Here
  buckets are process-scoped and refcounted and a view is a camera over them. Invisible to the
  oracle by construction -- one view of a shared store draws what one `Map` draws.
- **A `no_std` core.** `tessella-tile`, `tessella-capture-abi`, `tessella-orchestrate`,
  `tessella-source`, `tessella-layout` and `tessella-style` are all `#![no_std]` against `alloc`.
  That is a deployment mbgl cannot reach at all, and it costs the parity path nothing.
- **A stream that can be replayed.** The capture stream is a description of a frame rather than a
  frame, so `tools/capture-render` rasterizes one without a consumer and the parity probe reads
  the same bytes a real consumer does.

**The register.** Candidates, not decisions. Each says what it buys and what it is measured
against; none is scheduled.

- **KTX2 for raster tiles.** *Deferred, and the first entry here.* Filament's staging already carries
  `libktxreader.a` and `libbasis_transcoder.a` with a `ktxreader::Ktx2Reader` that transcodes a
  KTX2/Basis payload into a `filament::Texture`; the probe simply does not link them yet, so the
  consumer side is a link-list entry and a call. What is missing is a texture worth pointing it at.

  `Ktx2Reader::load` builds a *whole* texture, which is the shape of the thing. Two of this
  frontend's three textures are built the other way round -- the glyph atlas and the sprite atlas
  are packed at runtime by a shelf allocator and uploaded as dirty sub-rects, and a
  block-compressed format cannot take a sub-rect at an arbitrary offset because its blocks are
  4x4. Even if it could, neither should be lossy: an SDF atlas is a smooth distance field and
  block artefacts in it read as text with wobbling edges, and an icon is now sampled *nearest*
  precisely so its texels land one to a pixel.

  That leaves raster tiles, where it is a real if modest win. Today the source serves PNG, JPEG or
  WebP, `zune-png`/`zune-jpeg`/`image-webp` decode on the CPU and the result uploads as RGBA8. A
  Basis transcode is much cheaper than a PNG decode, and a 512-square tile drops from 1 MiB of
  video memory to 256 KiB at BC7. At the covers rendered here -- six tiles at z15 -- the memory is
  a few megabytes either way, so the decode time is the half that matters.

  **Blocked on parity, not on merit.** The oracle is `mbgl-render`, compared pixel by pixel, and
  three of the four scenes are at zero differing pixels. Any lossy texture path breaks that on the
  first frame and takes the measurement away with it. There is no lossless version worth having:
  KTX2's Zstandard supercompression pays for assets read from storage, and these atlases are
  generated in process while raster tiles already arrive PNG- or WebP-compressed, which beats
  Zstandard over RGBA8.

  **The shape if it lands.** Raster sources only, opt-in per source, after the symbol parity work
  closes, and measured against this frontend's own uncompressed output rather than against mbgl --
  which cannot consume KTX2 either, so it would be a tessella extension rather than a
  transcription. The style spec has no media type for it.

- **A PMTiles archive read where it lives, rather than where it was copied to.** *Open, and the
  first test says it is cheap.* `tessella-storage/pmtiles` reads a v3 archive **on local storage**.
  So showing a place there is no local extract for means cutting one first --
  `pmtiles extract https://build.protomaps.com/<date>.pmtiles out.pmtiles --bbox=...` -- which is
  fourteen megabytes and a step, for a view that reads a few dozen tiles.

  It does not have to be. `pmtiles serve / --bucket=https://build.protomaps.com` proxies the whole
  planet over HTTP range requests with no local copy at all, and a style pointed at
  `http://127.0.0.1:8091/<date>.json` draws the centre of Paris at zero gross pixels against the
  oracle, four runs the same. The format is designed for exactly this; it is only *this* reader
  that insists on a file.

  What that leaves in the loop is a proxy process. Range requests are what `HttpFileSource`
  already does, and the archive layout is the one `tessella-storage` already parses, so the gap is
  a directory-and-header reader that takes byte ranges from a URL instead of from a file. Then a
  style names a planet archive and the map draws anywhere, with no server and no extract.

  Invisible to the oracle -- a different way to reach the same tile bytes -- so by the first
  question above it needs no second basis.

- **MBTiles as a storage backend.** *Open, and cheap by the first test.* §16 leaves it open next to
  PMTiles. It is invisible to the oracle -- a different way to reach the same tile bytes -- so it
  needs no second basis, only the SQLite dependency the `cache` feature already carries.

- **A cross-process capture ring.** *Unexamined.* §4's transport is an SPSC ring in one process.
  The envelope ABI is `repr(C)` with offsets asserted against a generated C header, which is most
  of what a shared-memory variant between two processes would need. It would put the producer and
  the renderer in separate address spaces -- a crash domain boundary, and a way to drive a
  consumer this repo does not build. Invisible to the oracle, so the first test passes; nothing
  else about it has been thought through.

- **Parity was only ever measured flat, and the probe can now be pitched.** `render_probe` took a
  latitude, a longitude, a zoom and a viewport, so every number recorded above was taken with the
  camera at pitch zero and bearing zero. `mbgl-render` has taken `--pitch` and `--bearing` all
  along; the probe now takes the same two numbers in the same units, and `Host::setCamera` -- which
  existed and was never called -- is called only when they would change something, so the flat path
  is exactly the sequence of calls every earlier measurement went through. Re-running `one_roads`
  at an explicit pitch zero returns the previous frame pixel for pixel.

  The flat sweep, thirteen scenes against a freshly built oracle: nine at zero gross pixels,
  including every symbol family. `families`, `one_buildings` and `only_extrusion` share the same
  eight -- eight *isolated single pixels* on building edges, each about 20/255 out, which is
  rasteriser tie-breaking at polygon boundaries and about the floor for a different rasteriser.
  `icons_only` is 272, and that is one 18-by-17 icon this does not draw and mbgl does: 96 against
  97, so one collision decision.

  Pitched, at 45 degrees, is a different picture:

  | scene | flat | pitch 45 |
  | --- | --- | --- |
  | one_roads | 0 | 0 (also 0 at 30 and 60) |
  | one_place-labels | 0 | 0 |
  | one_buildings, only_extrusion | 8 | 36 |
  | families | 8 | 8,622 |
  | one_poi-labels | 0 | 10,104 |
  | icons_only | 272 | **33,943** |

  Lines are exact through 60 degrees and point labels are exact at 45, so the projection and the
  label plane are not the problem. What breaks is what branches on pitch. The icon-only scene draws
  **12 icons against the oracle's 148** -- 92% of them dropped -- and the ones that survive have a
  median blob of 218 pixels against 253, so they are also being drawn small. Both point at the
  perspective ratio: `placeFeature` scales a collision box by `tileToViewport`, which is
  `projectedAnchor.first * textPixelRatio` where the first element is the perspective ratio, and
  `approximateTileDistance`'s incidence stretch is `cameraToAnchorDistance / pitchFactor` -- identity
  at pitch zero and nothing like it at 45. The collision grid's `viewportPadding` also doubles when
  the camera is pitched, which `symbols.rs` notes and does not do.

  That is the next thread, and it is worth more than the 272 flat pixels above it.

- **Two pitch terms the placement path never had.** *Both fixed; the pitched scenes more than
  halve.* With the probe able to pitch, the first sweep at 45 degrees said lines and point labels
  were exact and everything else was not. Two things were missing, and reading told which:

  - **The perspective ratio.** `CollisionIndex::projectAndGetPerspectiveRatio` returns
    `0.5 + 0.5 * cameraToCenterDistance / w` beside the projected point, and its comment gives the
    reason outright -- collision is decided in viewport space, so a box has to shrink in the
    distance the way the drawn label does. `getProjectedCollisionBoundaries` scales the box by
    `textPixelRatio * perspectiveRatio`, and `placeLineFeature` scales a circle's radius by the
    same thing taken at the anchor. Nothing here scaled by it at all, so a label at the top of a
    pitched frame reserved as much viewport as one at the bottom -- several times what it draws.

    `FrameLabel` carries it, as it carries the glyph reach, because mbgl computes it once per
    feature at the anchor and that is the granularity. `w` comes off the label plane matrix rather
    than a second projection: that matrix is the coordinate matrix times the tile matrix, the
    coordinate matrix is affine, so its `w` is the `p[3]` mbgl reads. One at pitch zero, where
    every ground point shares a `w` equal to the camera distance.

  - **The viewport padding doubles when the camera is pitched.** `findViewportPadding` returns
    `viewportPaddingDefault * 2` the moment the pitch is non-zero -- not gradually with it. A
    pitched camera pulls the far edge of the world into the top of the frame, and the grid clamps
    what falls outside itself onto its boundary cells, so a margin that is too small does not
    merely miss those labels: it piles them into the edge cells to collide with everything else
    that landed there. This was noted in `symbols.rs` as a case "not produced here yet"; the probe
    now produces it.

  | scene at pitch 45 | before | + perspective | + padding |
  | --- | --- | --- | --- |
  | one_place-labels | 0 | 0 | 0 |
  | one_poi-labels | 10,104 | 2,745 | 2,745 |
  | families | 8,622 | 3,964 | 3,964 |
  | icons_only | 33,943 | 33,927 | **14,162** |

  Text wanted the ratio and icons wanted the margin, which is what the split says: a text label's
  box is large enough that scaling it wrongly decides collisions in the middle of the frame, while
  an icon's is small and what killed it was being clamped to an edge.

  Every flat measurement is byte-identical after both -- ten scenes re-run, all unchanged.
  `approximateTileDistance`'s incidence stretch, `(incidenceStretch - 1) * lastSegmentTile *
  |sin(angle)|` with `incidenceStretch = cameraToAnchorDistance / pitchFactor`, is still absent;
  it is the next candidate for what is left.

- **The probe is not reliably deterministic, and that has to be fixed before more icon work.**
  *Open, and blocking.* Chasing what was left of `icons_only` at pitch produced results that would
  not reproduce. The same binary, the same style, the same camera, eight runs: 0, 0, 9,520, 9,520,
  272, 272, 272 gross pixels, and then eight consecutive 272s. Every run reported `quiescent 1`
  and the same 27 renderables; what differed was `records`, which ranged 120 to 136.

  The quiescence condition is the wait loop's own comment made honest -- it was added for exactly
  this, after "6,502, 8,654 and 9,885 pixels of text" from one frame -- and it is too weak.
  "No new records for 200 ticks" is satisfied while a fetch is still outstanding: the producer
  goes quiet because it is *waiting*, not because it is finished. Raising the window to 800 ticks
  did not fix it either, which says the gap is not a longer silence but a missing signal.

  What is missing is a way to ask whether anything is still in flight. `tessella_status` reports a
  readiness and a reason and nothing about outstanding work, so the probe cannot distinguish "done"
  from "blocked". That is the thing to add -- a count of tiles requested and not yet answered --
  and it is worth adding for its own sake, since a consumer wanting a progress indicator needs the
  same number.

  **What this invalidates.** The single dropped icon recorded above as `icons_only`'s flat 272 is
  the modal result and probably real, but it was reported as though it were certain and it is not.
  An `isInsideGrid` port -- mbgl refuses a feature whose projected box lies wholly outside the
  padded grid, before testing it for collisions -- was written against this measurement, appeared
  to regress the flat path, and was reverted. That verdict is not trustworthy either. It should be
  attempted again once a run means something, and this note is here so it is attempted rather than
  assumed settled.

  Nothing above this entry is affected: those numbers were taken on scenes that reproduce, and the
  flat sweep was re-run whole after each change.

- **Glyphs were fetched exactly once, for whatever labels had landed by then.** *Fixed.* `Glyphs`
  carried a `scheduled` flag, set before the one fetch and never cleared, so `want_glyphs` returned
  early ever after. Tiles arrive over several ticks, so the labels that existed at that moment are
  not the labels the map ends up with: everything that landed afterwards wanted glyph ranges that
  were never asked for and drew without them, for the life of the map. It also made the frame a
  function of arrival timing -- the icon scene drew between 145 and 298 glyph quads across twelve
  identical runs.

  The flag is now the *set* that has been asked for, and `want_glyphs` schedules again when the
  landed tiles want something outside it. A subset test rather than a difference, because `wanted`
  is the total need of every landed tile rather than what is missing, so the job still fetches one
  cumulative set into one `Fonts` and hands that off. Guarded by `running` so two views ticking
  together still schedule once.

  All eight reproducing flat scenes are unchanged.

- **`tessella_pending`, and the probe waits on it.** *Added.* `TileSource::outstanding` counts
  tiles asked for and not yet answered plus an unfinished glyph fetch -- the two things that arrive
  after a tick rather than during it. Zero does not mean the map is complete, since a tile that
  failed is finished and still a hole; it means nothing further comes without another tick, which
  is the question a caller waiting for a settled frame is asking. `tessella_status` could not
  answer it, which is why the probe waited for records to stop instead -- a condition a source
  *blocked* on a fetch satisfies exactly as well as one that has finished.

  The probe's settle loop now requires both. A consumer driving a progress indicator reads the same
  number, which is why this is on the C API rather than in the test harness.

  **It is better and not yet right.** The icon scene went from three outcomes to mostly one -- nine
  runs in ten at 272 -- and then, after the glyph fix, back to a spread of 176 to 298 glyph quads.
  So `pending` closed one hole and there is another: something is still being decided by arrival
  order after the source reports nothing outstanding. The next thing to look at is that the settle
  loop always runs exactly 202 ticks, which says the 200-tick quiet window is never once reset --
  every run believes it settled immediately, and the variation is all in the wait loop before it,
  which exits on the first tick with any primitive drawn.

- **The probe reproduces now, and one number above it was noise.** *Closed.* Ten runs of the icon
  scene give 272 gross pixels every time; `one_poi-labels`, `families` and Washington give 0, 8 and
  0 across three runs each. The two fixes together did it -- `tessella_pending` so the settle loop
  can tell a finished source from a blocked one, and the glyph refetch so the atlas stops being a
  function of which tiles happened to land first.

  Tracing the settle loop is what showed the shape of it: at the first tick `records=80 pending=2
  prims=23 glyphs=208`, and by the fiftieth `records=128 pending=0 prims=27 glyphs=221`, holding
  there for the remaining twelve hundred. The map converges quickly and then genuinely stops; what
  was missing was any way to know it had. The probe keeps that trace behind `TSF_TRACE`, because
  the next argument about whether a frame settled is better had with it than without.

  `glyph_quads_drawn` still varies between 176 and 221 across runs whose *pixels* are identical, so
  that counter is counting something other than what is drawn -- hidden quads, most likely. Worth
  knowing before it is used as evidence for anything.

  **The correction.** The pitched figures above were taken before any of this and one of them does
  not survive. Re-measured with a probe that reproduces:

  | scene at pitch 45 | recorded above | actually |
  | --- | --- | --- |
  | one_roads | 0 | 0 |
  | one_place-labels | 0 | 0 |
  | one_buildings | 36 | 36 |
  | one_poi-labels | 2,745 | 2,745 |
  | families | 3,964 | 3,964 |
  | icons_only | 14,162 | **34,121** |

  So the viewport padding did not take the icon scene from 33,943 to 14,162; that improvement was
  a noisy run, and pitched icons are roughly where they started. The perspective ratio and the
  padding stay -- both are what mbgl does, and the poi-labels and all-families improvements
  reproduce -- but the icon scene at pitch is unexplained rather than half-fixed, and it is the
  largest open number here by a factor of ten.

- **Pitched icons are a cover problem, not a collision problem.** *Located, not fixed.* With a
  probe that reproduces, the icon scene at pitch 45 is 34,121 gross pixels and draws **8 icons
  against the oracle's 148**. Instrumenting both sides at that camera says where it comes from,
  and it is not where the last three fixes were:

  | | icon candidates offered | placed | drawn | tiles |
  | --- | --- | --- | --- | --- |
  | tessella | 1,604 | 1,222 | 8 | 39 renderables |
  | mbgl | 462 | 279 | 148 | 13 |

  The individual boxes are right. Ours reads `b(42.11,109.75,59.61,126.46)` where mbgl reads
  `b(42.55,110.13,59.23,126.02)` for the same anchor -- same place, same size to within half a
  pixel -- and the perspective ratios come out at a sensible 0.79 to 0.89. What is wrong is that
  there are three and a half times as many of them, all competing for one grid, so the ones that
  are actually on screen lose to ones that are not.

  mbgl renders thirteen tiles at that camera: `15/17602..17605/10744..10747`, a four-by-four block
  less three, every one at z15. So this is not the level-of-detail pass either -- `allowVariableZoom`
  is `pitch > tileLodPitchThreshold` and the threshold is sixty degrees, so at forty-five mbgl
  covers uniformly, as this does.

  **That table is wrong in two of its four columns, and the conclusion drawn from it was wrong.**
  Both errors are mine and both are the same mistake -- comparing numbers that are not the same
  kind of thing.

  *The cover is exactly right.* The 39 was renderables, not tiles. Asked directly, `cover()` at
  this camera returns thirteen tiles, `15/17602..17605/10744..10747`, which is mbgl's list to the
  identifier. Renderables are three per tile on both the flat and the pitched frame -- nine tiles
  and twenty-seven, thirteen and thirty-nine -- and the overzoom that looked pitch-specific is
  present flat as well, at the same scale, because this source's maxzoom is below fifteen.

  Finding that out cost a second self-inflicted detour: a probe of `cover()` across pitches
  returned nine tiles at every angle up to seventy degrees, which looked like a pitch-invariant
  cover and is in fact a test passing radians to a field documented in degrees. `ViewTransform::
  pitch` is degrees, `pitch_radians` converts, and `cover.rs`'s own test at line 1046 passes
  `FRAC_PI_6` -- half a degree, not thirty. That test is asserting almost nothing and should be
  fixed.

  *The candidate counts were not per frame.* Placement runs once per emitted frame and the probe
  ticks until quiescence, so a counter in `frame_in` accumulates across however many frames the
  settle took: 1,214 labels over eighteen buckets flat, for a nine-tile cover, is two frames. Per
  frame the pitched figure is nearer eight hundred against mbgl's 462 -- still more, but not the
  three and a half times that was written down, and not enough to carry the conclusion.

  So what is actually established is only the symptom: **8 icons drawn against 148**, reproducibly,
  with the cover correct, the tile count correct, and individual collision boxes agreeing to within
  half a pixel. Whatever is wrong is between those two facts, and it has not been found yet.

  **Measured properly, one frame against one frame, per tile.** Logging the offer and the outcome
  in `frame.rs`, where the tile is in scope, and taking only the last frame's thirteen records:

  | | offered | placed | drawn |
  | --- | --- | --- | --- |
  | tessella | **462** | 348 | 8 |
  | mbgl | **462** | 279 | 148 |

  The offers are identical -- 462 across `15/17602..17605/10744..10747`, the same thirteen tiles --
  so everything up to and including which symbols exist is right. What follows is not.

  Three things that are *not* the explanation, each checked rather than assumed:

  - `text_placed` is zero on every tile and `drawn` is zero with everything `fading`, at pitch
    **and flat alike**. `ViewSymbols` is rebuilt per frame, so `drawn` is always zero and those
    counters are not diagnostic. Reading them as a pitch symptom was a fourth wrong turn.
  - The geometry is not being culled, it is not being built: `glyph_quads_drawn` plus
    `glyph_quads_hidden` is 251 flat and about 51 pitched, and that is the size of the buffers, not
    of what survived a depth test. With *more* tiles at pitch.
  - It does not improve with settling. 34,011, 34,121, 34,011 and 31,378 gross at twenty, sixty,
    two hundred and six hundred quiet ticks -- so the 14,162 recorded earlier for the padding fix
    was a third flaky run, and the pitched figure is stably about 34,000.

  **It is not the producer at all.** Two more corrections and then the answer.

  This scene has **no text**. `text-field` is set but the features carry no `name`, so every label
  is icon-only, zero text quads are emitted at either pitch, and the `text_placed=0` that looked
  like a symptom was never one. And `glyph_quads_drawn`/`_hidden` count quads as a drawable's
  dynamic buffer is *built*, not as it is drawn, so a drawable the consumer keeps between frames is
  not counted -- which makes the 251-against-51 comparison meaningless too. Both of those were read
  as evidence and neither was.

  Measured on the producer, one frame, last thirteen buckets:

  | | icon quads emitted | quads with visible opacity | icons on screen |
  | --- | --- | --- | --- |
  | flat, 9 tiles | 340 | **237** | 96 |
  | pitch 45, 13 tiles | 462 | **237** | 8 |

  The producer emits *more* geometry at pitch, and marks exactly the same number of quads visible.
  Every decision it makes -- which symbols exist, which are offered, which are placed, which get
  opacity -- is the same or better at pitch than flat. Then 96 reach the screen and 8 do.

  So the icons are being drawn somewhere invisible, and the search moves out of placement and into
  the symbol vertex path: the dynamic buffer a point symbol carries and what the shader does with
  it. `lay_out` writes a point label's anchor into that buffer in *tile* units while the shader
  reads it as label-plane coordinates, which the comment in `write_line_positions` already flags as
  worth thousands of pixels when it goes wrong -- and a pitched label plane is exactly where a
  coincidence at pitch zero would stop holding.

- **The pitched probe does not reproduce, and every pitched number in this document is therefore
  suspect.** *Open, and it invalidates more than the last entry did.* Five identical runs of the
  icon scene at pitch 45 drew 11, 8, 11, 53 and 0 icons; the last produced an entirely black frame,
  630,000 gross pixels against the oracle. The "8 icons against 148" of the previous entry was one
  sample of that distribution reported as a fact.

  So the following, all recorded above as results, are unsupported: the perspective ratio taking
  poi-labels from 10,104 to 2,745 and the all-families scene from 8,622 to 3,964; the viewport
  padding taking the icon scene to 14,162 and then not; the icon scene sitting "stably about
  34,000". The two *fixes* stand on their own -- they are what `projectAndGetPerspectiveRatio` and
  `findViewportPadding` do, transcribed -- but the numbers attributed to them were measured on an
  instrument that was not measuring.

  **Flat is a different matter and mostly holds.** Four runs each of Washington, poi-labels, roads,
  all-families and the icon scene give 0, 0, 0, 8 and 272 every time. But it is not airtight
  either: an earlier five-run check of the icon scene flat produced one 9,520 among four 272s, so
  the same fault is present and merely rarer where there are nine tiles instead of thirteen.

  **The hole is precisely locatable.** `TileSource::outstanding` counts `inflight` -- tiles
  *submitted* and not yet landed -- so a tile the cover wants but has not been submitted for yet is
  invisible to it, and the settle loop sees zero and stops. More tiles means a wider window for
  that, which is why pitch is worse. `want()` already receives the whole cover, so the source could
  answer "wanted and not yet landed" instead, which is the question actually being asked.

  What stops that being a five-line change is failure: a tile that will never land must not keep
  the count above zero forever, or the probe waits out its budget on every scene with a bad tile.
  That needs per-tile failure state, which `failures` -- a count and one reason -- does not carry.
  Both halves are worth having anyway; a consumer showing progress wants the same pair.

  **What still stands, because it is not a rendered frame.** The producer-side counts are internal
  and timing-independent once the frame is emitted: at pitch 45 we offer 462 icons across
  `15/17602..17605/10744..10747`, which is mbgl's 462 across mbgl's thirteen tiles, and we mark 237
  quads visible at pitch and 237 flat. Those are the numbers to build on.

- **The frame depends on tile arrival order, and that is a defect in this frontend rather than in
  the probe.** *Open, and it outranks the pitch parity gap.* The probe now settles on the image --
  it ticks on and re-reads the framebuffer until two consecutive captures agree, bounded, and says
  whether it got there. It reports `stable_rounds 0` on every run: the image is already stable
  *within* a run. So the probe was never measuring too early, and the previous two entries blaming
  it were wrong about where the fault is.

  | | six runs |
  | --- | --- |
  | icon scene, flat, 9 tiles | 272, 272, 272, 272, 272, 272 |
  | icon scene, pitch 45, 13 tiles | 29,221, 29,221, 29,221, 25,800, 29,221, 21,794 |

  Same binary, same style, same camera. Renderables are 39 in every pitched run, `missing_atlas`
  and `missing_batches` are zero in every one, and the image does not change if it is ticked
  further. Only `records` moves, 160 to 168. So the map settles into genuinely different final
  scenes depending on the order the thirteen tiles happened to arrive in, and flat is deterministic
  only because nine tiles offer fewer orders to arrive in.

  That is worth more than the pitch gap it was found under. A map that draws differently depending
  on which tile the network returned first is a map whose output is not a function of its inputs,
  and every parity number here is a sample of a distribution rather than a measurement -- flat ones
  included, since flat is only rarely bitten rather than immune.

  **Where to look.** Not in placement: it walks `order`, which sorts by `(pass, depth_slot,
  sublayer, priority, z, x, y)` and is a total order, and there is no `HashMap` or `HashSet` in
  `tessella-orchestrate`, `tessella-place` or `tessella-layout` for iteration order to escape
  through. What *is* order-dependent is anything accumulated as tiles land: the sprite atlas's
  shelf packing, the glyph atlas's, and `Landed::by_tile`'s merge of buckets into an existing
  entry. The first two change texture coordinates rather than which symbol wins, so the merge is
  the first place to read.

- **Four candidates for the arrival-order defect, eliminated.** Recorded so the next attempt does
  not repeat them:

  - *Iteration order.* There is no `HashMap` or `HashSet` in `tessella-orchestrate`,
    `tessella-place` or `tessella-layout`, and `order` is a total sort. Nothing can escape through
    a hash seed.
  - *`Landed::by_tile`'s merge.* The `and_modify` arm appends a second build's buckets to an
    existing entry, which would be order-dependent -- and it never fires. Logged across three
    pitched runs: zero merges. The two dedup guards, `by_tile.contains_key(&job.tile)` and
    `inflight.insert(job.key)`, are sufficient, because an overscaled job's key carries the data
    tile *and* its `overscaled_z`.
  - *The tile cover.* Exactly mbgl's thirteen at pitch 45, identifier for identifier.
  - *Registry retention.* A retained stream would announce a symbol's dynamic and opacity buffers
    once and never update them, which is exactly the shape of this fault -- but the registry is
    optional and the probe's stream does not carry one, so every drawable is re-sent whole each
    frame.

  And one experiment that did *not* say what it first appeared to. Forcing a single worker made the
  pitched scene deterministic across five runs at 34,121, which looked like confirmation that
  concurrent landing order was the cause. It is not: the same knob makes the *flat* icon scene
  give 21,318, 12,070 and 12,070 where it otherwise gives a reliable 272. Serialising the pool does
  not order the arrivals so much as starve the settle, so the run ends earlier with less in it. The
  arrival path is still implicated and the mechanism is still not isolated.

  The honest position: there is no reproducible measurement for anything beyond the flat scenes
  that happen to be stable, and the parity numbers in this document should be read with that in
  mind.

- **Bearing turned the map the wrong way.** *Fixed, and it had never been measured.* Every parity
  number in this document was taken north-up. The probe has taken a bearing since it learned pitch,
  and the first time one was asked for the answer was plain: at 45 degrees the all-families scene
  differed by 186,672 gross pixels of 630,000, buildings by 78,257, poi-labels by 62,479 -- and
  cropping it showed the two renderers drawing different ground entirely, so the fault was the
  camera rather than any layer.

  Rendering *ours* at minus 45 against the oracle's plus 45 settled it in one command: 78,257 gross
  pixels became 45. mbgl converts a camera's bearing with `util::deg2rad(-*camera.bearing)`, and
  `bearing_radians` took the degrees straight through. The two conventions run opposite ways -- a
  bearing is the compass direction the camera *faces*, and turning the camera clockwise turns the
  world under it anticlockwise -- so the negation is the whole of it.

  | scene at bearing 45 | before | after |
  | --- | --- | --- |
  | one_roads | 0 | 0 |
  | one_place-labels | 2,425 | **0** |
  | one_buildings | 78,257 | **45** |
  | families | 186,672 | **3,627** |
  | one_poi-labels | 62,479 | **3,998** |

  Every flat scene is unchanged, which is what a sign that only matters when it is non-zero should
  do. What is left at bearing is symbol contention of the same shape as everywhere else, plus the
  45 on buildings that is the extrusion rasteriser floor.

  This is the answer to how much a whole untested axis is worth: the largest single visible defect
  found in this frontend, sitting behind a camera parameter nothing had ever varied.

- **The settled frame keeps ancestor tiles the oracle has dropped, and how many varies.** *Located.*
  Hashing every encoded bucket by tile across pitched runs finds the difference immediately, and it
  is not in the z15 tiles at all: the records that differ are at **z13 and z14**. Ancestors.

  Their count varies run to run -- ten parts, twelve, ten, ten -- and one run carried an extra tile
  entirely, 51 distinct bucket-hashes against 49. mbgl's placement log at the same camera lists
  thirteen tiles and every one is z15. So a settled frame here draws five or six ancestor tiles
  that the oracle does not draw at all, and which five or six depends on arrival order.

  That is the arrival-order defect, and it is also part of the pitch gap: an ancestor's buckets are
  offered to placement alongside its children's, so those extra tiles compete for the same grid.
  It explains a symbol scene losing icons without any symbol code being wrong.

  **Where.** `Substitution::get` answers `renderable: true` for any tile whose buckets are built,
  and `onion` prefetches ancestors two levels up so their buckets *are* built -- coarsest first, so
  the map becomes legible early, which is the right strategy for fetching and not a licence to keep
  drawing them. `updateRenderables` should use an ancestor only where the ideal tile is not
  renderable; that all thirteen ideal tiles are built by quiescence and ancestors are still drawn
  says the substitution is not collapsing when its children arrive.

  The comment sitting on `get` already names the mechanism it is missing: "renderable means the
  buckets are here. §13.2 asks that it eventually mean consumer-*acknowledged* rather than merely
  built, which is where mbgl's single-frame holes come from." The gap it describes is the same one
  measured here from the other end.

- **The pitch and bearing sweep, now that most of it reproduces.** Bearing is reliable everywhere
  tested -- three runs each at 90 degrees give 7,480 and 7,763 exactly. Pitch reproduces too on
  every scene but one: buildings give 36 three times and poi-labels 2,745 three times at pitch 45.
  The exception is `icons_only`, which gives 29,221, 33,227, 29,221, 25,800 pitched and a flat 272
  every time -- the scene with the most symbols and the most tiles, and so the most ancestors to
  substitute. That is the entry above, seen from the outside.

  Gross pixels of 630,000, against `mbgl-render` at the same camera:

  | scene | p0 b0 | p45 b0 | p0 b90 | p45 b90 | p60 b45 |
  | --- | --- | --- | --- | --- | --- |
  | one_roads | 0 | 0 | 0 | 0 | 0 |
  | one_place-labels | 0 | 0 | 0 | 0 | 0 |
  | one_water | 0 | 571 | 0 | 536 | 773 |
  | one_buildings | 8 | 36 | 7,480 | 46,038 | 1,894 |
  | one_poi-labels | 0 | 2,745 | 1,044 | 11,664 | 18,082 |
  | families | 8 | 3,964 | 7,763 | 50,427 | 10,588 |

  Lines and point labels are exact at every camera in the grid, which is worth stating plainly:
  two whole families hold up under rotation and tilt together.

  Everything else degrades with the camera, and the two axes compound rather than add -- buildings
  are 36 pitched, 7,480 rotated, and 46,038 doing both. That shape says the residue is not one
  missing term but something that both axes feed, which is what a shared projection or a shared
  collision space would look like. Extrusions are the worst of it and were never expected to be:
  8 flat is the rasteriser floor, 46,038 is not.

  Where to start is not in doubt. The ancestor substitution above is the only known defect that
  scales with tile count, both axes raise tile count, and it is the one thing already proven to
  change what reaches placement.

- **Withdrawn: the settled frame does not keep ancestors.** The entry above located the
  arrival-order defect in ancestor substitution on the strength of z13 and z14 buckets appearing in
  the encode hashes. They do appear -- and not in the settled frame. Logging the draw list once per
  frame instead of hashing every encode across the run shows the last two frames plainly:

      DRAWN 13 tiles: 14/8801/5372 15/17602/10745 15/17603/10744 ...
      DRAWN 13 tiles: 15/17602/10744 15/17602/10745 15/17603/10744 ...

  The second-to-last frame has `14/8801/5372` standing in for `15/17602/10744` while it loads, and
  the last has the thirteen z15 tiles and nothing else -- mbgl's list exactly. Substitution is
  working: it collapses when the child arrives, and `onion` feeds `pass.wanted` rather than the
  draw list, so a prefetched ancestor is fetched and not drawn. `update_renderables` is not at
  fault and neither is `Substitution::get`.

  So the final tile list is correct *and* deterministic, and the icon scene's image still varies
  between runs at pitch. The defect is somewhere with an identical set of thirteen tiles on both
  sides of it.

  **The mistake, which is worth more than the finding.** `packed_bytes` is per frame, so a log
  placed at its cache miss fires once per bucket per *frame*, and the probe emits many frames while
  a cover loads. Every ancestor in that hash log came from a frame that legitimately had one. This
  is the fourth time in this session a conclusion has been drawn from a counter accumulated across
  frames -- after `records`, after `glyph_quads_drawn`, after the icon candidate counts. The
  measurement that has never once misled is the one that names a frame and a tile and compares like
  with like: the Washington placement-sequence diff, the 462-against-462 offer count, and this draw
  list. Anything summed over a run should be assumed to be summing over frames until shown
  otherwise.

- **Arrival-order defect: placement is deterministic, emission is not.** Measured per frame and per
  tile, last frame only, three to four runs each at pitch 45 on `icons_only`:

  - draw list: identical, 13 z15 tiles
  - offered and placed counts per tile: identical
  - placement decisions, FNV over the `(text, icon, vertical)` sequence per tile: **identical**
  - encoded payload hashes: **differ**, and the count of freshly encoded buckets differs -- 2, 18, 2

  Images for those runs were 29,221 / 25,800 / 29,221 gross.

  So placement decides the same thing every run and the frame emits a different subset of it.
  `packed_bytes` is per `emit_group` call, so the hash log counts buckets encoded fresh that frame;
  a bucket not re-encoded keeps whatever the consumer already holds. Symbol opacity is written per
  frame from a placement that is global -- every tile competes in one grid -- so when a late tile
  lands and shifts the decisions, any bucket the frame does not re-emit keeps opacity from an
  earlier, smaller cover.

  Next: find what gates re-emission per bucket and whether symbol opacity can be excluded from it.
  A symbol's dynamic and opacity buffers change every frame by construction; geometry does not.

- **Per-bucket re-emission gate found; symbols excluded.** `emit_group`:

      if registry.is_some() && !fresh_buckets.contains(&(tile_index, bucket_index)) { continue; }

  `fresh_buckets` is `registry.is_new(&key)` -- first announcement only. So a bucket is encoded
  once ever, which is right for geometry and wrong for a symbol: its dynamic and opacity buffers
  are rewritten every frame from a placement that is global, so a bucket held back keeps opacity
  decided against whatever cover existed when it was announced.

  Instrumented at pitch 45 on `icons_only`: 78 non-symbol buckets skipped, 13 fresh; 28 symbol
  buckets skipped, 36 fresh. The 28 are the bug.

  Symbols now bypass the gate. Flat parity unchanged -- Washington 0, poi-labels 0, place-labels 0,
  icons_only 272, families 8, buildings at bearing 90 7,480.

  **Not sufficient.** `icons_only` at pitch still varies: 21,794 / 13,757 / 23,223 / 21,794 /
  34,177 / 31,294. So there is a second contributor with placement decisions identical and symbol
  buffers now re-sent every frame. Candidates not yet checked: the sprite atlas upload, and
  whatever else the consumer retains between frames.

  The cheaper shape is still open: re-sending a whole symbol bucket to update two small buffers is
  what mbgl avoids by uploading dynamic and opacity separately each frame. An ABI record for that
  is the follow-up, once the remaining nondeterminism is understood.

- **Sprite atlas eliminated.** Hashed pixels and packed positions per frame across four pitched
  runs of `icons_only`: `size=[1024,1024] n=73 pixels=7896af8d65c414dc pos=51db3747a7e1ec50`,
  identical every run, while the images were 13,757 / 13,757 / 6,035 / 13,757. Atlas content and
  packing are deterministic; symbol vertices are computed from them and still differ.

  Eliminated so far, each measured per frame and per tile: iteration order, `Landed::by_tile`'s
  merge, the tile cover, ancestor substitution, the registry gate for geometry, placement decisions
  themselves, and now the sprite atlas. Excluding symbols from the re-emission gate was a real fix
  and did not close it.

  Distributions across batches: before the gate fix 21,794..34,177; one batch after it 13,757 x3
  and 6,035; the next batch 29,221 x5, 34,177 x2, 21,794. The distribution itself moves between
  batches, which points at machine load rather than at anything the code chooses.

  **Stop eliminating and diff the stream.** Every remaining hypothesis is about what the consumer
  receives, so dump the wire records to a file per run and diff two runs byte for byte. That names
  the first divergent record instead of testing one candidate at a time, and it is the only
  approach left that cannot miss. The probe already reads every record; writing them out is a few
  lines in `Host::tick`.

- **Wire stream diffed; two findings, neither the one being hunted.** `TSF_DUMP=<path>` writes every
  record the consumer reads, so two runs can be compared byte for byte. Three pitched runs of
  `icons_only`: 144 records each, identical kind counts (47 GeometryAdd, 47 ViewUse, 18 UboUpdate,
  8 ViewRelease, 8 GeometryRemove, 7 TextureUpdate, 4 OrderUpdate, 4 CameraUpdate, 1 ViewDeclare).
  Two runs even produced identical stream lengths and different images.

  **Uninitialised tail padding on the wire.** The first differing record is a `ViewUse`, and the
  bytes that differ are at record offset 52-55 -- `fe7f0000` against `ff7f0000` -- past
  `_pad` at 35. `ViewUse`'s fields end at 36 and `size_of::<ViewUse>()` is 40, so four bytes of
  compiler tail padding are copied to the ring uninitialised. All 47 records carry it. That is
  process memory published to a consumer, and it makes the stream differ run to run whatever else
  is happening. Worth fixing on its own account; `#[repr(C)]` structs written as bytes should be
  zeroed or built field by field.

  **Geometry ids are assigned in arrival order.** `GeometryAdd` records differ at record offset 0,
  the id, and nowhere else -- payloads are byte-identical. So the same geometry gets a different
  handle depending on which tile landed first. Harmless in itself, and it defeats a positional
  diff: records at the same index in two streams are not the same logical record, which is why the
  `UboUpdate` differences above cannot be read as semantic yet.

  Next: key the diff by (tile, layer, sub-layer) rather than by position, then compare payloads.

- **ViewUse tail padding zeroed.** `_pad` was one byte where the record needed five: fields ended at
  35, `size_of::<ViewUse>()` is 40, and `as_bytes` copied four bytes of compiler padding to the
  ring uninitialised. All 47 records carried it and it differed run to run. Now `[u8; 5]`, so every
  byte of the record is a field, which is what the `WireRecord` contract already asked for. Header
  regenerated. Flat parity unchanged.

- **Arrival-order defect found: an ancestor's `ViewUse` is not always released.** With padding out
  of the way and the diff keyed by `(tile, layer, sub_layer, pass, flags, has_tile)` rather than by
  stream position -- geometry ids are handed out in arrival order, so position means nothing --
  three pitched runs of `icons_only` compare cleanly:

      run1 vs run2: 49 vs 47 ViewUse; only-A 2, only-B 0
      run1 vs run3: 49 vs 47 ViewUse; only-A 2, only-B 0
        only A: tile 13/4400/2686, layer 1, sub 0
        only A: tile 13/4400/2686, layer 1, sub 1

  Both extras are the symbol layer's two drawables on one z13 ancestor. The stream is cumulative:
  a `ViewUse` binds until a `ViewRelease` unbinds it, and the frame that legitimately drew that
  ancestor while its children loaded did not always release it when they arrived. The earlier
  reading that "the settled frame draws thirteen z15 tiles" was measuring the frame's own draw
  list, which is right; what the *consumer* holds is that list plus whatever was never released.

  Eight `ViewRelease` and eight `GeometryRemove` records are emitted, so release works in general
  and misses this case. That is the next thing to read: what decides a release when substitution
  collapses.

- **Narrowed to eleven symbol payloads.** Keyed comparison of two pitched streams, after the
  padding fix:

  | | run 1 | run 2 |
  | --- | --- | --- |
  | ViewUse emitted | 49 | 47 |
  | ViewRelease emitted | 10 | 8 |
  | live drawables at end | **39** | **39** |
  | live set (tile, layer, sub) | identical | identical |
  | final payload differs | **11 drawables** | |

  So the extra ancestor bindings in run 1 *are* released -- the previous entry's "not always
  released" was wrong, the counts balance -- and both runs leave the consumer holding the same
  thirty-nine drawables on the same tiles. What differs is the last payload emitted for eleven of
  them, and all eleven are layer 1 sub-layer 1: one half of the symbol layer.

  Everything else is now excluded by measurement: tile list, placement decisions, live drawable
  set, sprite atlas, geometry payload for every other layer. The remaining question is why a symbol
  bucket's last-emitted bytes differ between runs whose placement decisions are byte-identical.
  Vertices and indices come from layout, which is camera-free; that leaves the dynamic and opacity
  buffers, which are written per frame -- so the next step is to hash those two separately from the
  vertex data.

- **The producer is deterministic; the consumer is not.** Hashing the last symbol buffers emitted
  per bucket -- vertex count, dynamic buffer, opacity buffer -- across three pitched runs of
  `icons_only`: thirteen keys, all three runs **identical**, while the images were 29,221 / 18,192
  / 25,800 gross pixels.

  Together with the stream comparison above, the producer side is now fully accounted for: same
  tile list, same placement decisions, same live drawable set, same sprite atlas, same final symbol
  content. The `GeometryAdd` payload differences seen earlier were slab *references*, not content --
  the vertex, dynamic and opacity bytes live in slabs the ring dump never captured, so those
  offsets differ with allocation order and mean nothing.

  So the same bytes produce different pictures, and the defect is in `tessella_fluorite`. The
  obvious candidate is draw order: symbols are alpha-blended, so the order drawables are issued in
  changes the result, and `drawlist.cc` merges and retains runs across frames. Four `OrderUpdate`
  records go out per run; whether the consumer re-sorts on them or keeps insertion order within a
  batch is the next thing to read.

- **Producer audit complete: every byte it emits is identical between runs.** Three pitched runs of
  `icons_only`, images 13,757 / 34,177 / 14,162 gross. Compared, each keyed rather than positional:

  | | result |
  | --- | --- |
  | tile list | identical |
  | placement decisions per tile | identical |
  | live drawable set after releases | identical (39) |
  | final draw order | identical (52 entries) |
  | UBO contents | identical (6 slots) |
  | symbol vertex content | identical |
  | symbol dynamic buffer | identical |
  | symbol opacity buffer | identical |
  | sprite atlas pixels and packing | identical |

  So `tessella` is deterministic and `tessella_fluorite` is not: the same bytes make different
  pictures. Draw order is not the cause -- `DrawList::build` walks `order.entries` and never merges
  a symbol batch, and the order is identical.

  What is left on the consumer side: the seven `TextureUpdate` records, and how a repeated
  `GeometryAdd` for an id the consumer already knows is applied. Symbols are re-emitted every frame
  now, so each sends a second `GeometryAdd` under the same id; if that is dropped rather than
  replacing, the consumer keeps whichever version arrived first, which is exactly the shape of
  this fault. `filament_renderer.cc` appears to replace via `onRetire` then `meshes_[add.id] = ...`,
  so read that path and the texture uploads next.

- **Withdraw "producer audit complete".** The table above compared the *last* value logged per key
  across a whole run. Counting the log lines shows each bucket is encoded between one and four
  times, with 30, 37 and 31 encodes over three runs -- so "last per key" takes different keys from
  different frames, and says nothing about what any single frame emitted. That is the fifth time
  this session a conclusion has rested on a value accumulated across frames.

  What still stands, because each was compared per frame or is frame-independent: the tile list,
  the placement decisions, the live drawable set after releases, the final draw order, the UBO
  contents keyed by slot, and the sprite atlas. What does *not* stand is "symbol vertex, dynamic
  and opacity buffers are identical" -- that comparison must be redone with a frame marker, taking
  only the last frame's encodes.

  It also raises the question the counts imply: if a symbol bucket is encoded once in some runs and
  four times in others, the consumer's copy came from whichever frame encoded it last, and that
  frame differs. Excluding symbols from the re-emission gate was meant to make every frame re-encode
  them; the counts say it did not. Check whether a bucket reaches the encode loop at all when its
  tile is drawn but nothing about it changed.

- **Restore the producer audit; the withdrawal was wrong.** The withdrawal argued that "last value
  per key" could take different keys from different frames. Counted properly, it cannot here.
  Instrumenting each emit group and the symbol buckets reaching its encode loop, at pitch 45 on
  `icons_only`:

      groups: 7   symbol-bucket visits: 58
      distinct symbol buckets per group: group 4: 2, group 5: 4, group 6: 10, group 7: 13

  The last group covers **all thirteen**. `packed_bytes` dedups within a group, so each bucket
  encodes once per group it appears in -- which is what produced the one-to-four spread that
  prompted the withdrawal, and it is a count of *groups a tile was drawn in*, not evidence about
  the last one. Because group 7 holds every key, last-per-key is the last frame's value.

  So the audit stands: tile list, placement decisions, live drawable set, final draw order, UBO
  contents, symbol vertex content, dynamic buffer, opacity buffer and sprite atlas are all identical
  between runs whose images differ by twenty thousand pixels. The producer is deterministic; the
  consumer is not.

  Also confirmed here: excluding symbols from the re-emission gate works. Twenty of the fifty-eight
  visits were to buckets the gate would have skipped, and none were skipped.

- **Consumer: textures and copy semantics cleared; the puzzle stands.** Continuing on the consumer
  with three fresh pitched runs (25,800 / 25,800 / 18,192 gross):

  - `TextureUpdate`: two texture ids, uploaded once and six times in every run, final content
    identical. Cleared.
  - Draw order: `DrawList::build` walks `order.entries`, never merges a symbol batch, and the final
    order is identical entry for entry. Cleared.
  - Hash-map iteration: `meshes_`, `materials_`, `textures_` and `instances_` are `unordered_map`,
    and the only iteration over them is in `~FilamentRenderer`. Nothing in the render path. Cleared.
  - Slab lifetime: `frame.h` calls the index buffer borrowed and the producer repacks its slab table
    each frame that allocates, so a `GeometryAdd` read late would see moved bytes. `buildSymbol`
    mallocs and memcpys every attribute and the index buffer at add time, so this is not a hazard.
    Cleared.

  So every record's content is identical, the consumer copies it immediately, the draw order is
  identical, and the images differ by seven thousand pixels.

  Next, and it is the experiment that closes this either way: hash what the *consumer* holds at
  capture time -- the bytes it handed Filament per mesh -- and compare across runs. If those match,
  the divergence is inside Filament or the GPU path and nothing above it matters; if they differ,
  the consumer transformed identical input into different state, and the transform is in
  `buildSymbol`.

- **Found it: the per-frame symbol buffer differs per tile, and the earlier "identical" was a
  positional artefact.** Hashing what the consumer hands Filament, keyed by the tile id the record
  carries rather than by position:

      MESH t=14/8801/5373 sh=32 n=536 pos=8a62.. dat=8a62.. px=8a62.. placed=279dbe4fcd65ec85 idx=c4ee..
      MESH t=14/8801/5373 sh=32 n=536 pos=8a62.. dat=8a62.. px=8a62.. placed=b6db261302385d55 idx=c4ee..

  `pos`, `dat`, `px` and `idx` match; **`placed` does not**. `placed` is assembled verbatim from the
  `projected` and `fade` attributes, so the producer is sending different dynamic and opacity bytes
  for the same tile between runs. Affected tiles across three runs: `14/8801/5373`, `13/4401/2686`,
  `14/8802/5373` and `15/17603/10746` -- ancestors *and* at least one z15 tile. Mesh counts differ
  too, 19 / 19 / 18.

  The producer audit's "symbol dynamic and opacity buffers identical" was keyed by
  `(tile_index, bucket_index)`, which is **positional within the frame's tile list**. Sorting those
  lines and diffing proves the multiset matches, not that any tile's buffer matches. Same class of
  error as the cross-frame ones, in a different dimension: the key has to name the thing, not its
  index.

  So the hunt returns to the producer with a working handle. `write_opacity` and
  `write_line_positions` fill those two buffers per frame from placement; placement decisions were
  compared per tile and match, so the next question is what else feeds them -- the fade state, which
  `ViewSymbols` rebuilds per frame, is the obvious candidate.

- **Two measurements in tension; neither is wrong yet, one is mistimed.** Producer-side, hashing the
  dynamic and opacity buffers *after* `write_opacity`, keyed by real tile id, last value per tile:

      run1 vs run3: identical     (images 14,162 and 31,294 gross)
      run1 vs run2: one tile only, 13/4401/2686 with n=0 -- an ancestor carrying no symbols

  Consumer-side, hashing what is handed to Filament, also keyed by tile id, last value per tile:
  `placed` differs for `14/8801/5373`, `13/4401/2686`, `14/8802/5373` and `15/17603/10746`.

  Both cannot be true of the same frame. `placed` is packed from the `projected` and `fade`
  attributes, which are the dynamic and opacity buffers -- so if the producer's last value per tile
  matches and the consumer's does not, the two logs are reading different frames. The order within
  a frame is place, `write_opacity`, encode, so the encoded bytes should be the ones just written;
  what is not established is that the consumer's last `GeometryAdd` for a tile comes from the
  producer's last frame for that tile.

  Next: put a frame counter in both logs and compare the same frame number on each side. That
  settles which of the two is mistimed, and it is the same discipline that has resolved every step
  here -- name the thing, and name the instant.

## 18. The quad: four maps, four ihs platform views, one Flutter app

The stated exit requirement. Seattle, Tokyo, Switzerland and China in a 2x2 of ihs platform views
in a single Flutter app. What follows is what the existing pieces already give and what is missing.

**The stack.** `ihs` is ivi-homescreen, Toyota's Flutter embedder. `/mnt/dev/ihs_filament_view` is
Fluorite, its Filament-backed engine, exposing `FluoriteView` -- a `PlatformViewLink` on viewType
`views/fluorite-view`, one platform view per widget.

**Multiple views are already the design.** `packages/fluorite/src/ihs/engine_host.h`:

    /// Get or create the shared host. ...
    /// Platform thread only, which the ihs_pv factory guarantees: two views
    /// created concurrently would otherwise race to build the engine.
    static EngineHost* Acquire();

  Refcounted, one Filament engine shared across views, each view owning its swapchain. A quad is
  four `FluoriteView` widgets over that one engine.

**tessella already attaches rather than owns.** `tsf::FilamentRenderer(engine, scene, materialDir,
width, height)` takes an existing engine and scene -- the probe happens to make its own, but nothing
requires it. So a map can be rendered into a `FluoriteView`'s scene.

**Four maps or one map with four views.** `tessella_create` makes a map with one camera, and
`Pool::shared()` is process-wide, so four maps already share the worker pool while holding their own
source, cache and store. For four *different cities* that is the whole of it -- there are no tiles
in common, so §5's shared store buys nothing here. One map with four views is the §5 shape and is
not wired.

**Most of the bridge was already written.** `render_probe` is tessella and Fluorite in one view and
has been all along; the integration inside it is five lines --

    tsf::FilamentRenderer::configureCamera(*camera);
    auto backend = std::make_unique<tsf::FilamentRenderer>(engine, scene, materialDir, W, H);
    auto host = tsf::Host::create(config, lat, lon, zoom, &error);
    host->tick(*backend);   // per frame
    host->retire(seen);

-- and everything else in that file is Filament setup a `FluoriteView` already provides. Those five
lines are now `tsf::MapView`, which takes an engine and a scene and owns a `Host` and a
`FilamentRenderer`. `render_probe` goes through it, and parity is unchanged: Washington 0,
poi-labels 0, roads 0, icons_only 272, families 8, buildings at bearing 90 7,480.

**What is actually missing** is only the Flutter half: a `FluoriteView` handing its engine, scene and
size to `MapView::create`, a per-frame `tick()` on the view's callback, and the camera routed from
Dart. One `MapView` per pane, four maps sharing `Pool::shared()`.

**The shared scene, and what the quad needs because of it.** `filament_producer.cc` gives every
platform view the *same* scene -- `view_->setScene(filament_system->getFilamentScene())` -- and
differs them by camera: `FilamentProducer::FilamentProducer() : view_id_(g_next_view_slot.fetch_add(1))`,
with `ApplyEcsCamera` binding the ECS camera whose `getViewId` matches. The header's "the
creationParams carry a slot that nothing decodes yet" is stale; slots are handed out in creation
order and used.

One scene and four maps means four maps drawn in every pane. Filament's answer is layer masks, so
`FilamentRenderer` now takes a layer and sets it on both of its `RenderableManager` builders, and
`MapView` passes it through. One bit per view, `View::setVisibleLayers(0xFF, 1 << slot)` on the
other side. Default 0x01 leaves single-view callers exactly as they were; parity confirmed
unchanged.

**Remaining for the quad**, in order: a Fluorite-side hook that constructs a `MapView` per producer
with that producer's engine, scene, size and layer; a `tick()` on the producer's frame path;
`setVisibleLayers` per view; and the camera routed from Dart's `MapCamera` to `MapView::setCamera`.

### The quad renders, headless

`quad_probe` is the shared-scene case with no Flutter in it: one engine, one scene, four
`filament::View`s with their own camera and viewport over one swapchain, four `MapView`s on layers
0x01 through 0x08. Seattle z13, Tokyo z15, Liestal z12 and Shanghai z14 each draw in their own pane
and nowhere else, so the layer masking holds and four maps on one `Pool::shared()` reach quiescence
together.

One thing the probe got wrong twice before it was right: the map is drawn y-down and the PPM writer
flips the framebuffer to put it upright, and that flip swaps the pane rows as well. The rows are
laid out pre-flip.

**The Fluorite seam is written.** `FluoriteViewExtension` -- attach, frame, detach, installed through
`fluorite_set_view_extension` -- is the general form of the hook the quad needs, because the thing
it hands over is not map-specific: the view's engine, scene and `filament::View`, plus a per-frame
call on the Filament thread. `CreateView` narrows the view to `0x01 | (1 << (slot + 1))`, keeping
layer 0 for the ECS content so it still draws in every pane, and giving each slot one of the seven
bits left. Seven views fit; past that a view gets no attach.

**What is left** is the Dart package: `tessella_fluorite`'s `hook/` and `lib/` are still empty, so
nothing installs the extension yet. The native half needs a C entry point that takes a style and a
per-slot camera and registers a `FluoriteViewExtension` whose `attach` builds a `MapView` on layer
`1 << (slot + 1)` and whose `frame` ticks it.

### 512 was the atlas's starting size, not its size

*Found by the quad, which is what a Latin-only test scene could not do.* Tokyo and Shanghai rendered
road geometry with holes in the labels -- `八重洲一丁目` as `八　　一丁`, `さくら通り` as
`さくら　り` -- with the advance still spent for the missing characters.

Two causes, one asset and one real:

- **The glyph store has no CJK.** Every extracted stack under `/mnt/dev/renders/glyphs` carries the
  same 129 ranges, and 12288-12543, 19968-40959 and 44032-55295 are absent from all of them. A
  `Noto Sans Regular` stack mirrored from MapLibre's own font server covers 0-65535 and is what the
  quad style now asks for. Not customer data, not in any repo, and both renderers read it from the
  same server.

- **The atlas dropped what would not fit.** `ATLAS_SIZE = 512` was read off an observation --
  `symbol_style.dump` lists a `512x512` texture -- and generalised into a fixed page, with a comment
  reasoning that growing would invalidate rectangles already handed out. mbgl's actual behaviour is
  in `DynamicTextureAtlas::uploadGlyphs`: `startSize` is 512, and when a glyph of the set will not
  pack it releases what it packed, discards the texture and retries at double, until the whole set
  fits. 512 holds a few hundred glyphs; a CJK frame has thousands.

  `Atlas::add` now doubles and re-packs. The invalidation the comment feared does not arise: shelves
  only ever gain room to the right and below, so a bin's x and y do not move. What moves is the
  texture the rectangles are relative to, so the atlas marks itself wholly dirty and the extent goes
  out with the upload -- and the consumer already rebuilds a texture whose dimensions changed.
  Capped at 4096, because this atlas is per font stack and lives as long as the map rather than per
  bucket and per frame.

  Tokyo z15 against `mbgl-render`: **9,733 gross to 4,482**, every glyph drawn. What is left there
  is line-label placement on vertical lines, not glyphs. Washington 0 and families 8 unchanged.

The methodological note is the same one §17 already carries in a different dimension: a constant
read off one scene is a measurement, not a definition. This one had a comment explaining why it
could not be otherwise, and the explanation was wrong about the oracle rather than about the code.

### 60 fps with all four cells

The quad was ticking at 202 ms a frame while panning. Four defects, found by profiling rather
than by reading, and each one a thing being redone every frame that only ever changes when
something else does.

**The arena was serialised every frame.** `SlabArena::pack` was 83% of the producer's samples,
copying every slab into a fresh `Vec` so a consumer could read it. `SlabArena::in_region` exists
so that is not needed and nothing used it; the FFI now allocates a slab region beside the ring
and the arena writes into it. `Config` gains `slab_capacity`, zero taking 64 MiB.

**Nothing was ever swept.** `arena.sweep()` was not called anywhere in the frame path. Releasing
hands back the bytes a drawable held, but a slab whose last reference has gone is still a slot
with a length in the table until a sweep drops it. Over an owned arena that only wasted memory;
over a region it is fatal, because every slab looks live and nothing is reclaimable.

**A bump cursor does not retreat.** A moving map sweeps from the bottom and allocates at the top,
so gaps open below it and no region is large enough -- 800 KB a frame, a gigabyte in twenty
seconds. `compact_region` walks the sealed slabs in address order and moves each down onto the
last. It is invisible to a consumer, which is the point: a `SlabRef` names a slab and an offset
*within* it, and where the slab sits is only in the table. Nothing is invalidated and nothing is
re-announced, which is what separates it from DR-21's displacement.

**Glyph dependencies were surveyed every tick.** `want_glyphs` walked every bucket of every
landed tile calling `dependencies()` -- cloning a font stack per layer, iterating every feature's
text -- and only then reached the subset test that decided the walk had been unnecessary. It runs
on the landed generation now. A settled map's tick went from 1.00 ms to 0.002 ms; the still frame
was almost entirely this.

**Symbol layout was redone every frame.** Shaping, bidi, glyph resolution and quad building, none
of which depends on the camera; mbgl does it once when the tile is parsed. `SymbolCache` holds it,
keyed by the identity of the tile's bucket list so a re-parse misses rather than resolving to the
geometry it replaced.

Four maps, 640x480 panes, release, ticked serially on one thread:

|                  | before   | after    |
|------------------|----------|----------|
| motion tick p50  | 202.2 ms | 3.47 ms  |
| motion tick p99  | 327.4 ms | 6.09 ms  |
| motion tick max  | 340.3 ms | 7.50 ms  |
| still tick p50   | 1.00 ms  | 0.002 ms |
| peak rss         | 5473 MiB | 1155 MiB |

With the frame at 1.07 ms p50 and 1.80 p99: **4.5 ms at p50, 7.9 at p99, 9.6 at the worst frame
of 660** -- every percentile inside 16.7 ms, with the four ticks serialised, which is the
pessimistic arrangement. Fluorite runs every view's Filament work on one strand, so serial is what
it will see; per pane the worst is 1.16 ms.

`quad_bench` is what says so, and it says it in percentiles rather than means because a quad that
is fast on average and stalls every thirtieth frame drops frames. Its `prod`/`drain` split at the
FFI boundary is what made each of these findable: the consumer, the uploads and Filament together
never exceeded a millisecond, and all four defects were on the other side of the boundary.

**And the rings, which nothing had measured.** `Host` records the peak unread bytes, taken inside
`tick` between the producer publishing a frame and the consumer draining it -- the only moment the
figure means anything, since after the drain the ring is empty. The worst is not the densest
style: an all-families scene at 900x700 peaks at **11.0 MiB**, liberty's hundred and eleven layers
at 1920x1080 reach 6.0, and the quad's four street-level views 0.3 to 1.8. It was 256 MiB per view
on the grounds that a cover had never come close, which was true and was not a measurement.

Sixty-four, about six times the worst. The margin is asymmetric on purpose: a ring that fills
mid-pan drops a frame and the next retries, but one that cannot hold a *single* frame can never
make progress, because a frame is emitted whole or not at all.

Peak RSS **1156 to 472 MiB**, and 5473 to 472 across the whole of this work -- 11.6x. Timings
unchanged.

### The quad on a screen

It runs: four platform views under ivi-homescreen on Wayland, one Filament engine, one scene,
four `MapView`s on four layers, with a HUD over each pane. Everything between "the bundle builds"
and "the map is on the screen" was a defect, and none of them was visible from a probe.

**The library could not be loaded at all.** Dart's native-asset loader opens each asset with
`RTLD_LOCAL`, so fluorite's symbols are not in the global scope for anything else -- the first
call into the extension died on `undefined symbol: fluorite_set_view_extension`. A DT_NEEDED plus
an `$ORIGIN` rpath fixes it, and the two libraries sit in one directory so the loader resolves
the soname to the file Dart has already mapped: same path, same inode, one Filament. Then
`libfluorite_core_ffi.so` turned out not to be loadable either -- its `filament_FOUND` branch
links the package config's aggregate, which carries the core group and not gltfio, imageio or
the ubershader archive. A shared object is allowed undefined symbols, so nothing said so until a
`dlopen` failed.

**And it had to be the same Filament.** `filament_DIR`, a CMake package config pointing at the
emb overlay, wins over fluorite's own `FILAMENT_INCLUDE_DIR`, and that release has a non-const
`Builder::build`. Compiled against the newer headers the probes use, the builders mangle const
and nothing resolves. The materials have to come from that Filament too -- `matc` stamps a
version and Filament refuses one it did not write.

**Then the view.** Four things the probes set on their own views and fluorite does not, each
found by looking at what came up:

- *Post-processing off.* A map is display-referred sRGB, like the UI over it. Filament's pipeline
  treats what a shader wrote as scene-referred light and tone-maps it: roads at a couple of
  percent contrast against their background, water grey. Reproduced headlessly with
  `TSF_POSTPROCESS`, which `render_probe` honours.
- *Stencil on.* Tessella clips tiles with a stencil pass, and Filament panics rather than
  degrades when a view asks for a stencil the swapchain does not carry -- so the swapchain gains
  the flag as well.
- *The camera, every frame.* `ApplyEcsCamera` runs immediately before the extension's callback.
- *The Y sign, which is the target's rather than the producer's.* An offscreen target read back
  with `readPixels` comes out bottom-up and every probe writes it top-down again, so the two
  flips cancel and the camera carries one; a swapchain presented straight to a compositor is read
  by nobody. `configureCamera` takes the answer instead of assuming it, and `FilamentRenderer`
  takes the same one for its scissor boxes -- flipped one way and scissored the other, every tile
  is clipped to where its own reflection overlaps it.

### A frame's fades are the frame's

*Found because one pane had no labels.* `Fades::step` rebuilds its map from the placements it is
handed -- that is how a symbol whose tile was released is dropped -- so it is a per-frame call.
`frame_in` made it per bucket, with only that bucket's placements, so each bucket wiped the one
before it. A last bucket that placed nothing emptied the map; `settle` then found it settled,
because an empty set is, and returned without creating a single fade. **Every label in the frame
drew at zero opacity.**

Liestal at z12 in a 959x440 viewport: 1848 glyph quads laid out and none drawn, while either
symbol layer alone drew normally. It needed two symbol layers *and* a particular viewport, which
is why every scene measured so far missed it -- the parity scenes have one symbol layer each, and
the quad probe runs at 640x480.

`ViewSymbols::step` is the per-frame call now, `settle` steps before it tests rather than after
(testing first meant a map whose fades do not exist yet created none), and `frame` -- the
whole-frame convenience -- does all three parts rather than the middle one.

### What the HUD says

`tessella_fluorite_stats_of` reports a pane's zoom, frame rate, tick split into produce and drain,
the worst of the last 120 frames of each, primitives, records, tiles outstanding, both regions,
and readiness. Sampled at 2 Hz rather than pushed: a callback per frame would cost more than the
thing it measures, and would take the render thread's lock to say so.

### The zoom sweep, and the frames that go blank

The quad sweeps its whole zoom range on start: out to 0, in to 18, home, eased at both ends and
across the joins. Zoom is interpolated rather than scale -- a level is a doubling, so
linear-in-zoom is what keeps the apparent rate constant. `quad_bench` runs the same three legs,
which is the hardest thing the pipeline is asked to do: a pan moves the camera, a sweep replaces
everything it is looking at, twenty times over.

The timings are comfortable. Four panes at 640x480, 900 frames: tick p50 0.90 ms, p95 3.08, p99
3.86, worst 9.70; frame p50 0.58, worst 3.55. Inside a 16.7 ms budget at every percentile, no
region-full ticks, the slab regions steady at 4-6 MiB. The p50 is *lower* than a pan's, because
the extremes are cheap -- z0 to z2 is one tile and z16 to z18 is overzoomed from the source's z15.

**And it flickers.** Between the map and black, badly enough to be the first thing anyone says
about it. The timings say nothing about this, which is the point of writing it down here.

`sweep.blank` counts frames the producer emitted with nothing in them: `beginFrame` clears the
scene and rebuilds it from the frame's order, so an empty one is a pane that goes black. **777 to
853 of 930.** A captured mid-sweep frame is three panes black and a fourth showing bare
background. `TSF_BENCH_SWEEP_TRACE` puts zoom beside primitives per frame and says what it is
not: not a zoom -- blanks land at 12.4, 9.0, 0.4, 17.8 alike -- and not presentation, because the
producer emitted the frame and the consumer drained it. Roughly one emitted frame in five drains
records and leaves zero renderables.

**Where this got to.** The draw order is built in the binding pass, not in the geometry loop, so
the freshness gate that holds back re-encoding a bucket the consumer already has is *not* the
obvious culprit -- which is as far as it is honest to say. The next step is the consumer's side of
one blank frame: an order arrives, and no order entry becomes a renderable. Either the order is
empty when it reaches the wire, or its entries name geometry `meshes_` does not hold. `missing()`
is zero, so it is not a material.

This is the same family as the arrival-order defect already recorded: a frame that is a function
of what happened to have landed rather than of what the camera is looking at.

### Pitch exposes a label arrangement that was never written

Asking the quad to lean back fifteen degrees turned every road label into an opaque slab with the
text over it. Point labels in the same frame are perfect.

Isolated headlessly at Seattle z15, pitch 15, 959x359: the road layer alone draws 1,479 glyph
quads and reports five pitched labels; the place layer alone draws 56 and reports none. Bearing
does not do it -- a bearing of 15 with no pitch is clean, which is how the first attempt at this
missed it.

`filament_renderer.cc` had the reason written down and then talked itself out of it. A label
pitched *with the map* lays out in the tile's plane: its plane matrix is the identity and its
coord matrix carries the tile's projection, so the offsets between them are in tile units rather
than pixels. The viewport shader adds them as pixels. The path was skipped for exactly that
reason, and later un-skipped on the grounds that "the shader's arithmetic is the same for both".
It is not, and the slab is what the difference looks like: the quad reaches far enough past its
glyph to sample the atlas either side.

Left drawing rather than skipped -- a pane with no road labels is not obviously better than one
with slabs, and the counter is what makes the case. What it wants is the second arrangement
written, not a branch in the consumer.

### What a zoom sweep found

Six defects, none of which any still-frame test could reach, and all of them from moving a camera
that had only ever been placed:

| | |
|---|---|
| An order held once and drawn once | 777 of 930 frames black |
| A stored empty background | every frame with nothing to draw |
| Arabic shaping | a panic on any joining script outside basic Arabic |
| World copies deduped away | half a world on a screen that holds two and a half |
| Glyphs fetched once | every script arriving later drew from an empty alphabet |
| Glyphs fetched from the newest survey alone | and forgot the last one |

The last two are the same bug seen from both ends, and the second was mine: the drain gate exists
because firing on the first tile asks for the prefetched ancestor's alphabet and nothing else, and
lifting it entirely replaces a complete `Fonts` with a partial one. A fetch hands the map a *new*
atlas rather than adding to the old, so it has to ask for everything asked for so far.

### Text made of fragments

The CJK panes drew their labels as fragments -- each glyph a magnified corner of itself with its
neighbours' corners around it. That is what a shader does when it divides atlas coordinates by the
wrong number.

It took a while to find because it is invisible everywhere it was looked for. Flat is clean.
Settled is clean. Latin is clean. `render_probe`, `quad_probe`, `extension_probe` and both
benchmark phases are clean, pitched and moving, at the pane's exact size -- and mbgl agrees with
all of them. It happens on the *third* pane of a *running* app while the camera moves, and until
the frame dump could be told to sample periodically and to pick a view, there was no way to look
at it.

Two causes, one found and one still open.

**The gamma scale read pitch as radians.** `ViewTransform::pitch` is degrees, as every angle on it
is, and `cos` is not. At fifteen degrees this computed cos(15 radians), which is *negative*: the
distance field's ramp inverted and every map-aligned label drew as an opaque slab with its text
over it. Line labels take that alignment by default, so a pitched map lost all of them. At zero
the two readings agree, which is the whole reason a renderer only ever exercised flat could not
find it. Against `mbgl-render` at Seattle z15 pitch 15, road labels alone: 23,653 gross to 3,505.

**And a drawable's `texsize` can be a frame behind its atlas.** A glyph fetch that finds a new
script hands the map a larger atlas; the upload carries the new size and a drawable whose uniforms
were not re-sent still names the old one. `FilamentRenderer::atlasMismatched` counts it -- three
drawables on Tokyo, eight on Shanghai, over a minute of sweeping, none on the Latin panes.

The consumer now takes the size from the texture it has bound, which is the authoritative answer
and one it already had. That is a correction, not a cure: the producer is still emitting a
drawable that disagrees with the atlas it uploaded in the same frame.

**What the cure wants.** `Map::set_fonts` marking the frame dirty says *redraw*; what a new atlas
needs is *re-tell*. `session.forget(view_id)` is the direct way to say it and does not work: the
next frame re-announces geometry the consumer still holds, and the retire path then frees a
texture something is still using -- "Handle (Texture) is being used after it has been freed", on
the all-families scene, immediately. So there is a consumer lifetime bug sitting behind this one,
and it is the next thing to pull on.

### The cross-tile index is wired, and the fades run for the first time

`CrossTileIndex` was transcribed from mbgl's `CrossTileSymbolLayerIndex`, documented, tested
against its own cases, and reachable from nothing but those tests. `frame.rs` assigned
`cross_tile_id = base + index` instead: an ordinal into whatever order a frame happened to walk
its buckets in, from a counter reset to one every frame.

Two things followed, and they had to be fixed together. `ViewSymbols` was constructed *inside*
`emit_group`, so a frame began with no fades at all and a label's opacity was decided against a
history one tick long. And the identity a fade is keyed by did not survive a frame either, so
there was nothing for a persistent fade to key on. Making the state outlive the frame without
stable identities would have been worse than neither: the fades would have run, keyed on numbers
that mean a different label each frame.

The ordinal's failure is not that it is arbitrary but that it *moves*. Insert a tile ahead of
another in the walk and every number after it shifts, so labels that did not move inherit the
fade state of whatever now holds their old slot -- which is the same shape as the zoom crossing
the index exists for, where a tile is replaced by four children and "Detroit" is a different
instance at a different index.

**What the test had to be, and what it could not be.** §13.3 already recorded this trap for the
*criterion*: a continuity assertion passes with the index deleted, because a label handed a fresh
identity every frame is always a new label taking its first step. The same trap sits under the
regression test. A settled scene walks its buckets in the same order every frame, so an ordinal is
stable across it and "the numbers do not change" passes with the defect in place -- verified by
restoring the ordinal and watching that test go on passing. The case that discriminates is a tile
*arriving beside* another: the walk changes, the ordinal shifts, the index does not. Restoring the
ordinal fails that one and the re-parse one, and both pass with the index.

**Identity is remembered here, not on the bucket.** mbgl keeps `crossTileID` on the symbol
instance, so `add_bucket` returning early for a bucket it has seen leaves nothing to fill in. Here
the laid-out symbols are shared and immutable, so the assignment is memoised per (layer, tile),
holding the bucket list's `Arc` rather than comparing its address -- a dead `Arc` frees its address
for the next allocation, and a memo keyed on a recycled address would hand a new tile the previous
occupant's identities.

Cost is nothing measurable: the index runs only for a bucket whose parse changed, and the quad
bench is inside its usual run-to-run spread on every figure, with zero blank frames on all four
panes.


### The consumer lifetime bug behind the atlas staleness, and why the re-announce is not wanted

The entry above ended "there is a consumer lifetime bug sitting behind this one, and it is the
next thing to pull on". Both halves of that turned out differently than expected: the bug is real
and is fixed, and the thing it was blocking should not be done.

**The bug.** `FilamentRenderer` keeps one `MaterialInstance` per (layer, shader, ubo slot) across
frames, and an instance holds the samplers set on it until they are set again. `onTexture`
destroys a texture outright when an atlas grows, and every cached instance that named it goes on
naming it. It is only rebound if the same key comes round again; a full re-announce reshuffles
which drawable lands on which key, and the first instance reused for a drawable that binds no
texture draws with a freed handle. That is "Handle id=... (Texture) is being used after it has
been freed", on the all-families scene, immediately.

The fix is to drop the instance cache when a texture is replaced. That is safe exactly there and
nowhere later: `beginFrame` has already destroyed every renderable of the previous frame and
batches are only issued in `endFrame`, so at the moment a texture update is handled no instance is
attached to anything. They are a cache; `issue` rebuilds and rebinds what it needs.

**The re-announce is not wanted.** `Map::set_fonts` marking the frame dirty says *redraw* where a
new atlas needs *re-tell*, and `session.forget(view_id)` was the direct way to say it. With the
crash fixed it runs clean -- and there is nothing left for it to fix. A symbol drawable is
re-encoded and re-announced every frame now, because its vertices carry the camera, so it names
the current atlas by construction: `atlasMismatched` reads zero without the re-announce on
all-families, Tokyo and Shanghai, the three scenes that produced it. Forgetting the view would
re-announce every fill and line in the cover each time a glyph range lands, for geometry no font
ever touched.

So the open item closes by a route it did not anticipate: the symbol re-announce landed for the
label-anchoring defect and took this with it. The consumer fix is kept on its own merits -- a
stranded sampler is a hazard on any texture replacement, and atlas growth is routine.

**A measurement not to trust, recorded so it is not repeated.** The first pass at this attributed
a large slowdown to the re-announce, from a quad bench run taken while another homescreen and a
Flutter build were on the machine. The figures were contention, not cost. The decision above rests
on `atlasMismatched` instead, which is a correctness count and does not care what else is running.


### A viewport is a property a map can be told, not one it is born with

`width` and `height` were settable only at `tessella_create`, so a consumer whose surface changed
had exactly one option: destroy the map and build another. The Filament producer takes it --
`ResizeOnThread` calls `Release()`, which calls `DestroyView()`, which detaches the view extension
and takes the map with it -- so every window resize refetched every tile, rebuilt every bucket and
re-shaped every glyph, for a change that moves no camera.

`tessella_set_viewport` is the missing primitive. Nothing is invalidated by it: the cover is
recomputed every frame from the view, so the next tick sees a changed camera and rewrites the
matrices by the path a pan takes. What survives is everything a resize does not change -- the
tiles, their buckets, the layouts, and the label identities with the fades keyed on them.

**The half that was easy to miss.** A resize has to *register*, and it did not. Nothing else in
`CameraKey` moves when a window is resized: the centre, zoom, bearing, pitch and `pixels_per_meter`
are all functions of where the camera points and how far away it is, not of how large the surface
is. A map that merely accepted a new size would have reported a settled camera and gone on drawing
through the matrices of the old viewport. The viewport is a field of the key for that reason, and
removing it again fails `a_resize_is_a_camera_change` and nothing else -- which is the check that
it is load-bearing rather than decorative.

A resize is constrained like any other camera change, because `camera::constrained`'s zoom floor is
a function of the viewport's height: a view that is legal at 768 pixels tall is not necessarily
legal at 200, and a resize that skipped the constraint would be the one way left to reach the state
that function exists to make unreachable.

What is *not* done here is the consumer half: `FilamentProducer::DestroyView` still detaches on
resize, and `GlFilamentProducer::ResizeOnThread` still goes through `Release()`/`Allocate()`. The
producer has to keep its view and hand the new size across instead. This is the primitive that made
that possible, not the change that uses it.

### The labels were never fading, and that is what "flying text" was

Every parity render agreed, and both sides were drawing the same thing: a map with no crossfade.
`FrameOptions::increment` was the constant `1.0`, `Fade::step` moves an opacity by that much and
clamps, so a label reached full opacity or full transparency in one frame. That is not a defect
against the oracle -- `mbgl-render` runs in static map mode and `Placement::symbolFadeChange`
short-circuits to exactly `1.0` there. Both renderers were right, and the comparison could not see
it.

It is wrong for a map somebody is looking at. During a zoom the set of placed anchors changes
constantly: a line label has an anchor every `symbol-spacing` along its road and which of them win
depends on what else is on screen, so a name stops being drawn at one anchor and starts at another
further along. With a crossfade that reads as one label yielding to another. With none it reads as
the text having flown down the road, which is what was reported, from about z11 in -- which is
where the style's line-placed road labels start.

The measurements that located it are worth keeping, because each one ruled out a whole class:

- Settled at the sweep's own camera, 1.16% gross. Settled at 2.25x overscale past a source's
  maxzoom, 0.13%. Settled on the *same tile set the sweep was drawing* -- z13 tiles at view 14.25,
  forced by a maxzoom override so mbgl used them too -- 0.41%. So neither overscale nor a lagging
  zoom latch is the cause; each individual camera is right.
- Held: freezing the camera for the last forty frames of a sweep and dumping three of them gives
  *pixel-identical* frames. So nothing the motion leaves behind is unstable either.

Every frame correct and the sequence wrong is what points at a per-frame decision changing rather
than a value being miscomputed -- and the fades are what exist to cover that.

`fade::increment` was already written, and already unused, in the same way the cross-tile index
was. `tessella_advance` is the missing input: the elapsed milliseconds a fade is a fraction of.
A map that is never told keeps the still-picture behaviour, so every capture and every probe is
untouched.

### The zoom gives way, not the centre

`camera::constrained` followed mbgl's decomposition: a zoom floor from the frustum's extent, then
the centre clamped into what is left. Correct, and the wrong trade for a map somebody is aiming.
At zoom zero in a short pitched viewport a camera on Seattle cannot have all three of its centre,
its zoom and no off-world strip, and mbgl gives up the centre -- so the map slides south and the
city leaves the screen, which is what was reported after the strip itself was fixed.

Folding the latitude into the floor gives up the zoom instead: a request to zoom out further than
the world allows stops a little short. That is the failure nobody notices. It is stricter than mbgl
away from the equator, where a short side has less world to cover, and identical to it flat on the
equator -- which is what the flat test's control now uses, since Seattle's latitude is exactly
where the two diverge.

### Placement cadence: a real difference from mbgl, and why adopting it naively fails

mbgl does not run the collision pass every frame. `PlacementController::placementIsRecent` gates
it, and while that holds `RenderOrchestrator` does not place at all -- it steps the opacities
toward the placement it already has and reprojects the line labels against the current camera.
`Placement::getUpdatePeriod` is at least `DEFAULT_TRANSITION_DURATION`, so a new placement every
300 ms and eighteen frames at sixty of keeping the last one. When new symbol buckets arrive the
period drops to 30 ms -- but only on an untilted view, because on a tilted one "the new symbols are
normally far away and the user is not that interested to see them ASAP". The quad is pitched, so
mbgl would hold the full 300 ms there.

We place every frame. That is a genuine divergence and it is worth closing.

**It cannot be closed by gating alone.** Tried, measured, reverted: gating on the period takes
frame 277 of the zoom sweep from 949 text pixels to **zero**. The reason is where opacity lives.
mbgl keeps it on the bucket -- `updateBucketOpacities` writes into the bucket's own
`placedSymbols`, so a bucket the current placement did not touch goes on drawing at the opacity it
was last given. Here the label list is rebuilt every frame from the cached layout and each label's
opacity is looked up by cross-tile id in what the last placement decided; a label that is not in
that set reads as hidden rather than as unchanged. Between placements the tile set moves on, the
kept decision no longer names what is on screen, and the map empties.

So the order is: opacity has to become a property of the laid-out bucket, surviving a frame that
did not place, before the cadence can be gated. Doing it the other way round trades a defect for a
worse one.

**And the cadence is not the flying text either.** With the gate in place the labels that remained
still moved. That is three explanations tried against this defect -- symbol geometry announced once,
the fade rate, and now the placement cadence -- of which the first two were real defects that
needed fixing and none of which was the cause. What is established, from diffing a swept frame
against a settled one at the same camera and tile zoom, is that every road, coastline and fill is
pixel-identical and only the text differs. The producer is choosing different anchors, and the
input that differs is what previous frames did. The next thing to instrument is the placement
decision itself -- which anchors win, per frame, dumped for both runs and diffed directly -- rather
than another guess at which piece of state carries the difference.

### The flying text: a label drawn where nothing wrote it

`write_line_positions` hides a label it could not walk, and says why in its own comment: what
`lay_out` left in the dynamic buffer is the anchor in *tile* units, the shader reads that buffer as
label-plane coordinates, and a label drawn without a walk "lands some thousands of pixels from
where it belongs". `write_opacity` then runs and writes every label's fade across the whole opacity
buffer, hidden ones included, so the hide is undone.

That was harmless while the fades were rebuilt each frame: a label never offered to placement had
no fade entry, which reads as hidden, and the overwrite wrote the same zero. It stopped being
harmless the moment the fades began persisting across frames. A label that was placed last frame
and whose road runs out this one now *has* an entry -- it is fading out -- so the overwrite gives
it an opacity and it is drawn at a position nothing wrote this frame. Thousands of pixels away.
Which is the flying.

The icon half has always re-hidden its `without_room` labels after `write_opacity`. The text half
did not, and nothing noticed until the fades were made real.

**What found it, after three wrong answers.** Dumping the placement decision itself -- id, tile,
anchor, opacity, per label per frame -- and diffing consecutive swept frames. *Zero* labels changed
anchor between frames 279 and 281. That killed every remaining theory about placement churn at a
stroke: the decision is stable, the id is stable, the anchor is stable, and the text still moves.
Which leaves only the step between an anchor and a drawn glyph, and the one branch there that
leaves the buffer untouched.

The lesson is the one the earlier entries keep circling. Three explanations were tried from
plausible mechanisms and pixel diffs -- geometry announced once, the fade rate, the placement
cadence -- and two of them were real defects that needed fixing while none was this. The thing that
worked was dumping the intermediate and comparing it, which is what §16 said to do about mbgl and
is no less true of comparing a frame against the frame before it.

Checked at every zoom from 11 to 16 against `mbgl-render`, settled, pitched: 0.86%, 2.52%, 2.60%,
2.75%, 2.02%, 0.90% gross. The middle of that range is the settled probe stopping while fades are
still part way, not misplacement -- the same camera measured 1.16% before the fades ran at all.

### The text was half the colour it should be, and the fades are parked

Caught by eye, from the parity captures: our glyphs topped out around (137,133,123) where the
style asks for `#333333` and mbgl reaches (16,15,14). The tint is the giveaway -- ours is *warm*,
which is the beige background showing through, so the glyphs were being drawn at about 55% alpha.
Uniform across z11-z16, and absent from every capture taken before the fades were made real:
darkest (7,7,7) with 7951 dark pixels before, (137,132,122) with 140 after.

It is the gap recorded on `Map::advance`, and the note there was wrong about its reach. "It does
not arise on a moving map" is true and irrelevant: it arises on every *settled* one, which is every
capture and every parity comparison this project runs. Opacities travel in the vertices, so a
frame that is not emitted is a fade that does not move, and a settled map does not emit -- so the
labels stopped part way and stayed there.

Marking the map dirty while a fade is in flight does complete them -- `fading` runs 294 to 0 -- and
renders **the whole frame black**, every pixel of it, while the producer reports emitting
(geometries 8, drawables 21) and every tick returns OK. Suppressing the slab release that a
re-announcement stages makes no difference, so it is not the arena handing back bytes the same
frame is using. A map emitting on every tick draws nothing, and that is the defect to find.

Until it is, the frame's elapsed time is not passed to the producer. `tessella_advance`,
`MapView::advance` and `Host::advance` all exist and are unused, which is the honest shape: the
mechanism is right and cannot be turned on. A fade completes in one step, labels draw at full
opacity, and the captures agree with the oracle again -- darkest (16,15,14) against mbgl's
(16,15,14) at z14 and (47,45,42) against (47,45,42) at z16, with gross falling from 2.75% to 1.46%
and 0.90% to 0.56%.

The lesson for the parity metric: a 2.75% gross reading was recorded as "the settled probe stopping
while fades are part way", which was true and was treated as benign. It was a regression in the
text's colour, visible at a glance, and nobody looked because the number was small.

### A frame that writes anything must send a camera

The reader states the protocol: "A frame opens at its first record and closes at its camera, which
is the commit point ... nothing is emitted after it. So there is no end-of-frame marker to look for
and none is needed." `emit_group` violated it. The camera was gated on `camera_moved ||
order.changed`, and that gate sits *after* the frame may already have written geometry -- so a
frame emitted for any other reason wrote records and returned without a camera. The frame never
closed: the consumer had cleared its scene at `beginFrame` and its `endFrame` never ran.

Measured with `render_probe` and the consumer's own counters, forcing the map dirty every tick:
`renderables 0, primitives 0, missing_batches 0, lit_pixels 0 of 630000`. Nothing was built and
nothing was even looked for, which is what distinguishes "the draw list was empty" from "the draw
list never arrived". With the camera sent for any frame that wrote anything: `renderables 66,
lit_pixels 630000`.

The gate now asks the ring whether the frame wrote anything -- `producer.head()` against its value
when the frame opened -- which is the rule the other two conditions are special cases of. A parked
view writes nothing and stays silent, so §10's exit criterion is untouched.

### The fades: three attempts, three regressions, and what is actually known

Enabling them has now been tried three times and made the picture worse every time: black frames,
then text at half the colour the style asks for, then 26% gross at z14 with the text still wrong.
The camera-commit bug above was one real cause underneath it and fixing that did not make the
feature work.

So `tessella_advance`, `MapView::advance` and `Host::advance` exist and are unused, and the
extension does not pass the frame delta. A fade completes in one step, labels draw at full opacity,
and the captures agree with the oracle: darkest (16,15,14) against mbgl's (16,15,14) at z14 and
(47,45,42) against (47,45,42) at z16, at 1.46% and 0.56% gross.

What is worth carrying forward rather than re-deriving:

- The rate is right. `fade::increment` over `DEFAULT_TRANSITION_DURATION` is mbgl's arithmetic, and
  `mbgl-render` runs in static mode where `symbolFadeChange` returns one -- so instant fades are
  the correct thing to compare captures against and always were.
- Persisting the fades exposed a real defect that is now fixed: `write_opacity` un-hid labels whose
  road had run out, and they drew at their tile-unit anchor, thousands of pixels away.
- Something downstream still turns a running fade into a wrong picture, and it is not the camera
  commit and not the arena release. It wants a probe that can watch one label's opacity across
  frames rather than another attempt at switching the feature on.

### A probe that watches one label, and what it said first

`tessella_orchestrate::watch` records one line per watched label per frame, carrying every value
between the placement decision and the byte the shader reads: the frame, the label's text, its
cross-tile id, whether the line walk found room for it, the opacity the fade holds, and the
opacity actually written into the vertex, decoded back out of the packed value.
`TESSELLA_WATCH=Madison` follows every Madison Street; `TSF_FADES=1` on the consumer side turns the
fades on, so the two runs can be compared without rebuilding anything.

It exists because every previous attempt on this defect worked from *pictures*, and a picture says
the frame is wrong without saying which label, when, or which of the several values that decide a
glyph's opacity disagreed. Three explanations were shipped on that basis and two of them were
wrong.

**It answered its first question immediately.** With the fades running at z16, one label across
three emitted frames:

    frame=33  id=75  room=true  fade=0.389  vertex_opacity=0.386  vertex_placed=true
    frame=35  id=75  room=true  fade=0.833  vertex_opacity=0.827  vertex_placed=true
    frame=37  id=75  room=true  fade=1.000  vertex_opacity=1.000  vertex_placed=true

The identity is stable, the anchor is stable, the fade ramps and the vertex tracks it to three
decimals, and the label reaches full opacity. **The producer is correct.** And the captured frame
is (170,168,163) — an alpha of about 0.383, which is frame 33's value, not frame 37's.

So what is left is not the fade arithmetic, the identity, the placement or the opacity written:
it is that the picture shows an earlier frame's vertices than the last one emitted. That is the
consumer or the capture path, and it is the first time this defect has been narrowed to a side of
the wire rather than to a candidate mechanism. The next step is to compare what the producer last
announced against what the consumer last uploaded, at the moment the pixels are read.

### Both sides of the wire, and where the fades actually break

`TSF_WATCH_FADE` is the consumer's half of `TESSELLA_WATCH`: for every symbol geometry it
receives, the range of packed opacities across the whole buffer, decoded the way the producer
packs them. The range rather than one vertex, because a buffer whose labels sit at different
points of their fades is the expected picture and one that is uniformly a single value is a
buffer nobody rewrote.

Pointed at the same drawable, same run, z16:

    fades off   recv id=9  vertices=2556  opacity_min=0.000  opacity_max=1.000
    fades on    recv id=9  vertices=2556  opacity_min=0.000  opacity_max=0.386

and on the producer's side, the same run with the fades on, one label over the frames it appears
in: `0.389`, then `0.833`, then `1.000`, with the vertex tracking the fade to three decimals every
time.

So the producer writes 1.000 into the buffer and the consumer never receives a buffer holding more
than 0.386. Every receipt across the run caps there. **The frames carrying the finished fade are
emitted and not delivered.** That is the whole remaining question, and it is now a question about
delivery rather than about placement, identity, the fade arithmetic, the opacity written, or the
label plane -- each of which has been measured and is correct.

Worth noting how the first version of this measurement lied. It printed the *first vertex* of each
buffer, which belongs to whichever label happens to be first and is not the label the producer's
watch was following; the two numbers agreed at 0.386 for reasons that had nothing to do with each
other. Reporting the range instead is what made the two sides comparable. An instrument aimed at
the wrong quantity is worse than none, because it answers.

### Published and not received: the fade defect, stated exactly

`watch::frame_end` records how each frame ended, because `place_symbols` runs while a frame is
being *built* and a frame that then fails is aborted -- head stays where it was and the arena
rewinds -- so every record it wrote is discarded. Without that line a published frame and a thrown
away one look identical from the producer's side. The probe prints a `capture` marker into the same
stream, so emits and readbacks can be ordered against each other.

Interleaved, one run, z16, fades on:

    watch frame=40 id=75 room=true fade=0.389 vertex_opacity=0.386
    watch frame=42 id=75 room=true fade=0.833 vertex_opacity=0.827
    watch frame=43 id=75 room=true fade=1.000 vertex_opacity=1.000
    capture   (x6, no emit between any of them)

61 frames in the run, **zero aborted**. Every symbol buffer the consumer receives caps at
`opacity_max=0.386`; with the fades off the same drawable arrives at 1.000. And the picture is
(170,168,163), an alpha near 0.383.

So: the producer publishes a buffer holding 1.000, six consumer ticks follow with nothing else
emitted, and the consumer never receives a buffer above 0.386. Each of those is measured on its own
side and they cannot both be true of the same bytes. What is *not* the cause, each ruled out by
measurement rather than argument: placement, cross-tile identity, the fade arithmetic, the opacity
written into the vertex, the label plane, frame abort, a bounded drain, and the arena release a
re-announcement stages.

**Next instrument, and it follows the pattern that has worked twice now.** Log the slab reference --
offset and length -- on both sides: what the `GeometryAdd` names when it is written, and what the
consumer resolves it to when it reads. That separates "the record names different bytes" from "the
bytes were overwritten between publish and read", which are the only two shapes left. Everything
above was found by instrumenting both ends of a value and comparing; nothing was found by reasoning
about the code.

This thread has now cost far more than the defect is worth against its alternatives, and the fades
remain off. It is written down at this level of detail so the next attempt starts from the
measurement rather than from the beginning.

### The fade defect, narrowed to a publish that reports success

Three instruments, each added because the previous one could not tell two cases apart, and each
one wrong first in a way worth recording.

`watch::sent` logs the slab reference a geometry record names as it is written; `TSF_WATCH_FADE`
logs what the consumer resolves the same reference to. Pairing them looked like a swapped slab
index -- producer 10 against consumer 9 -- which would have been a fine, wrong conclusion: geometry
ids restart per map and every pane creates its map with the same view id, so lines from four panes
are unpairable and the "swap" was two panes' records laid side by side. `TSF_EXT_PANES=1` runs one
city and removes the ambiguity outright. Every measurement in this section was taken that way.

With one pane, one drawable, fades on:

    sent frame=13 id=15 slab=6 offset=556712 length=14848
    sent frame=14 id=15 slab=8 offset=556712 length=14848
    sent frame=15 id=15 slab=5 offset=556712 length=14848
    recv           id=15 slab=6 offset=556712 length=14848  opacity_max=0.386

The producer writes the drawable three times and the consumer receives the first. Across the run:
21 records sent, 7 received -- exactly one frame's worth of the seven geometries a frame carries.

And the reader is not behind. Its drain reports `cursor=831840 head=831840 records=0` at the end,
having consumed everything published, with 831840 the highest head ever seen. So the frames the
producer reports as `geometries=7 published=true` **do not advance the ring's head**. The records
are counted, the commit is reached, and nothing is published.

That is the defect: a publish that reports success and moves no head. It is on the producer side,
which is where it can be fixed, and it is a much smaller thing than "the fades are wrong".

The pattern across all of this is worth stating once. Every step forward came from instrumenting a
value at both ends and comparing; every step backwards came from reasoning about the code. Three of
the instruments answered confidently before they were aimed correctly -- a counter a reset could
hide, a first vertex that belonged to another label, and a slab index compared across panes -- and
each was caught only by asking what else could produce that number.

### A re-announced geometry was never applied

**Correcting the entry above.** It concluded that frames reporting `published=true` do not advance
the ring's head. That was read off the *idle tail* of the drain log, after the producer had stopped
emitting. Interleaved properly, head advances on exactly the frames that publish: 29168 to 300672
to 565312 to 829952 across the three frames in question. The records are published and drained.

What actually happens is one line in the reader. `GEOMETRY_ADD` does not deliver anything -- it
stores the record in `geometry_` and waits, because a geometry announcement is only half a
drawable and everything per-view arrives with a `ViewUse`. The two are joined when the *use*
arrives. And a use is durable: the producer sends one when a drawable enters a view's cover and
never again. So a geometry re-announced afterwards updated the map and was never joined, never
reached `onGeometry`, and never became a mesh.

The consequence is much wider than the fades. Symbol drawables are re-announced *every frame*,
because their vertices carry the camera -- that is what `write_line_positions` and `write_opacity`
put there. None of it has been reaching the consumer. Every label on screen has been drawn with the
line positions and opacities computed for the frame its tile was first announced in, which is
exactly "labels not anchored while the map moves", and it is why the per-frame re-announcement
landed earlier in this thread with no visible effect.

The reader now remembers the last use per geometry and re-joins on re-announcement, and drops it
again on release or removal so a recycled id cannot inherit another drawable's view, tile and draw
flags.

Measured: 21 records sent and 21 received where it was 21 and 7; the consumer receives the whole
fade, 0.386 then 0.827 then 1.000, where it saw only the first. Swept frame 280 -- the frame this
whole thread started from -- has its street names on their streets, with Broadway back on First
Hill. Settled parity is unchanged, 1.456% at z14 and 0.563% at z16 with the text exact.

The fades stay off: with them on, z16 is 0.660% against 0.563% but z14 is 24%, which is a separate
fault and not one to chase on the back of this.

### Fades on: the basemap disappears at z14, and it is not the re-join

With the fades running at z14 the frame is *labels on an empty background* -- no roads, no water,
no coastline, every street name correctly placed on nothing. That is the 24.275% gross, and it
reads 24.275% both before and after the re-join fix above, so it is a pre-existing fault in the
fades-on path rather than a consequence of it. z16 does not show it: 0.660% against 0.563% with
fades off.

Symbols are the only family re-announced per frame, so the basemap's geometry is announced once and
retained. For it to vanish, something must take it away -- and the candidate worth trying first is
the release a re-announcement stages. `record_refs` hands the previous frame's slab refs to
`retire`, which releases them and lets `sweep` free a slab whose live count reaches zero. If a slab
carries a symbol's bytes beside a fill's, and only the symbol's release is counted, the sweep frees
a slab the fill is still named against.

A caution from this session, since the same trap was fallen into twice: measure this with the gross
metric, not `magick compare -threshold`. The two disagree wildly on the same pair of images -- 0.2%
against 22.5% -- and the ImageMagick reading is what briefly made this look like it was not
happening at all.

### The slab-release theory is wrong, and the basemap is in the scene

The theory was that a re-announcement's staged release drops a slab's live count to zero on a
symbol's account while a fill still holds bytes in it, and `sweep` then frees the lot. The arena
documents the invariant that would catch this -- a slab's live bytes equal the lengths of every
reference still held into it -- so the check is now written where the sweep happens, behind
`TESSELLA_WATCH`. **Zero divergences, fades on or off.** The accounting holds; the theory is dead.

Four more things fell out, each narrowing rather than explaining:

- **Nothing is removed.** 324 frames with the fades running, `removed=0` on every one. The basemap
  is not being retired.
- **It is not the panes.** One pane loses the water exactly as four do, so it is not the shared
  scene or one renderer clearing another's entities.
- **It is not the producer.** `render_probe` drives tessella directly, and with `TSL_FADE_MS`
  advancing the fades from inside the tick it renders a complete frame: 28 renderables, 2034 glyph
  quads, 630000 of 630000 pixels lit, identical to fades off. Whatever this is, it is on the far
  side of the extension.
- **And the geometry is there.** The consumer reports **28 primitives in the scene with the fades on
  and 28 with them off** -- while the picture has no water at all and 913 road pixels. The basemap
  is present and not visible.

Present-and-invisible is a different defect from anything chased so far. It is not delivery, not
retention, not the arena: it is paint order, a uniform, or a blend. The viewport-covering
background is the first thing to look at -- it is one quad over the whole frame at a fixed
coordinate, drawn by paint order rather than depth, and a background that lands after the fills
instead of before them would leave exactly this: water and roads painted over, labels on top.

Note also that the map never settles with the fades running: 324 emitted frames against about
eighteen without. Whatever keeps `fading()` above zero forever is worth knowing on its own.

### The uniforms follow the frame, not only the camera — and the fades are on

Two theories died first and both were worth the measurement. The viewport background is not
hiding the basemap: forcing the per-tile path with `TSL_NO_VIEWPORT_BG` changes nothing, water
still zero, roads still 913 pixels. Nor is the slab release, per the accounting check above.

It is the uniform gate. `emit_group` writes each layer's consolidated uniform buffer under
`camera_moved || scene_changed || declare`, which is DR-8's camera-rate rule and is why a parked
view is silent. A frame emitted for a *third* reason -- a fade in flight is the one that found it --
announces geometry and refreshes none of the slots that geometry is drawn against. The consumer
rebuilds its scene from the order every frame and draws each drawable against its layer's uniform
slot, so it is handed geometry with no matrices to place it. Everything below the labels vanished:
present in the scene, 28 primitives either way, and invisible.

Exactly the shape of the camera-commit fix earlier in this thread, and the same test settles it:
`producer.head() != opened_at` -- did this frame write anything. A parked view still writes nothing
and stays silent.

**The label fades are on by default now**, after four attempts that each regressed the picture. With
them running: z14 darkest (16,15,14) against mbgl's (16,15,14) and 1.668% gross, where it was
24.275%; z16 unchanged at 0.563%; and the swept frame at z14.25 is complete -- water, roads, street
names on their streets, Broadway on First Hill.

`TSF_NO_FADES` turns them off, which is what a capture wants: `mbgl-render` runs in static map mode
where `symbolFadeChange` returns one, so instant fades are what a parity comparison is against, and
a settled probe that stops mid-fade reads a label at part of its colour. That is the whole of the
1.668% against 1.456% difference between the two modes.

Both defects found here -- the camera and the uniforms -- are the same mistake made twice: a gate
written for "the camera moved" standing in for "this frame has something to say". Anywhere else
that pattern appears is worth the same look.

### The same look, taken: there is no third gate

Three places read `camera_moved`. Two were the defects above. The third writes the frame-wide
`GlobalPaintParams` block under `camera_moved || declare`, and it is sound for a reason the other
two did not have: the consumer's `uniforms_` is never cleared, `declare` guarantees a first write,
and `camera_key` already carries `viewport: [width, height]`, so a resize is a move. A quiet frame
reads the last value written and it is still the right one. The two that broke were per *layer*,
where a layer meeting its first drawable on a quiet frame has no slot at all -- absent, not stale.

That is the distinction worth carrying: a durable slot whose key is fixed can be written lazily; a
durable slot whose key can appear for the first time on any frame cannot. By that test the rest of
the consumer's stores are already right. `meshes_`, `textures_` and the reader's `geometry_` are
fed by writes with no gate on them; `masks_` and `references_` come from `write_layer_state`, which
is inside the gate the fade defect fixed; the reader's `uses_` was the third instance of the shape
and was fixed earlier in this thread.

The sweep did turn up three constants in that frame-wide block that are not the values they name.

`pattern_atlas_texsize` was `[64.0, 64.0]` against an atlas of whatever size the sprites packed
into. Inert -- a fill's pattern texsize reaches the shader through the per-tile properties block,
and no material reads the frame-wide field -- but a value the wire claimed and did not have, which
is the kind of thing that costs a day when something finally reads it. It now carries
`patterns.size`, and `[0, 0]` when there is no atlas.

`pixel_ratio` is `1.0`, and this one is read: `line.mat` divides its antialiasing feather and its
blur by it. The consumer hardcodes `1.0f` for the line material too, with a comment naming the gap
-- "a host on a HiDPI display passes its scale and the feather narrows to match". Both halves are
placeholders for a device pixel ratio that nothing carries yet. Left alone deliberately: closing it
means a number on the FFI, the same wiring `WorldCopies` and `set_viewport` took, and it changes
nothing until a view runs at a scale other than one. `mbgl-render` captures at 1.0, so it is
invisible to the parity metric by construction.

`symbol_fade_change` is `0.0` and its doc says "zero until R2 has symbols to fade". R2's fades
shipped, per-symbol in the geometry rather than through this field, so the field is dead and the
comment is stale in a way that reads as unfinished work rather than a road not taken.

### Three pitched entries above do not reproduce, and the instrument is why

Re-measured with `mbgl-render` re-run at the *same* camera as the probe, ten runs per scene:

| scene | flat | pitch 45 |
| --- | --- | --- |
| `icons_only` | **0** | **2,029** (0.322%) |
| `one_poi-labels` | 0 | 450 (0.071%) |
| `families` | 18 (0.003%) | 967 (0.153%) |

Ten consecutive runs of the worst of those give 2,029 every time. So the icon scene is neither
flaky nor 34,000 gross, and "8 icons drawn against the oracle's 148" is not a thing this draws.

Two instrument faults account for the difference, and both were mine.

**The oracle was a different camera.** `icons_only_mb.png` in the scratchpad was captured at a
camera nobody wrote down, and the probe was pointed at one derived from the tile block. Comparing
those gives 41,355 gross and a confident story about pitch. Re-rendering the oracle at the probe's
own camera gives 2,029 for the same tessella frame -- the frame never changed.

**A material directory that will not load renders black, and nothing said so.** Filament resolves
Vulkan on this GPU as *mobile*; a `matc -p desktop` package is refused with "not built for mobile"
and a null material, which the loader dropped silently. The frame comes out entirely black, the
probe wrote its PPM and exited zero, and 630,000 gross -- recorded above as one run of a
distribution -- is what that looks like against any oracle at all. `-p desktop -p mobile` is the
fix; `materialsLoaded` and a non-zero exit are so the next one is not diagnosed twice.

This does not retire the *reasoning* in those entries -- `projectAndGetPerspectiveRatio` and
`findViewportPadding` are what mbgl does and are transcribed either way. It retires the numbers,
including the ones this document treated as symptoms to explain: the cover was always right, the
462 offers always matched, and there was nothing left over to find.

**What is not established.** These runs are warm: the pmtiles server has been up, and every run is
a fresh process against a hot page cache. The arrival-order entry's claim is about a race, and a
warm cache is exactly the condition that hides one. `TileSource::outstanding` still counts
`inflight` -- tiles *submitted* and not landed -- and still reports zero while `Readiness` is
`Resolving`, when the whole map is pending and nothing has been submitted at all. That hole is
real whether or not it is currently reachable, and it is the thing to close before the flakiness
is called gone.

**Closed.** `outstanding` counts `Resolving` now, and the test holds the manifest fetch on a
condvar rather than racing a window that is a network round trip -- which would pass on a slow day
and prove nothing on a fast one. It fails without the change.

### The parity table, re-measured on an instrument that reports itself

Every number below is one camera rendered by both renderers in the same command, with the oracle
produced at run time rather than read from disk, and with the probe refusing to continue unless
it reports `materials_loaded 12`. `TSF_NO_FADES`, which is the mode `mbgl-render`'s static map is.

| scene | flat | pitch 45 |
| --- | --- | --- |
| `icons_only` | 0 | 2,029 (0.322%) |
| `families` | 18 (0.003%) | 967 (0.153%) |
| `one_poi-labels` | 0 | 450 (0.071%) |
| `one_roads` | 0 | **0** |
| `one_place-labels` | 0 | **0** |
| `one_poi-dots` | 0 | 18 (0.003%) |
| `one_water` | 0 | 197 (0.031%) |
| `one_buildings` | 18 (0.003%) | 71 (0.011%) |

Seattle on the full planet style, z11 to z16:

| | z11 | z12 | z13 | z14 | z15 | z16 |
| --- | --- | --- | --- | --- | --- | --- |
| flat | 0.000% | **1.913%** | 0.529% | 0.128% | 0.010% | 0.000% |
| pitch 45 | 0.094% | 3.698% | 2.649% | 3.023% | 2.082% | 1.025% |

Two things in that are new and neither is noise -- four repeats each give the identical count.

**z12 flat is an outlier among flat frames**, at 1.913% where every other flat number on the sweep
is under 0.53% and most are zero. Whatever it is, it is a property of that level rather than of
pitch.

**Pitch costs an order of magnitude more on the full style than on any single-family scene.** The
worst single-family frame is 0.322%; the planet sweep is 1 to 3.7% at every level from z12 up. So
the pitched gap is real, it is not the icon defect that was written down, and it only appears when
many families are drawn together -- which is where to start looking, rather than in any one of
them.

### The settle is deterministic, including when tiles arrive late and unevenly

A forwarding proxy in front of both origins delays every response by a uniform 0.8 to 3.5 seconds,
which takes a run from 1.26s to 5.27s and spreads arrivals across many ticks. Thirty-six runs --
three scenes, flat and pitched, six each -- give the identical gross count every time, and the
same count the fast path gives.

So the arrival-order entry's flakiness is gone, and it was gone before this session: settling on
the *image* is what fixed it. Worth stating because the `outstanding` change above did not do it.
Reverting that change and re-running the same six slow pitched runs gives 2,029 six times as well
-- the probe does not depend on `outstanding` alone, so the hole was real in the code and no longer
reachable through this harness. It still matters to a consumer reading that number for a progress
indicator, which is what it is for.

### The quad on a screen, confirmed by eye

Four panes under `ivi-homescreen` on `wayland-0`, all four granted dma-buf slots, the tile server
serving z0 through z15 across four cameras, no errors. Joel's read of it: *"text looks much
better"* -- which is the thread that started as "labels are flying around, not anchored" and ran
through the re-announce, the camera commit, the uniform gate and the fades.

That is the confirmation this side could not produce. The compositor has no screen-capture
protocol and the shell's own Vulkan capture writes black until fluorite #425 lands, so everything
measurable from here -- request logs, granted buffers, absence of errors -- says the panes are
*running*, and none of it says what they look like.

Two parity gaps found in the same day's re-measurement are still open and are not visible at this
zoom: z12 flat at 1.913%, and pitch costing 1 to 3.7% on the full style where the worst
single-family scene is 0.322%.

### One identity counter per view, and z12 flat goes to zero

The z12 flat outlier was symbols: with the symbol layers stripped it is 0 gross, and with *either*
symbol layer alone it is also 0. Only both together differ. That shape -- correct apart, wrong
combined -- is the whole diagnosis.

`ViewSymbols` keys a label's fade and its orientation by the cross-tile identity alone, and every
layer's `CrossTileIndex` numbered from a `next_id` of its own. So the first road label and the
first place label were both identity 1, and a road label read a place label's decision. mbgl has
one `maxCrossTileID` on `CrossTileSymbolIndex` and hands each `CrossTileSymbolLayerIndex` a
reference to it; the per-layer split is about *matching*, never about numbering.

On screen it was unmistakable once cropped: Elliott Avenue, 2nd Avenue, Stewart Street and Pine
Street stacked through "Belltown", and three ferry routes over each other in Elliott Bay, where
mbgl draws three road names in the same square and no ferry pile.

| Seattle, flat | z11 | z12 | z13 | z14 | z15 | z16 |
| --- | --- | --- | --- | --- | --- | --- |
| before | 0.000% | 1.913% | 0.529% | 0.128% | 0.010% | 0.000% |
| after | 0.000% | **0.000%** | **0.000%** | 0.011% | 0.010% | 0.000% |

`families` at pitch 45 goes 967 to 135 for the same reason. Nothing else moved.

### The pitched gap is road labels, and it is not the perspective ratio

Split the same way at z14 pitch 45: no symbols is 13 gross, place labels alone 13, **road labels
alone 19,747**. So it is line labels at pitch, inside one layer, and the identity fix barely
touched it (3.023% to 2.975%).

Two things checked and *not* the cause, so the next person does not check them again. The
collision ratio is `0.5 + 0.5 * cameraToCenterDistance / w`, which is
`CollisionIndex::projectAndGetPerspectiveRatio` to the character, and mbgl uses it for collision
boundaries only -- "we need to scale down boxes in the distance". And `symbol_sdf.mat` computes
`clamp(0.5 + 0.5 * distance_ratio, 0.0, 4.0)` with the viewport-aligned branch dividing, which is
mbgl's vertex shader transcribed. Both halves are faithful.

What is left is what a pitched frame does to the collision *area* rather than to any one box:
`viewport_padding`, the grid's size, and which anchors along a line are offered at all.

### The pitched road labels: located, and the term we do not have

Measured, not guessed. At Seattle z14 the road-label layer alone is 71 gross flat and 19,746 at
pitch 45. The excess is text we draw where mbgl draws background, it is 55% concentrated in the
top three tenths of the frame -- the far field -- and we put 20% more text pixels on screen than
mbgl does at pitch against 1% more flat.

Two candidates ruled out by reading rather than measurement, recorded so nobody re-reads them:

- **Viewport padding matches.** `viewport_padding` is 100 flat and 200 pitched and the grid is the
  viewport plus twice it, which is `findViewportPadding` and the `CollisionIndex` constructor to
  the constant.
- **The perspective ratio and the shader match.** `0.5 + 0.5 * cameraToCenterDistance / w` is
  `projectAndGetPerspectiveRatio`, used for collision boundaries only, and `symbol_sdf.mat`'s
  `clamp(0.5 + 0.5 * distance_ratio, 0.0, 4.0)` is mbgl's vertex shader.

What we do not have is `CollisionIndex::approximateTileDistance`. mbgl decides which of a line
label's collision circles are tested by comparing each circle's signed distance against
`-firstTileDistance ..= lastTileDistance`, and those two are not the glyph offsets: they come from
walking the line to the outermost glyphs and then

    prevTileDistance + lastSegmentTile
      + (incidenceStretch - 1) * lastSegmentTile * |sin(lastSegmentAngle)|

with `incidenceStretch = pitchWithMap ? 1 : cameraToAnchorDistance / pitchFactor` and
`pitchFactor = cos(pitch) * cameraToCenterDistance`. It is a *pitch-only* correction -- the whole
term vanishes at pitch zero, which is exactly the shape of the defect -- and it exists because a
label drawn perpendicular to the viewport covers more ground on an oblique tile than a flat one.

Ours is `|glyph_offset * font_scale * perspective|` for each end: the right quantity from the
wrong source. mbgl's `placeFirstAndLastGlyph` walks the line and returns a `TileDistance` per end
carrying `prevTileDistance` and `lastSegmentViewportDistance` plus the segment angle; ours takes
the unwalked offset and scales it. Flat the two agree closely enough to score 71 gross, and at
pitch they do not.

That caution was wrong and is struck: `projectAnchor` returns `first` = the perspective ratio and
`second` = `p[3]`, so the `cameraToAnchorDistance` handed to `approximateTileDistance` is a real
distance after all. `incidenceStretch` is therefore at least one and grows with distance, and the
term *adds* -- a wider window, more circles tested, fewer far-field labels surviving. Which is the
direction the defect needs. The two elements of that pair are a ratio and a distance and it is
worth reading which is which before trusting either.

### The incidence term is not it, and that was worth finding out

`approximateTileDistance` was implemented and measured, and it does not fix the pitched road
labels. Recorded because the reasoning for it is good and somebody will reach for it again.

What was built: `PlacedGlyph` carrying `lastSegmentViewportDistance`, the first and last glyph
walked onto the projected line as `placeFirstAndLastGlyph` does, `pitchFactor` and the camera
distance on `FrameOptions`, and the reach widened by
`(incidenceStretch - 1) * lastSegment * |sin(segmentAngle)|` with the anchor's `w` recovered from
the perspective ratio it already carries.

It behaves as designed and it is the wrong lever: z14 pitch 45 went 19,746 to **20,345**, and flat
stayed at 71 -- so the term is live, pitch-only, and pointing the wrong way by about three percent.
Dropping the `* perspective` from the base reach as well, on the grounds that mbgl's walked
distance carries no such factor, gives **20,668**. Two changes, both small and both the wrong way,
which is what a wrong model looks like rather than one needing tuning. Reverted.

`covered_by_label` was checked against mbgl on the way past and its sense is right: a circle
outside the reach is skipped and clears `previousCirclePlaced`, first test in the loop, as in
`placeLineFeature`.

So the far-field excess is upstream of which circles get tested.

**Fades are ruled out too, and cheaply.** `render_probe` never calls `advance`, and
`PlacementState::new` leaves `increment` at one, so every parity number on this page was measured
with instant fades: a label that fails to place drops to zero the same frame. A fade held open
cannot be what draws the extra text.

That is four candidates eliminated with measurements -- the incidence term, `covered_by_label`'s
sense, the viewport padding, the perspective ratio and shader -- and the useful thing to say about
the fifth is that source reading has stopped paying. Both sides hide labels at pitch and mbgl
hides more: ours goes 9,919 text pixels flat to 8,121 pitched, mbgl 9,859 to 6,765.

The next step is the technique that has worked every other time here and has not been applied to
this: print the same intermediate from both renderers and diff it. Concretely, one line per symbol
at this camera -- anchor in tile units, text, placed or not -- out of mbgl's `Placement::placeBucket`
and out of `place_symbols`, sorted and compared. That names the disagreeing labels instead of
narrowing the space of mechanisms one revert at a time.

### Both ends printed and diffed, and two rules were missing

The technique that was overdue. `mbgl-render` patched at `Placement::placeSymbol` and this side at
`place_symbols`, each writing one line per symbol -- anchor in tile units, placed or not -- at
Seattle z14 pitch 45 on the road-label layer alone.

The first thing it said is what *is not* wrong. Both offer **388 symbols at the same 388 anchors,
in the same order**: every position matches, median rank shift zero. So layout, anchor generation
and placement order were never the problem, and the four mechanisms eliminated before this were
eliminated for nothing more than being adjacent to the real one. mbgl placed 167 and this 211.

Patching mbgl again to say *why* it refused split the eighty disagreements cleanly:

| | count |
| --- | --- |
| ours placed, mbgl `hitTest` | 46 |
| ours placed, mbgl `notInGrid` | 16 |
| mbgl placed, ours not | 18 |

Two rules were missing, and the second is the interesting one.

**`isInsideGrid`.** A label with no part inside the padded viewport is not placed, whatever the
grid holds. There was no such concept here.

**An empty circle run is not placeable.** A label whose road runs out before a run can be built is
offered as `Shape::Circles(vec![])` rather than `None`, deliberately, so that `text_optional` and
`icon_optional` still see text -- that is what `5d87a53` fixed. But an empty run collides with
nothing, so `place` placed every one of them unconditionally: every road too short to carry its own
name kept its name. mbgl returns unplaced when `placeFirstAndLastGlyph` gives it nothing. The two
meanings are `placeable` and `in_grid` now instead of one accident, and the test that guarded the
first has been rewritten to assert both halves rather than only the half that was broken then.

| Seattle | z12 | z13 | z14 | z15 | z16 |
| --- | --- | --- | --- | --- | --- |
| flat, before | 0.000% | 0.000% | 0.011% | 0.010% | 0.000% |
| flat, after | 0.000% | 0.000% | **0.000%** | 0.010% | 0.000% |
| pitch 45, before | 3.698% | 2.649% | 3.023% | 2.082% | 1.025% |
| pitch 45, after | **0.998%** | **1.621%** | **2.576%** | 2.023% | 1.006% |

The 46 `hitTest` disagreements are what is left: mbgl finds a collision where this does not, with
the same symbols in the same order. That is now the whole of the pitched gap and it is a question
about one comparison rather than about the shape of the pass.

### The empty runs, and an overshoot worth knowing about

Counting circles on both sides at the same camera, after the two rules above landed:

| | ours | mbgl |
| --- | --- | --- |
| placed, of 388 | **141** | 167 |
| symbols with *no* collision run | **67** | 6 |

So the fix overshoots. Before it this placed 211 against mbgl's 167 and the error was all
over-drawing; now it places 141 and 47 of the misses are labels mbgl draws. The parity numbers
still improve at every level because over-drawing was much the larger error, but "closer" is not
"right" and the shape of the remaining error has flipped.

The cause is one number: **67 empty runs against six.** A label with no run is not placeable, which
is mbgl's rule and is correct; the defect is that this side fails to build a run for sixty-one
labels where mbgl builds one, and mbgl places twenty-nine of those. Every other statistic follows
from it -- the circles offered differ on 207 of the 321 symbols where both sides do build a run.

Why the two differ is structural rather than a constant to fix. mbgl lays its collision circles out
**once, in tile units**, at layout time -- `feature.boxes` is built by `CollisionFeature` and
projected per frame. This builds them per frame from the *projected* line, so a road foreshortened
by a pitched camera has no room for a run at all, and the label loses its run rather than its
circles being small. That is exactly why the counts agree flat and diverge at pitch.

So the next piece is not a comparison to correct but a place to move work: the run belongs in tile
units beside the anchors, projected per frame like everything else, rather than rebuilt in screen
space each time.

### The run moves into tile units, and the placements nearly agree

The structural fix the last entry called for. `collision_circles` is walked against the tile's own
line now and each circle projected afterwards -- centre through the same projection every anchor
takes, radius by the ratio that built it -- rather than walking a line already flattened by the
camera.

| at Seattle z14 pitch 45 | before | after | mbgl |
| --- | --- | --- | --- |
| labels with no run | 67 | **7** | 6 |
| placed | 141 | **179** | 167 |
| placement disagreements | 80 | **28** | -- |

The remaining 28 are fourteen where mbgl finds a collision this misses, six outside its grid, and
eight the other way. Where both build a run -- 381 of 388 now, against 321 - the circle counts
agree on 192.

Pitched parity improves at every level, z12 0.998% to 0.908% through z16 1.006% to 0.958%, and
z14 2.576% to 2.453%.

**And that is the interesting part.** Twenty-eight disagreements out of 388 cannot account for
2.45% of a frame. The placement decisions now nearly agree while the picture still differs, so what
is left is mostly *not* which labels are drawn -- it is where their glyphs land once drawn. The
next look belongs in the vertex path at pitch, not in placement: `write_line_positions` and what
the shader does with the dynamic buffer it fills.

### The pitched remainder is glyph size, not placement -- *wrong, see below*

Cropped tight on one label drawn by both at the same anchor -- "Harrison Street" at Seattle z14
pitch 45 -- mbgl's glyphs are plainly larger than this one's. Same label, same position, different
size, and flat is pixel-identical. So the remainder is the vertex path, which the arithmetic
already implied: twenty-eight placement disagreements out of 388 cannot make 2.45% of a frame.

Both sides shrink distant type by the same rule, `clamp(0.5 + 0.5 * distance_ratio, 0, 4)` with the
viewport-aligned branch dividing `cameraToCenterDistance` by the anchor's `w`. A *smaller* ratio
here means a larger `w`, so the inputs are where to look, and two of them are already ruled out:

- `camera_to_center_distance` is `0.5 * height / tan(fov / 2)`, character for character
  `TransformState::getCameraToCenterDistance`, with `DEFAULT_FOV` the same constant.
- `a_pos` is the tile-unit anchor and `matrix` is tile-local-to-clip on both sides, which is what
  mbgl's `u_matrix * vec4(a_pos, 0, 1)` takes `w` from.

So the ratio's two named inputs agree and the output does not, which means the next measurement is
of the value itself rather than of the code around it: the ratio, or `w`, printed per label from
both renderers at one camera. Reading the two shaders side by side has now failed twice to explain
a difference that a single printed number would settle.

### The advance scale mbgl has, why it is not enough, and what it proves

mbgl scales the font size used to *walk* a line label, in `reprojectLineLabels`:

    perspectiveRatio    = 0.5 + 0.5 * cameraToAnchorDistance / cameraToCenterDistance
    pitchScaledFontSize = pitchWithMap ? fontSize * ratio : fontSize / ratio

That ratio is the shader's in the *other* orientation -- at least one, growing with distance -- so a
viewport-aligned label walks its line with a smaller size the further off it sits, and the shader's
`size *= perspective_ratio` shrinks the quads to match. There is no equivalent here: the walk uses
the plain size.

Adding it does not help, and the way it fails is the useful part. With it the label's advance
tightens and its glyphs do not change height at all -- "Harrison Street" goes from 101 pixels wide
to narrower, still out of the same small type, while mbgl draws it 114 wide out of visibly larger
type. Gross at z14 pitch 45 goes 16,900 to 20,456, because a tighter walk makes more labels fit
their roads and more of them are drawn. Reverted.

What that proves is worth more than the change. mbgl is larger in *both* dimensions and this is
smaller in both while being relatively wider between glyphs, so the two differ by an overall scale
and not by an advance. The walk term is real and belongs here -- but only after the scale is right,
because it can only be judged against a label that is already the correct size.

So the open question is unchanged and now sharper: the shader's `perspective_ratio` comes out
smaller here than in mbgl, its two named inputs agree, and the next measurement is the value
itself. Printing `w` per label from both at one camera settles it; three attempts at reading the
two shaders side by side have not.

### The ratio is right, and the size entry above was a bad read

Printed `w` and the perspective ratio per label from both renderers at Seattle z14 pitch 45 and
joined on the tile anchor. Across all 166 anchors mbgl draws, the two ratios agree to **5e-5**:
median difference 2e-5, worst 5e-5, none over 0.01. The ratio is not the defect and neither are its
inputs.

So "mbgl's glyphs are plainly larger" was wrong, and the way it was wrong is worth keeping. The
crop compared *"Harrison Street" to "Harrison Street"* -- but a road name appears at many anchors,
and the two renderers had drawn different instances of it, at different distances, therefore at
legitimately different sizes. Comparing a label to a label of the same name is not comparing like
with like, and nothing in the picture says which anchor a given piece of text came from.

Two instrument faults on the way to that, both caught by the numbers disagreeing with themselves:
`frame_labels` is called twice per bucket -- once for placement with the real projection, once for
opacity with `|_| 1.0` -- so a dump keyed by anchor and read last-write-wins reported a ratio of
exactly one for 161 of 166 anchors and looked like a spectacular defect. Taking the first pass
gives the agreement above. Anything that instruments `frame_labels` has to say which pass it means.

**Where that leaves the pitched gap.** The ratio agrees, placement agrees on 360 of 388, and the
frame still differs by 2.45%. Twenty-eight whole labels is on the order of a thousand text pixels
against sixteen thousand gross, so the remainder is most likely neither: it is the *positions*
agreed-on labels are drawn at. That is `write_line_positions` against `reprojectLineLabels`, and
the one term known to be missing there is the `pitchScaledFontSize` above -- which was reverted for
making gross worse, on the strength of a size argument that has just been withdrawn. It deserves a
second look now that the scale is known to be right, measured by how far agreed labels move rather
than by gross alone.

### The pitched gap was the walk's font size, and it is closed

Printing each glyph's label-plane position from both renderers -- anchor, index, x, y, angle --
found it in one measurement. At the first shared anchor mbgl's glyphs sat 6.47 apart and this one's
5.25: a ratio of 1.232, which is that anchor's `perspectiveRatio` to three decimals.

mbgl walks a line label with `pitchScaledFontSize`, not with the font size:

    pitchScaledFontSize = pitchWithMap ? fontSize * perspectiveRatio : fontSize / perspectiveRatio

and the along-line path is the *multiplying* branch, because along-line implies
`*-rotation-alignment: map` and `*-pitch-alignment` inherits it. A label lying on the ground covers
more ground the further off it is, so its glyphs step further apart in the plane to land the same
distance apart on screen. This walked at the near-field size wherever the label was.

The division was tried first, two entries ago, from the viewport branch -- and reverted for making
the picture worse, which it did. The branch was wrong, not the term.

With it the walks agree: of the 2,012 glyphs mbgl writes, the median position difference is
**0.000** and the first label matches to the printed digit, angle included. 421 still differ by
more than half a pixel, which is the labels the two place at different anchors.

| Seattle, pitch 45 | z12 | z13 | z14 | z15 | z16 |
| --- | --- | --- | --- | --- | --- |
| before | 0.908% | 1.485% | 2.453% | 1.831% | 0.958% |
| after | **0.259%** | **0.404%** | **0.781%** | **0.191%** | **0.007%** |

The road-label layer alone at z14 goes 2.683% to 1.065%. Flat does not move -- the ratio is one at
pitch zero -- and `icons_only`, `families` and `one_poi-labels` are unchanged.

What is left of the pitched frame is under a percent everywhere and is the 28 placement
disagreements plus whatever those 421 glyphs are: the labels the two renderers put at different
anchors, which is a placement question again rather than a projection one.

### What is left of pitch, counted properly

Re-diffed after the walk fix, joined on the tile anchor rather than by row -- the row counts no
longer match, and pairing by position gave a nonsense 165 disagreements before that was noticed.

| at Seattle z14 pitch 45 | |
| --- | --- |
| anchors mbgl offers placement | 388 |
| anchors this offers | **350** |
| placed: this / mbgl | 171 / 167 |
| disagreements on the 350 shared | **31** (18 `hitTest`, 7 mbgl-places, 6 `notInGrid`) |
| mbgl anchors never offered here | **38**, of which mbgl places 13 |

The 38 are the larger half and the walk fix is what exposed them. `write_line_positions` runs
before `frame_in` and removes from the offered set every label whose *whole* walk fails; stepping
by the pitch-scaled size makes a label need more road, so more of them fail. mbgl does not gate
placement on the whole walk: `placeLineFeature` asks `placeFirstAndLastGlyph` for the two outermost
glyphs only, and a label whose middle will not fit still competes and is hidden later by
`placeGlyphsAlongLine` returning `NotEnoughRoom`. So mbgl offers all 388 and draws 13 that this
never offers.

That is the next piece, and it is an ordering question rather than an arithmetic one: the fit test
belongs after the competition, on the first and last glyph, not before it on all of them.

### Every label competes now, and the pitched frame is under 0.7%

The 38 anchors this was not offering were not a stricter *gate* -- gating on the outermost glyphs,
which is `placeFirstAndLastGlyph`, gives numbers indistinguishable from gating on the whole walk,
because when the whole walk fails an outermost glyph is what failed. They were an ordering: mbgl
lets every symbol reach `placeSymbol` and hides the unwalkable ones afterwards, and this filtered
them out before the competition.

| Seattle, pitch 45 | z12 | z13 | z14 | z15 | z16 |
| --- | --- | --- | --- | --- | --- |
| before the walk fix | 0.908% | 1.485% | 2.453% | 1.831% | 0.958% |
| walk fixed | 0.259% | 0.404% | 0.781% | 0.191% | 0.007% |
| and every label competing | 0.259% | **0.347%** | **0.694%** | **0.173%** | 0.007% |

Flat is unchanged at zero but for 63 pixels at z15, and `icons_only`, `families` and
`one_poi-labels` do not move.

Worth being plain about what this last step is not: it is a *removal*, and the comment it removes
argued the opposite -- that a label which will not be drawn must not hold space against one that
will. That argument was sound and the conclusion was still wrong, because mbgl holds the space too.
The thing that makes it safe is `Shape::placeable` refusing a label with no run, which did not
exist when the filter was written.

### Where parity stands, and what the icon scene has left

With pitch under 0.7% everywhere, the largest single-family gap is now `icons_only` at 0.322%,
which is the *point*-symbol path and untouched by any of the line-label work above.

Diffed the same way, final decision per anchor: **254 anchors on both sides, all shared**. Layout
and anchor generation agree exactly. This places 146 icons against mbgl's 150, and the two disagree
about **eight** -- two this places and mbgl does not, six the other way. Eight icons at icon size is
about two thousand gross pixels, which is the whole of the 2,029.

So the icon scene is eight collision decisions from exact, and nothing structural is left in it.

**A note on the instrument, because this is the fourth time.** The first pass at that comparison
reported 327 anchors against 254 and 144 disagreements, all of it wrong: the dump is written once
per `frame_in` call, `frame_in` runs per bucket per frame, and a naive read of the file mixes
several frames of the same buckets together. Keyed by anchor and read last-write-wins it gives the
254 above. Every join in this document that has gone wrong has gone wrong the same way -- a key
that stopped identifying one thing - and the fix each time was to make the dump say which pass,
which bucket, or which frame it came from.

### The padding was not scaling, and fixing it exposes the hit test

The icon scene's eight disagreements were one number. Dumping the projected box from both
renderers for each of them: every one of ours was about six tenths of a pixel wider and taller than
mbgl's, three tenths on each edge, in all eight and in the same direction.

mbgl adds padding in tile units -- `y1 = top * boxScale - padding.top` in `CollisionFeature` -- and
multiplies the whole box by `tileToViewport` when it tests. So its two pixels of padding become two
times the perspective ratio in the distance. This held padding at a flat two screen pixels wherever
the symbol was. With it scaled the boxes agree to **0.000** and the eight disagreements become
none; `icons_only` at pitch 45 halves, 0.322% to 0.146%, and the full style reaches zero at z16.

**And it costs something, which is the useful part.** Oversized boxes collide more often, and that
was masking a hit test that finds *fewer* collisions than mbgl's. With the boxes identical this
places 194 road labels at Seattle z15 pitch 45 where mbgl places 185, and fourteen of the twenty-one
remaining disagreements are collisions mbgl finds and this does not. The full style goes 0.347% to
0.366% at z13, 0.694% to 0.715% at z14 and 0.173% to 0.459% at z15 -- worse numbers out of a
provably more correct box.

That is the trade taken deliberately: a measured-equal box with a known-weak test beats a wrong box
whose error happened to cancel. The anchor sets now match exactly too -- 360 against 360 at z15,
where before the ordering fix they were 350 against 388 -- so what is left in the pitched frame is
one thing: `Shape::collides` against `collisionGrid.hitTest`, on boxes that are now identical.

### Not the hit test: the reach window

The last entry pointed at `Shape::collides` against `collisionGrid.hitTest`. It is not that.
Counting circles on both sides for the 360 road labels at Seattle z15 pitch 45, with the boxes now
identical:

| | agreeing |
| --- | --- |
| circles in the run | 195 of 360 |
| circles inside the label's reach | 236 of 360 |

The run sizes differ by one either way and nothing more -- 195 exact, the rest ±1 to ±3 -- which is
rounding in the walk that lays them out. The reach is the real difference, and it is one-sided:
where the two disagree this side almost always includes *more* circles, by one to seventeen.

Which is `approximateTileDistance` again. mbgl decides which circles a label covers by walking the
line to its outermost glyphs and converting through `pixelsToTileUnits` with the incidence term;
this still uses `|glyph_offset * font_scale * perspective|` converted to tile units, the right
quantity from the wrong source, as the earlier entry said. That entry tried the term, took it from
the *viewport* branch, measured it worse and reverted it -- and the walk it was meant to match was
itself wrong at the time, since `pitchScaledFontSize` had not been found yet.

So it is worth a third attempt, on a foundation that has since changed twice: the walk is now
mbgl's, the boxes agree to 0.000, the anchor sets match 360 to 360, and the run sizes agree on
195. The measurement to hold it to is the in-reach count above, not gross.

**And on the instruments.** Two of the three numbers in this entry were wrong before they were
right. mbgl's circle counter increments after the reach test but *before* thinning, and the first
version of this side's counted the thinned survivors -- so "tested equal on 52 of 360" compared two
different quantities. That is the fifth join or counter in this document to compare unlike things,
and every one was caught the same way: a number that did not fit the story it was supposed to tell.

### The run belonged at layout scale, and that was the reach problem too

Three readings of the reach were measured against the count of circles each label covers, on the
360 road labels at Seattle z15 pitch 45:

| reach scaled by | labels agreeing |
| --- | --- |
| the shader's ratio (what was there) | 236 |
| the walk's ratio | 92 |
| neither | 132 |

Monotone, and none of them good -- which is what a wrongly-posed question looks like. The reach was
not the thing.

`CollisionFeature` builds a line label's circles **once per bucket, in tile units**, and the camera
reaches them only as `tileToViewport` when one is projected to be tested. This folded the
perspective ratio into the run itself, so every circle's `signedDistanceFromAnchor` moved with the
camera -- and the reach window those distances are compared against moved *against* them rather
than with them. No scaling of the window can fix a window and a ruler that disagree.

With the run built at `font_scale`, padding added in tile units as `CollisionFeature` adds it, and
the ratio applied to the radius on projection:

| | before | after |
| --- | --- | --- |
| circle runs matching mbgl's | 195 of 360 | **360 of 360** |
| in-reach counts agreeing | 236 | 246 |

and the reach then wants no ratio at all, which is what mbgl does:
`approximateTileDistance` reduces to `prevTileDistance + lastSegmentTile` for a label pitched with
the map, and the walk it measures is handed `fontSize / 24`, not the pitch-scaled size the drawing
walk uses. Two walks at two sizes, which is the thing this thread kept conflating.

| Seattle, pitch 45 | z12 | z13 | z14 | z15 | z16 |
| --- | --- | --- | --- | --- | --- |
| four turns ago | 0.908% | 1.485% | 2.453% | 1.831% | 0.958% |
| now | **0.254%** | **0.342%** | **0.453%** | **0.141%** | **0.000%** |

Flat is unchanged and no other scene moves. What is left is 114 labels whose in-reach count still
differs and the run-size rounding that is now gone -- and the honest note is that gross was the
wrong instrument for all three reach experiments: it went 0.404%, 1.034%, 0.383% across readings
whose real quality was 236, 92 and 132. The count is what discriminated.

### The icon scene reaches zero, on a sampler

Its placements had agreed exactly since the padding fix, and the 919 pixels left at Seattle z15
pitch 45 were all of one kind: ours at either the icon's flat colour or the background's, mbgl's a
blend of the two. Hard edges against antialiased ones.

mbgl picks an icon's filter with `sdfIcons || isChanging || iconScaled || iconTransformed`, and
`iconTransformed` is `rotationAlignment == Map || pitch != 0`. This tested `iconScaled` only, so a
pitched icon -- being resampled however the style sizes it -- was still drawn nearest.

The single-family table at z15, pitch 45, after:

| scene | flat | pitch 45 |
| --- | --- | --- |
| `icons_only` | 0 | **0** |
| `one_poi-labels` | 0 | **0** |
| `one_place-labels`, `one_roads`, `one_parks`, `one_imagery` | 0 | 0 |
| `one_poi-dots` | 0 | 18 |
| `one_buildings` | 18 | 71 |
| `one_water` | 0 | 197 |
| `families` | 18 | 110 |

Seven of eleven scenes are pixel-exact at both angles. What is left is not symbols: `one_water` and
`one_buildings` are fills, and they are the two largest numbers on the page now.

### The fills are at the rasterisation floor, not carrying a defect

`one_water` was the largest number left at 0.031%, and it is not extra water. Its 197 differing
pixels form **170 runs, of which 143 are a single pixel and none is longer than two**: isolated
specks along the coastline, not a region. `one_buildings` is 41 runs with a maximum of four,
`families` 70 runs with a maximum of four. All three are edges.

Nor is it antialiasing. Where the two differ, this side is the fill's flat colour and mbgl's is the
background's -- both pure, neither blended -- so the polygon edge simply lands on the other side of
a pixel centre. One pixel of coverage, on boundaries thousands of pixels long.

Two things were checked against mbgl and ruled out on the way, because the far-field concentration
looked like a horizon problem: `tanFovAboveCenter` is `tan(fov/2)` on both sides once offset and
roll are zero, and the far plane is `cameraToSeaLevelDistance / (1 - tanMultiple) * 1.01` on both.
This version of mbgl has no horizon clamp to be missing.

So there is no fill defect to chase. Seven of eleven single-family scenes are pixel-exact at both
angles and the other four differ only along edges, which is where two independent rasterisers stop
agreeing.

### Placement is exact, and the remainder is the flip

Decomposing the full style at Seattle z14 pitch 45: no symbols is 13 gross, place labels alone 13,
**road labels alone 4,084** -- and the full style is 2,857, *lower* than road labels by themselves,
because place names suppress some of them. One layer carries all of it.

`Shape::in_grid` was scanning every circle of a run where mbgl accumulates `inGrid` inside the loop
that skips out-of-reach and thinned circles -- a circle it never tests never reports itself in the
grid. Six labels were placed here for that reason alone. Restricted to the tested set, the
road-label layer's placements are **exact**: 388 anchors on both sides, run sizes matching on all
388, 167 placed against 167, zero disagreements.

**And the picture is unchanged at 4,084.** Identical decisions, different pixels -- so what is left
is where the placed labels are drawn.

Dumping every glyph's position says it precisely: of 2,012 glyphs the median difference is 0.000
and 421 differ, across **33 labels of 153**, and their angles differ by almost exactly π (267
glyphs) or 3π (142). Those labels are drawn in the opposite direction.

It is the keep-upright test. Both sides ask the same question -- is the first glyph to the right of
the last, `firstPoint.x > lastPoint.x` -- but mbgl asks it of points **projected to screen**:

    const Point<float> firstPoint = project(firstAndLastGlyph->first.point, glCoordMatrix).first;
    const Point<float> lastPoint  = project(firstAndLastGlyph->second.point, glCoordMatrix).first;

and this asks it of the label-plane points directly. For a label pitched with the map the label
plane is the tile's, and the perspective divide between there and the screen can reorder two points
in x. Flat it cannot, which is why the flat frame is exact and only the pitched one is not.

So the fix is to project the two end points through the label plane's own `getGlCoordMatrix` before
comparing them, which needs that matrix threaded into `place_upright` -- the one place it is not
already available.

### The upright test, and the pitched frame closes

The last entry named it and it holds. `place_upright` decides the flip where the projection is now,
not inside the walk, and it asks mbgl's question in mbgl's space.

| Seattle | z12 | z13 | z14 | z15 | z16 |
| --- | --- | --- | --- | --- | --- |
| pitch 45, before | 0.254% | 0.342% | 0.453% | 0.141% | 0.000% |
| pitch 45, after | **0.018%** | **0.023%** | **0.002%** | **0.012%** | **0.000%** |
| flat | 0.000% | 0.000% | 0.000% | 0.010% | 0.000% |

The road-label layer alone at z14 pitch 45 goes 4,084 gross to **13**. Every remaining number on
this page is now edge noise of the kind measured two entries ago: single pixels along a boundary,
pure colour against pure colour.

Worth noting what the sequence looked like from inside, because it did not look like progress at
the time. Placement was made exact -- 388 anchors, 167 against 167, zero disagreements -- and the
frame did not move at all, 4,084 pixels before and after. That is the measurement that mattered: it
proved the remaining error was not *which* labels are drawn, and pointed at the glyph dump, which
named 33 backwards labels in one pass. A decision that changes no pixels is not a wasted one.

**The parity thread is done.** Flat is exact but for 63 pixels at z15 across the whole Seattle
sweep; pitched is under 0.025% everywhere and exact at z14 and z16. What remains anywhere is
polygon and glyph edges.

### The quad, rebuilt on the finished parity work

`emb bundle --build` against fluorite `main` at `e0241bcc` -- twelve commits on from the view
extension -- and tessella at the end of the parity thread. Four platform views granted their
dma-buf slots, no errors, 381 tiles served, and it holds. Killed after, since nobody is watching it.

Two notes for the next rebuild. The hook tree has to go first (`rm -rf <app>/.dart_tool/hooks_runner
<app>/build`) or a cached `filament_DIR` outlives the change that was meant to move it. And the
`fluorite_core_ffi` the quad's pubspec links against is a build artifact of fluorite's own hook,
dated a day behind: it did not matter this time, because the link only needs a copy with the right
soname and the same symbols, and `$ORIGIN` resolves the bundled fresh one at runtime -- but it is
only safe while fluorite removes no symbol `tessella_fluorite` uses. Checked rather than assumed
this time: 71 undefined filament symbols on that side, all 71 exported by the fluorite in the
bundle.

### The globe's consumer half, started at the arithmetic

§13.4 leaves four things: the vertex bend, the subdivision, the horizon cull, and symbol placement
on a sphere. Three of them need the same projection and only one is a shader, so `tessella-tile`
gets a `globe` module before any material does -- written once, a CPU horizon test and a GPU bend
cannot drift apart the way a shader and a hand copy of it do.

- `sphere_point` is GL JS's `latLngToECEF`, negated `y` included. That sign is the screen's axis
  rather than the Earth's and every formula downstream of it there assumes it, so it is kept rather
  than corrected.
- `sphere_point_from_mercator` is the form the bend will use, tile geometry being Mercator. It
  clamps past the Mercator limit, so a tile edge running off the top of the world lands on the pole
  instead of diverging.
- `faces_camera` is the horizon: for a unit sphere seen from `d` radii out the tangent grazes at
  `cos θ = 1/d`.
- `camera_distance` ties the sphere's radius to `world_size`, so the globe and the plane agree
  about scale at the zoom a map would switch between them.

**The tests are identities, because there is nothing to render against.** The poles are one place
however their longitude is spelled; the antimeridian's two spellings meet; a round trip through
Mercator returns the latitude it started from; a point at exactly sixty degrees is exactly on the
horizon of a camera two radii out; every point is on the unit sphere. That is weaker than the
gross-pixel comparison every other page here rests on, and saying so is part of the record: this is
the one piece whose correctness is argued rather than measured.

What is next, in the order it can be checked: the horizon cull, because it needs only the module
above and a per-tile dot product and its effect is countable; then the subdivision, whose output is
a vertex count and can be pinned; then the bend, which needs a material and is the first thing on
this page that can only be judged by eye; then symbols, which need the bend to exist first.

### The horizon cull is worth nothing per tile, and §13.4's number says why

Implemented and counted. The number on this page -- a third to a half of the cover behind the
sphere between z1 and z2.5, nothing outside it -- reproduces to the tile:

| | z0 | z1 | z2 | z2.5 | z3+ |
| --- | --- | --- | --- | --- | --- |
| tiles whose *centre* is behind | 1 of 1 | 2 of 4 | 3 of 9 | 0 | 0 |
| tiles with *no corner* visible | 0 | 0 | 0 | 0 | 0 |

The first row is what was measured and the second is what can be culled. A z1 tile spans ninety
degrees of longitude, so its centre passes behind the horizon while a third of it is still on
screen; culling on the centre leaves a hole in the planet. Asked safely -- is *any* part of this
tile visible -- nothing is ever removed, because at the zooms where a horizon exists the tiles are
enormous.

So the cull does not belong before the subdivision, where §13.4 put it. It belongs *after*, on
patches small enough for the question to have a useful answer, and the arithmetic is written and
tested for when it gets there. The zero is pinned, so a later change that starts culling whole
tiles has to explain itself.

Counting it also found a bug that the identity tests could not: `camera_distance` took no viewport
height and stood in one pixel, which put the camera 1.018 radii out at zoom zero -- a visible cap
ten degrees wide, and a cull that removed the entire world. It takes the height now. That is the
argument for doing the countable piece before the one that needs a shader.

### The subdivision, derived rather than tabulated

GL JS carries a granularity expression -- a base halved per zoom, floored at a minimum -- whose
numbers are chosen. A bound can be checked instead: split an arc of angle θ into n pieces and each
chord sits `R(1 - cos(θ/2n))` inside the arc, so the count that holds that under a pixel is
arithmetic with an answer, and it re-derives itself for a different tolerance or field of view
instead of being re-tuned.

Segments per tile edge at a half-pixel tolerance, and what they cost where a tile is actually drawn:

| tile | z0 | z1 | z2 | z3 | z4 | z5 | z6 | z8 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| segments, at its own zoom | 29 | 21 | 15 | 11 | 8 | 6 | 4 | 1 |
| vertices | 900 | 484 | 256 | 144 | 81 | 49 | 25 | 4 |

Cheap, and self-limiting: by z8 a tile is a flat quad. The off-diagonal is where it would bite -- a
z0 tile still on screen at zoom 12 hits the ceiling -- and `MAX_EDGE_SEGMENTS` is there because the
count is monotone in the radius and squares into a vertex count.

**§13.4's "ninety segments an edge" for a z1 tile is not what the geometry asks for.** A half-pixel
bound asks for twenty-one. Ninety came from a table, and the table it came from is GL JS's, whose
base granularity is picked for a renderer with different tolerances. Worth knowing before that
figure is used to size anything.

Two things the tests hold, beyond the bound itself: one segment fewer would break the tolerance --
without which "subdivide everything to the ceiling" satisfies it -- and a finer tile never asks for
more than a coarser one, which is the property that makes the count per tile rather than per frame.

### The globe's view matrix, and what three assertions caught

Sphere to clip: turn the world so the point under the camera faces it, back the camera off along
that axis, project. `rotate_x` and `rotate_y` join `rotate_z`, which was the only one this had.

Three sign or order errors, each caught by exactly one assertion, and all three of the kind that a
hand check would have missed:

- **The translation post-multiplied**, giving `R · T` where a view matrix needs `T · R`, so the
  camera translated in the already-turned frame and everything landed behind it. Caught by "the
  point under the camera is in front of it".
- **The rotations composed in the order written**, and these post-multiply, so
  `rotate_x(rotate_y(I, lon), lat)` applies the *latitude* first. Only a camera on the equator or
  the prime meridian landed right -- which is precisely the pair of cases anyone hand-checking would
  have picked. Caught by Seattle.
- **`sphere_point`'s `y` points down**, GL JS's convention, while clip space has `y` up. The flip
  belongs on the output side; post-multiplied it flips the point before the rotation rather than the
  picture after it. It reverses triangle winding, which the consumer needs to know when it culls
  faces. Caught by "north is up".

And one test was wrong rather than the code. "The antipode is behind the camera" is false: it sits
on the view axis *inside* the frustum, projects to the centre of the screen, and is merely further
away in depth. A projection cannot express occlusion — which is the whole reason `faces_camera`
exists, and the test now asserts the two agreeing instead.

That is three of §13.4's four pieces standing on identities and bounds, with no oracle anywhere:
the projection, the horizon, the subdivision and now the camera. What is left is the bend itself,
which is a material, and symbol placement, which needs the bend. The bend is where the eye becomes
the only instrument — everything up to it has been checkable, which is why it went last.

### Reviewing the globe module: two defects, and what is not there

**Reliability.** `1u32 << z` panics in a debug build for `z >= 32` and shifts by `z % 32` in a
release one -- the worse half, because at `z = 32` it answers one tile across and a caller reads a
tile covering the whole world. Three call sites. All now go through `tiles_across`, which clamps to
`cover::MAX_ZOOM`, the ceiling every cover already applies and the posture DR-9 states about a
camera not being trustworthy. And a viewport with no width divides by zero inside `perspective`,
handing back a matrix of NaNs that propagates into every vertex rather than failing anywhere a
caller can find; a degenerate viewport gets a square frustum instead.

Both were found by probing rather than reading, and both are tests now.

**Performance.** Nothing allocates on any path -- no `Vec`, no `Box`, no formatting. The one loop
is `edge_segments`' exact-check refinement, bounded by `MAX_EDGE_SEGMENTS` and normally not entered
at all, because the closed form it starts from is already right. The per-tile cost is five
`sphere_point` calls in the horizon test, each a pair of trig calls plus `latitude_of`'s `exp` and
`atan`; at forty tiles a frame that is two hundred, against the thousands of glyph quads a symbol
layer already lays out. Worth revisiting only if the horizon test moves per-patch, where the count
becomes five per *sub-patch* and the arithmetic wants hoisting out of the loop.

**Security.** No `unsafe`. Nothing here parses, allocates from a size it was handed, or indexes by
one. The exposure a renderer has to a hostile camera or tile id is a panic, and the shift was
exactly that: a denial of the view rather than of anything worse, and now clamped.

What this does not cover is the bend, which does not exist yet. A shader has its own version of
each of these -- an unbounded loop is a hung GPU, and a NaN vertex is a whole draw call gone -- and
none of the checks above will reach it.
