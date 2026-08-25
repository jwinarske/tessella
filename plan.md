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
- **`projMatrix` is f64 column-major** (glam `DMat4`); **`centerZoom0` is scale-free** — the
  zoom-flicker regression documented in frame_diff.hpp is a named test case.
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

Consumer effect: one Filament VertexBuffer/IndexBuffer per shared geometry; one renderable in
multiple `Scene`s; one `View` per map via the existing view slots. VRAM and upload bandwidth
scale with unique tiles, not views.

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

Four-view sizing: decode workers pinned to little cores, big cores for orchestrator +
Filament; ring sized for a four-view simultaneous integer crossing at worst-case tile counts;
per-view maxzoom clamps by view class (a cluster inset capped at z14 never joins a z16
crossing burst).

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
| cache DB | rusqlite (bundled) | sqlite vendored |
| HTTP | ureq (blocking, on workers) | cpp-httplib/curl |
| f64 math | glam DMat4/DVec | mbgl matrix |
| ring/sync | crossbeam (or hand SPSC matching ihs ring ABI) | — |

Expressions have no crate; hand port. Symbol placement has no crate; hand port.

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
- Shared-store counters (fetches, decodes, bucket builds, atlas uploads) do not scale with
  view count for overlapping covers.
- Screen-space UBO variants (R-2) differ per view over identical shared geometry.

### 9.3 Counters (CI assertions)

Extend the LogFrameSink-stats pattern: bytes/frame parked == 0; bytes/frame during pure pan ≤
camera-block budget; OrderUpdate count == order-change count; AttributesModified == 0 on a
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
  Parked bytes are zero over five hundred settled frames. Qualifications: GeoJSON polygon vertex
  *order* is a rotation of the oracle's, which DR-19 explains and declines to chase; and
  `proj_matrix` refuses bearing and pitch, the probe being unrotated, so the quaternion path
  waits for a capture to check it against.
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
  reaches first geometry in 2.0 ms against 6.6 ms cold, with zero round trips.
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
  Remaining: TLS and reading `.pmtiles`
  archives directly (both held for the cross toolchains, §16), cross-faded (pattern) binders,
  DR-11's bytecode VM — classification, constant folding and the direct evaluator are done and
  wired, and §12.1 now carries the measurement that says what the VM has to fix — and §12.5's
  startup path.
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
  Exit: probe parity on a *real* style sans symbols — **geometry half met**: nine tiles of a
  Protomaps planet extract at z5, all 21 drawables byte-identical to the probe, fills and lines
  both, over the same live origin. Cold-boot-to-first-tile is traced (§12.5): style parse,
  source resolution, cover, first fetch, first bucket, complete, with the cover fanned out
  across workers. Against a local Protomaps extract a nine-tile cover reaches first geometry in
  6.7 ms and completes in 22 ms, against 12.7/72 ms serially. The worker count is a bounded
  constant rather than the host's core count, for the reason §5.4 gives — decode belongs on the
  little cores and a host-derived number makes a workstation measurement say nothing about the
  device. §5.4's one process-scoped pool now exists, with the three
  priority classes, and the cold start queues onto it rather than spawning threads per view; a
  waiter helps with work at or above its own class only, so an hours-long region download at
  the background class cannot get in front of a view trying to draw. Remaining for exit: a
  budget to hold the worker count against, on the RK3566 lane rather than on a workstation
  loopback. Decode and bucket build are now shared as well as
  fetched once: the bucket cache is consulted *before* the network, so a warm view costs no
  request at all — which matters because coalescing alone dedupes only *concurrent* fetches and
  is deliberately not a cache, so flatness across time waits on §12.6's byte cache or on a
  caller that checks its own first. GeoJSON sources resolve by URL as well as inline — one
  fetch feeds every tile of a cover, since the tiling is the client's. A tile is built per
  *source*: layers are scoped to the source they name, and the source-less ones — a background
  — are built once per tile rather than once per source. `boot` covers both kinds and their
  different lifecycles: a vector source is fetched once per tile because the server cut it up,
  a GeoJSON source once in total because this side does the cutting.
- **R1.5** — four views over the same style (§13). Exit: §9.2 invariants green; §13.3
  four-view zoom benchmark green on RK3566; §13.1 fractional-zoom counters at zero.
- **R2** — symbols: glyph manager, shaping, quads, per-view placement, collision, cross-tile
  index, fades. Largest phase; budget ≈ R0+R1.
- **R3** — raster, patterns/dynamic textures (rect-list damage), fill-extrusion.
- **R4** — hardening: ring backpressure under stall, teardown protocol under fault, process-
  isolation spike (§3.5) if the sandbox plan wants it, riscv64 soak.

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
`benches/expression_cost.rs` holds the measurement, against the zoom-10 tile
`benchmark/parse/vector_tile.benchmark.cpp` decodes in mbgl's own `Parse_VectorTile` — so the
two sides can be compared on the same bytes rather than argued about. Every absolute figure in
this section was taken on a machine that also measured the same decode at 455 µs and 812 µs an
hour apart under somebody else's build; the with-and-without ratios were alternated across
rounds and held, the absolutes wandered by a factor of two. Read the ratios.
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

### 12.8 Power and pacing

Wakeup pattern matters as much as throughput on DVFS-governed parts. One deadline wheel for
all timers (§5.5); produce at the consumption rate the reverse channel reports, not at loop
speed; parked extends to the scheduler — a parked view holds no timers except cache expiry.
Sustained-idle-then-burst beats constant medium load. Pacing counters land in R4.

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
  zoom that does not cross an integer level.

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
  around an integer zoom must not rebuild cover at gesture rate.
- **Never-blank, acknowledged.** Ancestors retained until every covering descendant's buckets
  are consumer-**acknowledged** via the reverse-channel epoch — mbgl retains until *built*,
  and the build→GPU-upload gap is exactly where its single-frame holes come from. Per-tile
  handoff as descendants land; stencil resolves overlap.
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

## 16. Open questions (rev 0.4 targets)

- PMTiles alongside mbtiles in tessella-storage (vendor tree already carries PMTiles; cheap in
  Rust).
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
- Little/big core affinity policy (§5.5): explicit pinning vs scheduler hints, per target.
- ~~Second-consumer sequencing~~ closed by DR-16: the impeller-rs mirror (Vulkan HAL) lands
  beside the R0 stub.
- ~~UBO floor~~ closed by DR-16: SSBO-only, Vulkan-first.
- ~~Reserve `tessella` on crates.io and GitHub~~ closed: `tessella` 0.0.0 published as a
  dependency-free stub, `github.com/jwinarske/tessella` public, workspace scaffolded to §7
  with the nine `tessella-*` members held at `publish = false` until they carry content.
- Direct-scanout product shape: tessella-* + impeller-rs single-binary cluster map over a leased
  DRM connector (wayland-leased-drm/DLM alignment); scope as its own plan doc if pursued.
