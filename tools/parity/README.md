# Visual parity

The §9.1 gate: the same camera through maplibre-native's own renderer and through this one,
counted in gross pixels. A change that moves these numbers has changed what the map looks like,
whether or not it meant to.

## Running it

    tools/parity/build.sh        # both probes and the materials, from the current trees
    tools/parity/sweep.sh        # the five cameras, against the oracle

    tools/parity/quad.sh         # the four-pane quad, replayed from its snapshot
    tools/parity/fixtures.sh     # the golden fixtures' pixels; needs no server at all

The quad is the other half of the gate and is not part of the sweep: four views on one engine,
which is what catches a regression in layer masking or in the shared scene that a single-view
render cannot. It holds at `22 / 22 / 22 / 32` primitives with `image_stable 1`.

It reads its planet archive through the examples' record-and-replay proxy rather than from the
origin. A dated protomaps build is kept for about a week, and pointed straight at one this test
rots on a timer: the archive 404s, every pane draws two primitives, and it reads as a regression
in whatever was being changed. `PARITY_RECORD=1 tools/parity/quad.sh` records what it lacks;
`quad-manifest.txt` is the URL, byte range and content hash of each, which is what makes the
snapshot checkable. Running `quad_probe` directly still works and still reaches the network.

Needs three things the tree does not carry: maplibre-native's `mbgl-render` as the oracle, a
Filament build, and the tile and asset servers the scenes name (`serve.sh` on 8080 and
`assets.py` on 8081). `env.sh` says where each is expected and every path is an override. The
terrain scenes read `scenes/dem.py` as well, which the sweep starts when nothing is listening on
its port and stops again when it is done.

`fixtures.sh` is the exception to that paragraph: the style fixtures the golden dumps are captured
from carry their geometry inline, so it needs the two renderers and nothing else. It exists because
`verify_goldens.sh` compares those fixtures' *structure* and never a pixel, while the sweep compares
pixels and never isolates a family -- a dash or a gradient is a few pixels of Berlin, well under the
noise of everything else in the frame. A shader or material regression in one of those families
could pass both. One camera per fixture, the family alone on the frame, and a generous budget whose
job is to notice a fixture that went blank rather than to police a pixel.

`examples.sh` runs the MapLibre GL JS documentation examples the same way, from a recorded
snapshot of what their origins serve; see [`examples/README.md`](examples/README.md).

## What the numbers mean

`gross N of M` counts pixels whose per-channel difference exceeds 48. Not a mean: a mean hides a
hundred wrong pixels among a million right ones, and the question is how many pixels a person
would call different. Pass 12 as a third argument to `gross.py` for the second lens, which is
what to use when a change is supposed to move nothing at all.

The sweep's numbers as of 2026-09-16, which is the gate:

    families_p          z14 p0     4 of 786432
    families_p          z14 p60   16
    families_p          z16 p0     0
    families_p          z16 p60    2
    families_p          z9  p0    30 of 2160000
    annot_p             z14 p0     3 of 786432
    annot_p             z16 p0     2
    puck_p              z14 p0     0 of 786432
    puck_p              z16 p0     0
    puck_p              z14 p60    0
    hill_p              z14 p0     0 of 786432
    relief_p            z14 p0     0
    hill_p              z11 p0     0
    relief_p            z11 p0     0
    terrain_flat_p      z14 p0     0 of 786432
    terrain_flat_p      z14 p60   28
    terrain_flat_p      z16 p60    0
    terrain_families_p  z14 p0     4 of 786432
    terrain_families_p  z14 p60   16
    terrain_families_p  z16 p0     0
    terrain_families_p  z16 p60    2
    terrain_families_p  z9  p0    30 of 2160000
    terrain_cover_p     z14 p0   holes 0 of 786432
    terrain_cover_p     z14 p30  holes 0
    terrain_cover_p     z14 p45  holes 0
    terrain_cover_p     z14 p60  holes 0
    terrain_cover_p     z16 p45  holes 0

`holes` is the other measure, for the one scene nothing can be compared against: pixels of a color
the scene uses for nothing but its background. See `terrain_cover_p` below.

### Where the example suite's residual is, as of 2026-10-15

`examples.sh` reads **2,032 gross over 59 cameras**, with **30 of the 59 at exactly zero**. That is
measured with fill outlines off, which is what that suite compares now and why: mbgl's two outline
paths disagree with each other and this side can only take one of them, so leaving them in put 90%
of the number beyond reach of any change. See `no_fill_antialias.py`.

Every one of the 2,032 is accounted for, by running `lean.py` over the ranking:

| | gross | |
|---|---|---|
| antialiasing jitter | 989 | 49% -- reads near 50%, finished |
| the circle-edge floor | 711 | 35% -- one-sided by construction, see the golden README |
| mixed | 223 | 11% -- `add-a-pattern-to-a-polygon` 139, `render-world-copies` 64 and one more |
| silhouette and edge effects | 65 | 3% -- `display-buildings-in-3d` 41 and the multidirectional hillshade 24 |
| the viewport-anchored light | 20 | `animate-map-camera-around-a-point`, 95% one-sided |
| everything else | 24 | across forty-one slugs, none above eight |

Two rows left this table since the figure above was 2,233 on 2026-09-24, and both are worth more
than the arithmetic. **tessella#297's 237** went when the Filament consumer gained its draw-once
stencil mask (fluorite-game/tessella_fluorite#37): `extrude-polygons-for-3d-indoor-mapping` reads 4.
And **857** went with tessella#403, which is a *regression this suite caught and the five-camera
sweep did not*: four of a basemap's label drawables, announced and displaced in the same frame, on
`display-a-popup-on-hover`. It read 1 before tessella#303 and 858 after, and every row of the sweep
stayed where it was through both -- which is the argument for keeping this suite rather than a
claim about why. A hundred and twenty layers of somebody else's basemap is a shape none of the
scenes here have.

**The largest single camera not covered by a named cause is 41 pixels.** That is the useful form of
this table: not that the suite is finished, but that nothing is hiding in it above forty pixels, so
the next parity defect has to be found by adding a scene rather than by staring at this one.

That 41 was looked at rather than left as a number. In `display-buildings-in-3d` the differing
pixels are a thin diagonal line along one building's silhouette where two *opaque* extrusions
overlap: this side shows the lower roof where the oracle shows the taller block, by less than a
pixel. It is a boundary effect of the same kind as the two floors above, which is why it is grouped
with them rather than carried as an open question -- and it is emphatically not tessella#297, whose
signature needs a translucent layer and a depth pass.

`#297` covered two examples rather than one -- `extrude-polygons-for-3d-indoor-mapping` at
`fill-extrusion-opacity` 0.5 and `animate-map-camera-around-a-point` at 0.8 -- and both are below
twenty now. A third extrusion example, `display-buildings-in-3d`, leaves its opacity at the default
and is therefore opaque, which is why its 41 was *not* that issue and is in the silhouette row.

## Properties no camera here can settle

A sweep measures what a frame shows, and some style properties do not reach one. Auditing this
build against mbgl's own `*_layer_properties.hpp` — 55 properties, which is the authoritative list
rather than this side's spec tables — left three that nothing here reads, each for its own reason.
Recorded so they are not investigated a fourth time.

| property | why no scene shows it |
|---|---|
| `raster-fade-duration` | Times a tile fading in over its parent. `pack_raster_props` writes the fade *complete* on purpose: a capture is a still frame, and a value invented for the transition would be a number on the wire nothing produced. |
| `symbol-avoid-edges` | Honored only in `MapMode::Tile`. `placement.cpp` builds a `TilePlacement` for that mode alone, and `getAvoidEdges` is its override — the capture probe runs `MapMode::Continuous` and `mbgl-render` defaults to `Static`, so neither oracle can exercise it. |
| `symbol-screen-space` | Feeds `defaultOpacityState`, the opacity a symbol carries *before* placement has run on it. Every frame compared here is a settled one, and the sweep runs with `TSF_NO_FADES=1`. |

Each was measured as well as read: setting it moves not one pixel of `mbgl-render`'s output in a
scene built to exercise it. That is a weaker statement than it looks — a property can be
implemented and still show nothing in a scene that does not reach it, which is how
`raster-resampling` hid (tessella #260: the parity raster source is flat enough that 16x
magnification put 25 distinct colors on the frame and the two filters agreed to within one channel
value). So the table gives the mechanism, not just the measurement; a zero on its own settles
nothing.

The other two the audit found were real and are fixed: `raster-resampling` in #260 and the three
`*-sort-key` properties in #259, with `text-keep-upright` in #261 and
`fill-extrusion-rounded-corner-distance` in #263 from the same pass.

**A caveat on the audit's reach.** It compares against mbgl's property *headers*, so it sees a
property this build never declared — which the earlier spec-table version could not. It still
misses anything mbgl has not declared either, and it needs care with names this build composes at
runtime: `{prefix}-rotation-alignment` and `{prefix}-pitch-alignment` are read through
`alloc::format!` and a literal grep reports them missing. Four false positives came from exactly
that.

## The scenes

- `families_p` — one layer of every family this build draws, over Berlin. The sweep's scene.
- `extrusion_one_p` and `extrusion_pair_p` — the same translucent buildings over Berlin, drawn as
  one fill-extrusion layer and as two split by a height filter. A pair on purpose, because neither
  number means anything alone: the consumer's draw-once mask owns a single stencil bit, so one 3D
  run per frame takes it and any second layer draws unmasked. The control is one run masked and
  reads **10**; the pair is the same pixels with the second run unmasked and reads **378**, of
  which 97.4% are darker on this side — a translucent surface blended twice. The 368 between them
  is what the limitation costs, and it is the only thing either scene is for. One color between
  the two layers so nothing but the masking differs. See tessella#312, and
  `FilamentRenderer::drawOnceSkipped`, which counts the runs that wanted the mask and could not
  have it: 0 for the control and 1 for the pair.
- `quad_remote` — the four-pane quad's style, read by range out of a planet archive with nothing
  on disk. Used by `quad.sh` rather than by `sweep.sh`, and through the proxy: the URL in it names
  the proxy, which holds the archive's ranges in the snapshot.
- `extrusion_pattern_p` — `families_p` with `fill-extrusion-pattern` on its buildings layer. It
  exists so that a capture carries `FillExtrusionPatternShader` and
  `FillExtrusionPatternInstancedShader`, which no other scene here sets: the two sat in
  `tessella_emblema`'s undrawn table for months on "the producer emits no drawable for it yet",
  and the producer had emitted them all along. A capture of this scene plans 24 of 24 geometries
  across ten families there.

  Two more things differ from `families_p`, and both are there so the row can fail. The sprite is
  `interstate_3`, 48x39 and high contrast: the pattern arithmetic is a texture *coordinate*, and
  over `sand_noise` -- 50x50 with five levels of spread -- every wrong coordinate still samples
  nearly the same color, so the scene read a clean 41 with the wall's edge distance zeroed. It is
  also chosen for low spatial frequency, because a sprite's own edges are where two rasterizers
  disagree: `rer.transilien`, which is 36x18 of fine detail, holds a floor of 1,267 where this one
  holds 725. And the light is `[1.15, 210, 64]` at intensity 0.8 rather than the default
  `[1.15, 210, 30]` at 0.5, which is a degenerate input twice over -- `1 - intensity` equals
  `intensity`, so the unlit floor cannot be mutated, and the roof's `z` of 0.996 drives the
  directional term into its clamp, so neither can the lit end. At 0.8 and a polar of 64 the roof
  sits at 0.75 of the range with both ends live.

  In the sweep flat and pitched, which are two different halves: a wall only shows at a pitch, and
  the sprite is sampled nearest on a flat camera and linearly on a pitched one -- see
  `extrusion_pattern_filter`, which is mbgl's rule and costs 356 against 37 if it is ignored.

      extrusion_pattern_p  z14 p0    37 of 786432
      extrusion_pattern_p  z16 p60  725

  Both lean 43% and 54% brighter, which is the antialiasing class.
- `line_pattern_p` — `families_p` with `line-pattern` on its roads layer, twelve pixels wide at
  `line-opacity` 0.6 and nothing else changed. `LinePatternShader` is in no other scene here, and
  the Filament consumer had no material for it: the layer drew nothing at all, 230,876 of 786,432
  at z14 p0 against `families_p`'s 4. fluorite-game/tessella_fluorite#42.

  The sprite is `interstate_3` for the reasons the patterned extrusion names, and the width is
  twelve because the pattern's second axis is *across* the line: a two-pixel road has nowhere to
  put a picture, and the across-line mapping reads 10,049 when it is flipped at this width.

  Both levels and north-up, and the second camera is what the scene is for. `berlin_z15` stops at
  fifteen, so a z16 camera draws z15 tiles — and the pattern's period is `1 / pixelsToTileUnits(1,
  intZoom)`, which is the one quantity in the family that has to notice that the cover is coarser
  than the camera. Measured at the tile's own level the two arguments cancel, the answer is
  sixteen at every zoom, and the sprite is drawn over twice the distance mbgl draws it over:

      line_pattern_p  z14 p0     6 of 786432
      line_pattern_p  z16 p0   123          (27,557 before the period was fixed)

  The z16 row leans 47.2% brighter, which is the antialiasing class; six pixels is under the
  twenty `lean.py` needs for the question to have an answer. Pitched the same cameras read 17 and
  101, and the 101 leans 49.5%.

  Three terms this row does not gate, said here rather than left to be assumed. The crossfade is
  pinned: `TSF_NO_FADES` makes the fade's `t` one, so only the `to` sprite is sampled and only
  `toScale`, which is 1, scales it — `fromScale` and `fade` reach the shader and cannot change the
  picture. The height clamp, which keeps a wide line inside its own symbol on the sheet, is
  inactive: it bites only for a line wider than its sprite is tall, and a road wider than 41
  pixels is not a style this suite has. And `line-opacity` moves 282,927 pixels of this side's own
  frame while raising the gross from 124 to 195, because a light sprite at 0.6 over a light ground
  differs from the same sprite at 1.0 by less than the threshold's 48 per channel — the control is
  `gross.py <before> <after> 8`, and that number is what says the term is read at all.
- `background_pattern_p` — a background with `background-pattern` and the roads over it, which is
  the whole scene. Not `families_p` with one property changed, like the other pattern scenes: a
  background is the layer *under* everything, and under that style's raster and fills there is
  almost none of it left to look at. Here it is the viewport, which makes it the sharpest test of
  a pattern's placement in the suite and the sharpest test of its atlas's edges as well.

  It found both halves of tessella#398 and is why that issue exists. A patterned background was
  reaching its shader with the *plain* background's drawable block — no pixel origin and a zero
  ratio, which takes the world position out of `patternPos` and leaves every fragment sampling the
  rectangle's own corner — and with `pattern_atlas_texsize` still zero, because the frame-wide
  block is written when the camera moves and a sprite sheet lands on whatever frame it lands on.

      background_pattern_p  z14 p0   462,804 of 786,432   both wrong
                                      21,823              both fixed
                                           0              and the pass right

  **In the sweep at both levels north-up, where it reads 0 and 0**, and it found two defects to
  get there.

  The *pass*: mbgl marks a background `Opaque | Translucent` only when it is opaque, and one with
  a pattern, an opacity under one, or a color that is not opaque is `Translucent` alone
  (`render_background_layer.cpp:59`). This side marked both for every background, and the opaque
  pass is drawn by replacement -- so the sprite's transparent texels were written over the frame
  instead of blended. 21,823 at z14, and the scene's whole number until tessella#401.

  The *atlas*: the border around a packed sprite was the atlas's clear, where mbgl fills a
  pattern's border with the image's own opposite edge so that a tiled copy wraps into itself.
  A linear sample at the seam blended with nothing instead, which is a thin outline around every
  copy -- 4,396 at z16, in 21 columns spaced exactly one pattern period apart, and below the
  threshold at z14 where the seams fall differently against the pixel grid. tessella#397.

      background_pattern_p  z14 p0   462,804 of 786,432   the drawable block and the atlas size
                                      21,823              the pass
                                           0              the border
      background_pattern_p  z16 p0     4,396 -> 0         the border, where it crosses 48

  The two had to be found in that order: with the pass still wrong the border is a fifth of the
  number, and an experiment that wrapped the border alone moved 21,823 to 22,090 and read as a
  disproof. See `a-residual-needs-a-control` for the rule that cost.

  The scene keeps `interstate_3` because its contrast is what makes it catch anything: over
  `sand_noise` the broken drawable block read 0 as well. Pitched it reads 4 and 3, which is the
  antialiasing floor of a pattern whose every copy has an edge.

- `stretch_p` — nine shields on a plain ground, from maplibre-native's own `sprites/stretch`
  pair. A sprite may name the columns and rows of itself that may stretch, and under
  `icon-text-fit` the picture is drawn as a *grid* of quads rather than one, so a shield's
  rounded ends keep their pixel size while the band between them grows with the label. Three
  sprites: `nine-part-content` at pixel ratio 2 with a content box, `fifteen-part` whose two
  stretchable columns make a five-by-three grid, and `three-part` fitted on width alone.

  No other scene here has a stretchable sprite and no example fixture has one either — every
  entry of `emerald`, which was the whole of `tests/sprite-fixtures`, names no `stretchX`. So
  this is the scene that found tessella#405 and tessella#406, and the argument for it is the one
  the examples section makes: a defect nothing draws is a defect nothing measures.

      stretch_p  z14 p0   12,769 of 786,432   drawn as one stretched picture
                          15,326              sliced, and measured in em
                               0              sliced, and measured in pixels

  The middle row is the useful one. Slicing the picture correctly made the number *worse*,
  because the second defect put the shield's box half again too wide and every cell of a correct
  grid then landed somewhere else. Neither fix reads as a fix on its own.

  It also found fluorite-game/tessella_fluorite#46 on the way through, which nothing else could
  have: the pixel offset the grid needs has been on the wire since the symbol vertex was written
  and had only ever carried zero, so a consumer reading it in the wrong units drew correctly
  until the day something put a value in it. 348,284 for one run, between the producer's fix and
  the consumer's.

- `textfit_p` — the other half of the same mechanism, which is a different one. The *layer* says
  which axes to fit with `icon-text-fit`; the *sprite* says how far that fit may distort it, with
  `textFitWidth` and `textFitHeight`. `apply_text_fit` implements the second and nothing drew it
  until this scene: three of `textFit`'s nine combinations, from maplibre-native's own pair.

  The proportional case bites, which is what makes the row worth having rather than a second copy
  of `stretch_p`. Fitted to the same wide label, the stretch-only sprite comes out 114 pixels
  across and the proportional one 98 — pulled back to its content box's aspect. Both read 0.

- `heat_p` — a heatmap over Berlin's POIs. Captured before any heatmap code existed, so the
  design behind it had a referee rather than an argument; it stood at `gross 271571 (34.532%)`
  then and is at **0** now. Not in the sweep because the sweep is the five-camera gate.
- `annot_p` — the three annotation classes over Berlin: symbols with and without an icon, a line
  and a multi-line, a polygon with a hole and a multi-polygon. It stood at `gross 39712 (5.050%)`
  the day it was written, when nothing drew an annotation, and now reads

      annot_p  z14 p0     3 of 786432
      annot_p  z16 p0     2
      annot_p  z14 p60   98

  In the sweep north-up at both zooms, and deliberately not at a pitched camera. `z14 p60` reads
  98 and `p40` reads 137, and that is the *oracle*: mbgl's fill outline is a GL line whose
  fragment measures its distance from `v_pos`, a screen-space varying, and a screen-space varying
  interpolated perspective-correctly -- which is all GLSL ES offers -- drifts from the truth
  toward the near end of any edge running away from the camera. A polygon's vertical edge runs
  exactly that way, so the oracle draws the outline at the far end of it and nothing below.
  Measured on a rectangle inside one tile: the oracle's coverage down that edge is 0.65 at the
  top row and 0.00 for every row under it, where this side holds 0.1 to 1.0 the whole way. Gating
  a pitched camera here would gate a defect, and the number could only get worse by fixing
  something.

  `z14 p60` stood at 576 and then 558 before any of that, and both were the fill outline too --
  the other half of it. The oracle's GL line is two pixels on screen whatever the camera does;
  this draws mbgl's own triangulated fallback, whose quad is extruded in tile units and
  foreshortened until the fade had no fragments left to spread across. The quad is widened to
  screen space now, which is what the GL path always did.

  No other scene here sets `fill-outline-color`, so this scene is the first thing to gate that
  path at all -- and the outline runs on *every* antialiased fill, so what was true here was true
  of every filled polygon in every style. `families_p z16 p60` came down from 52 to 50 with it.

  An annotation is not a style layer -- there is no `"type": "annotation"` and no stylesheet can
  produce one. They arrive through `Map::addAnnotation`, and the manager synthesizes the source
  `org.maplibre.annotations`, one symbol layer for the points and one line or fill layer per
  shape. So the scene is two files: `annot_p.json` is the style and `annot_p.geojson` is what the
  oracle is told to add. `parity.sh` pairs them by name, and offers every `.png` beside them as an
  icon. `marker.py` writes `marker.png`, so the icon's pixels have a stated origin.

  Stock `mbgl-render` has no such flags and will fail the run. They are on the capture branch
  (`jwinarske/maplibre-gl-native`, `capture-backend-phase0`), which is the oracle these numbers
  are measured against anyway. This side takes the same two files through `TSF_ANNOTATIONS` and
  `TSF_ANNOTATION_IMAGES`, because `render_probe`'s arguments are positional and a camera is what
  they are for.

  The scene's polygon winds its hole clockwise, which RFC 7946 requires and the first draft of
  this file did not. It matters more than a formality: mbgl runs every polygon through
  `fixupPolygons`, a wagyu union with **even-odd** fill, which makes a ring nested inside another
  a hole whatever way it winds. Nothing here does, so a same-wound inner ring draws as a second
  overlapping polygon rather than a hole — 3,477 pixels of it in this scene, before the winding
  was corrected. See the note in `tessella-source`'s `annotation` module.

- `hill_p` — a hillshade over generated terrain. It stood at `gross 174053 (22.132%)` at z14 and
  `379466 (48.252%)` at z11 the day it was written, when nothing drew a hillshade, and now reads

      hill_p  z14 p0   0 of 786432
      hill_p  z11 p0   0

  z11 read 151 when it was written and 190 by 2026-09-16, and drifted between 190 and 141 from one
  binary. That was the seam: each tile's border was a repeat of its own edge until its neighbors
  arrived, nothing replaced it, and which tile arrived first decided how the edge shaded. Since the
  store backfills borders on arrival it reads 0 every time the scene is run alone, and 0 to 14
  inside a full sweep -- the reconciliation lands a tick or two behind the tile, and on a busy
  machine the probe can settle on the frame before it.

  The terrain is generated rather than fetched, by `scenes/dem.py`, for three reasons in the order
  they decided it. No archive here carries a DEM and every public one carries a license, so a
  height field written here is nobody's data and redistributes nothing. A procedural field is the
  *same* field on both sides by construction. And it has an analytic derivative, so the normals a
  hillshade computes can be checked against what the surface actually does rather than only
  against the other renderer.

  It is one global function of world position sampled per tile, not a per-tile picture. A
  hillshade's prepare pass backfills each tile's border from its neighbors, and a field with a
  seam at a tile edge would make a correct backfill look broken and a broken one look fine.

  `python3 scenes/dem.py` listens on `PARITY_DEM_PORT` -- 8084, because 8083 was taken on the
  machine this was written on. The sweep starts it when nothing is listening there; to run a scene
  by hand, start it yourself and stop it when you are done.

- `relief_p` — a color relief over the same generated terrain, elevation mapped to color through
  six stops. It opened at `gross 786432 of 786432 (100.000%)` at both z14 and z11 -- every pixel,
  because the layer covers the frame opaquely -- and now reads **0 at both**, and 0 at the strict
  12 lens as well.

  It reads the *raw* DEM rather than the slope field a hillshade reads, and it needs two textures
  no other family does: the elevation stops as floats and the colors at them. mbgl takes the
  stops from the `interpolate` expression itself when `color-relief-color` is one, and samples 256
  points over -500..9000 meters when it is not.

  Same server as `hill_p`: `python3 scenes/dem.py`.

- `terrain_flat_p` — `terrain_p`'s hillshade and relief on a terrain with an exaggeration of zero.
  maplibre-native has no terrain -- a style carrying one renders exactly as the style without it
  -- so at zero it is an exact oracle for everything the terrain adds: the raised variants, the
  raised clip masks, the grid and the rebuilds all have to come out flat.

- `terrain_families_p` — `families_p` over the same DEM with a terrain of zero, held to
  `families_p`'s own numbers. It is what found that a zero exaggeration still split every vector
  tile at the mesh's finest grid: shredded fills at z9, and five times the geometry, which ran the
  slab region out and lost every label.

- `terrain_cover_p` — `terrain_p` raised by 1.5 over a magenta background. There is nothing to
  compare a raised terrain against, so what is held is that it is covered: the ground takes the
  background's color, and every magenta pixel is somewhere the ground shows through the layers on
  it. `coverage.sh` counts them.

  Five cameras, because two are not enough: `p0` and `p45` are the kindest in the space and held
  while everything around them was failing.

      terrain_cover_p  z14 p0       0 of 786432  (0.000%)
      terrain_cover_p  z14 p30      0            (0.000%)
      terrain_cover_p  z14 p45      0            (0.000%)
      terrain_cover_p  z14 p60      0            (0.000%)
      terrain_cover_p  z16 p45      0            (0.000%)

  z16 p45 held 28 holes longer than the rest, and they were the border again: two neighboring
  tiles whose shared edge disagreed by a border pixel raise their edge vertices to different
  heights, and the ground tears along the seam. Backfilling the border closed them.

  They read 62, 34140, 622, 180367 and 618072 before the camera took the ground's height into
  account, and z17 was a frame of pure background at any pitch. What that was:

  The camera sat a fixed distance above the *plane* -- `camera_to_center_distance`, a property of
  the viewport -- while a height in meters reaches the screen multiplied by pixels-per-meter,
  which doubles with every zoom level. So the ground climbed toward a camera that did not climb
  with it, and far enough in it arrived: at this build's 1152-pixel camera, 400 meters of ground
  exaggerated by 1.5 stands 826 pixels up at z16 and 1651 at z17. Past that the camera is
  underground and the frame is whatever the background is. The prediction and the measurement
  agree: at z16 p0 the scene holds at exaggeration 1.5 and 2.0, reads **zero** holes at 2.5 --
  the ground magnified until it covered everything -- and is 100% background at 3.0.

  The fix is GL JS's `Transform.elevation`: the camera's height is measured from the ground under
  the center rather than from sea level. It is applied as a shift in the raise block, which every
  raised family and the clip mask read, so it costs one subtraction and no camera plumbing.

  That exposed a second one. The far plane reached `distance * 1.01`, barely past the center, so
  once the center's ground sat on the plane everything below it was clipped -- 116770 pixels of
  no geometry at all, in bands following the contours. `ViewTransform::ground_below` carries the
  relief the far plane has to reach.

  `p60` was 5027 after that, and the rest of it was the picture's own cover. A DEM read as a
  picture is covered one zoom deeper than the ground -- the 256-pixel rule, so its texels land on
  screen pixels one to one -- and a cover computed at that deeper zoom fits the frustum more
  tightly than the ground's. Along the near edge of a pitched view it stopped short: ground drawn
  with no picture on it, which reads as a band of bare ground. The picture's cover is the ground's
  own refined now, which keeps the finer resolution and paints exactly the ground that exists. It
  costs tiles, because a coarse cover over-covers and refining multiplies that: 104 raster tiles
  where there were 72 at `z14 p60`, and 24 where there were 20 at `p0`.

  Both halves were needed. Refining only what is fetched changes nothing, because the walk that
  draws them visits its own cover; refining only what is drawn changes nothing, because the tiles
  were never asked for. Each was measured alone first and read as a dead end.

  The last of it was the skirt. The ground's mesh hangs one from every tile edge, because two
  tiles agree on a shared edge's world position and reach it through different matrices, so the
  two land a fraction of a pixel apart and the boundary pixels are claimed by neither. A picture
  drawn *on* that ground is its own surface with its own edges and cracks in the same places, and
  what shows through is the ground, which takes the background's color.

  So a raised raster hangs a curtain too, and so does the clip mask -- a mask cut to the surface
  clips the very skirt it admits, which is why the first attempt changed nothing at all. The
  curtain is not the ground's: that one is a fifth of a tile and is invisible only because the
  ground is the backmost thing there is, and the same length on a picture is a wall across the
  view. It is four pixels, in meters, at the frame's own camera -- wider than any crack and too
  narrow to see.

  Not on ground that is not raised. At an exaggeration of zero the tiles are coplanar and there is
  no crack; a curtain there is a line drawn rather than a seam filled, and the oracle says so --
  `terrain_flat_p` went from 0 to 547 before it was gated on displacement.

- `puck_p` — a location indicator over Berlin: an accuracy circle, a bearing, and the perspective
  compensation that decides how the puck leans when the camera pitches. **This one does not pass
  and is not meant to yet**: `gross 21586 of 786432 (2.745%)` at z14 p0 and `188573 (23.978%)` at
  z16 p60.

  It is the first family that is neither tiled nor a viewport quad. Its drawables carry `tnone` --
  no tile at all -- because a puck is at a *place*, not in a tile, and there is exactly one of it
  however many tiles the cover has.

  Two shaders and four drawables. `LocationIndicatorShader` draws the accuracy circle from 73
  vertices -- "72 points + position", which is mbgl's own comment -- as a fan of 216 indices for
  the fill and a strip of 72 for the border. `LocationIndicatorTexturedShader` draws three quads:
  the shadow, the bearing image and the top image, in that order.

  mbgl has *two* implementations of this layer and the header picks one unconditionally:
  `#define MLN_DRAWABLE_LOCATION_INDICATOR` at the top of
  `render_location_indicator_layer.hpp`, which leaves the raw-GL branch below it dead. That is
  what makes this transcription rather than argument -- the live branch produces drawables, so the
  capture backend records them and there is a stream to diff as well as a picture.

## Why the scenes are here and the frames are not

A scene is a question and belongs with the code it asks about. A rendered frame is an answer to
one run of it, is megabytes, and is regenerated in seconds — including the oracle's, which
`parity.sh` re-renders every time rather than trusting a stored PNG whose camera nobody wrote
down. Everything a run produces goes to `PARITY_WORK`, outside the tree.

No tile or glyph data is vendored here. The scenes name servers; what those serve stays where it
is licensed.
