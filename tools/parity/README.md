# Visual parity

The §9.1 gate: the same camera through maplibre-native's own renderer and through this one,
counted in gross pixels. A change that moves these numbers has changed what the map looks like,
whether or not it meant to.

## Running it

    tools/parity/build.sh        # both probes and the materials, from the current trees
    tools/parity/sweep.sh        # the five cameras, against the oracle

    cd "$PARITY_WORK" && ./quad_probe "$PARITY_DIR/scenes/quad_remote.json" mat quad.ppm

The quad is the other half of the gate and is not part of the sweep: four views on one engine,
which is what catches a regression in layer masking or in the shared scene that a single-view
render cannot. It holds at `22 / 22 / 22 / 32` primitives with `image_stable 1`.

Needs three things the tree does not carry: maplibre-native's `mbgl-render` as the oracle, a
Filament build, and the tile and asset servers the scenes name (`serve.sh` on 8080 and
`assets.py` on 8081). `env.sh` says where each is expected and every path is an override. The
terrain scenes read `scenes/dem.py` as well, which the sweep starts when nothing is listening on
its port and stops again when it is done.

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

## The scenes

- `families_p` — one layer of every family this build draws, over Berlin. The sweep's scene.
- `quad_remote` — the four-pane quad's style, read over https with nothing on disk. Used by the
  consumer's own `quad_probe` rather than by `sweep.sh`.
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
  which doubles with every zoom level. So the ground climbed towards a camera that did not climb
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
