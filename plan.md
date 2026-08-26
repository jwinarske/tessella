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
  speculative sprite and glyph fetch, which has nothing to fetch until R2.
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
- **R1.5** — *in progress.* Four views over the same style (§13). §9.2's three invariants are
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
  change a line's *height* as well as its width, and none of them has an oracle without the
  sprite atlas.
  **Held behind a capture**: the pitched paths, in three different states, and the difference
  between them matters. A line label's collision circles *do* carry the signed distance from the
  anchor that selects a prefix of the run under pitch — computed, stored, and read by nothing.
  `gamma_scale` is written as its pitch-zero value of one, and the perspective ratio mbgl scales
  it by is not written at all. The label-plane and coordinate matrices have a map-aligned branch
  that scales by tile units per pixel and rotates by the bearing, and that branch is deliberately
  absent rather than written and untested: producing it would put a matrix on the wire against no
  measurement. All three wait on the same thing — the probe is unrotated, so there is no capture
  to check any of it against, which is R0's second qualification reappearing rather than a new
  one.
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
  it. Vertical writing, images in text and per-section scaling are not implemented: each changes
  a line's height as well as its width, and none has an oracle here until R3 brings the sprite
  atlas. Three more tests were vacuous for want of a case — every one used zero spacing and no
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
  Line-placed icons are not built. They repeat along a line the way a label does and need the
  anchors `get_anchors` produces; taking the line's first vertex instead would place every icon
  of a road at one end, which draws and is wrong — so they are skipped rather than approximated.
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
  spend on a substitute) is as much of the contract as what it draws. The acknowledged part of
  the bullet is *not* yet done: `TileState::renderable` still means built, which is the caller's
  to define and does not change the algorithm, and making it mean acknowledged needs the
  reverse-channel epoch of R4.
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

- **DR-20 Sprites and raster decode PNG; compressed textures are a separate question.**
  KTX2 with a Basis or block-compressed payload is genuinely cheaper than RGBA8 where it counts
  — a 1024-square sprite sheet is 4 MB decoded and roughly 1 MB as ETC2 or ASTC, and on an
  RK3566 that is shared memory and shared bandwidth. It is the same argument §12.4 already makes
  for R8 glyph atlases, and §12.4's "the C++ formats are the floor, not the target" invites it.
  It still cannot replace PNG here, for three reasons that are not about the codec.
  **The format is not ours to choose.** A style-spec sprite is `sprite.json` plus `sprite.png`,
  and every style in the wild — Protomaps, MapTiler, OpenMapTiles — serves exactly that. Raster
  tiles are the same: the origin decides. A build that reads only KTX2 loads no existing style.
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

- ~~PMTiles in tessella-storage~~ closed: `tessella-storage/pmtiles` reads a v3 archive in
  place, byte-identical to `pmtiles serve` across zoom 0 to 15. It was cheap in Rust, as this
  said. MBTiles is still open, and is a different shape — SQLite rather than a directory format,
  so it lands on the `cache` feature's dependency rather than needing one of its own.
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
