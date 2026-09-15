# Consumer-camera mode: what it would take

A design note, not a change. Written from the consumer side — `tessella_flutter_scene`, which draws
tessella's stream with Flutter GPU — after finding that the last step of putting a map inside
someone else's 3D scene is the one thing the producer does not offer yet.

DR-9 decided this mode and §11.1 describes it. The ABI carries it, and nothing implements it. This
says what is already there, what is missing, what the change would look like, and the decision that
shapes it: the consumer publishes its own view-projection matrix, beside a map camera derived from
it.

## What the consumer wants it for

`flutter_scene` is a 3D engine. Its scene pass owns a camera, a color target and a depth buffer,
and it now lets a caller draw into that pass directly (`Scene.addSceneDraw`, on a branch). A map
drawn there shares the scene's depth: a building can stand behind a model and in front of another,
and the map is part of the world rather than a picture hung in it.

The consumer can draw its batches into that pass today. What it cannot do is *place* them. Every
drawable's matrix arrives as `proj_matrix(view) * placement`, so the geometry lands where tessella's
own camera put it, in tessella's own clip space. Drawn into a scene pass it covers the frame like a
screen-space layer, ignoring where the scene's camera is. The map is in the pass; it is not in the
world.

## What already exists

- **The mode is on the wire.** `ViewDeclare` carries `CameraMode` (DR-18), and `ViewSession::declare`
  writes it. Re-declaring is how a mode changes.
- **The camera block already degrades.** `CameraBlock::for_mode` (`camera.rs:114`) zeroes
  `proj_matrix` and `center_zoom0` under `CameraMode::Consumer`, with the reasoning that a
  stale-but-plausible matrix is worse than an obviously empty one. It is never called.
- **The reverse channel is defined.** `reverse.rs` has `ReverseChannel`, a per-view seqlock slot, and
  `ConsumerCamera` — center at zoom zero, zoom, bearing, pitch, viewport — with `publish_camera` and
  `camera` implementing the write and read discipline.
- **A placement-only matrix already ships.** The globe branch of `ubo::tile_matrix` emits
  `mercator_matrix_for_tile` alone and leaves the projection to the consumer, with the sublayer depth
  nudge carried in the placement's own `[14]` because a per-frame `globe_matrix` has nowhere to put
  it. Mercator in consumer-camera mode wants exactly this shape, one world scale up:
  `matrix_for_tile(z, x, y, wrap, zoom)`.

So the protocol is designed, the degradation is written, and the matrix form exists. What is missing
is the producer reading any of it.

## What is missing

1. **Nothing declares a view as consumer-camera.** `frame.rs` writes `CameraMode::Producer` at every
   declaration site.
2. **Nothing reads the published camera.** The only field of `ReverseChannel` anything consumes is
   `acked_geometry`. `Map::tick` takes its camera from `self.view`, which only `look_at` writes.
3. **The strip is allocated nowhere.** `tessella_map_regions` is four words — ring and slabs — and
   no `ReverseChannel` is constructed outside a test.
4. **The C surface has no way to say any of it.** There is no call to set a view's camera mode and
   none to publish a camera.
5. **What is on screen is decided in map-camera terms.** The cover is built from a `ViewTransform`,
   and labels are placed through a projection the producer builds from one. Neither can take a
   matrix yet, and under the decision below both have to.

## The decision: a matrix beside a map camera

`ConsumerCamera` is a **map** camera: center, zoom, bearing, pitch, viewport. A `flutter_scene`
camera is a free 3D camera — a position, a target, an up vector and a projection. Converting the
second into the first loses roll, an arbitrary field of view and an off-center projection, and a
consumer whose camera cannot be expressed as one would get a map that does not match its scene, with
the producer unable to tell.

So a free camera is a goal here, not a consequence, and the consumer publishes both: its actual
view-projection matrix, and a map camera derived from the same camera. That makes the two
authoritative for different things, and the rule is by the question being asked.

- **The matrix answers "where is it on screen".** Which tiles the view can see, since the cover is a
  frustum question. Where a label lands and what it collides with, since labels compete for screen.
  Any width or size measured in screen pixels at a point.
- **The map camera answers "which data, at what scale".** The integer zoom the cover is fetched at,
  the zoom paint expressions evaluate at, `pixels_per_meter`, and the zoom history the fades and the
  prefetch velocity read. These are geographic by nature and are written in these terms throughout.

They cannot disagree about the same frame, because the consumer derives one from the other and writes
both under a single seqlock generation, and the producer must read both in the same read — never a
matrix from one generation and a camera from the next. When the camera looks somewhere the ground
does not answer for — along the horizon, or up — the consumer publishes its best ground estimate for
the map camera, and the matrix still decides what is visible.

What this costs is item 5 above. The cover and label placement have to accept a matrix where they
take a `ViewTransform` now, and that is the largest part of the change.

## The shape of the change

Five parts, in the order they would be written.

**The strip.** `ViewSlot` grows a view-projection matrix beside the map-camera fields. The slot is
56 bytes and asserted today, but nothing allocates or reads it, so its layout is free to change and
nothing on either side of the ABI moves with it.

**Producer state.** `Map` holds `camera_mode` beside `projection` — the same argument `projection`
is held under, that this is a property of how the view is drawn rather than of the camera — with a
setter modeled on `project_on`.

**The camera in.** `Map::tick` reads the published generation before `camera_key_of`, when the mode
is consumer and the slot says published. The map camera goes into `self.view`, so the damage key,
the fetch zoom and the paint zoom follow it as they follow `look_at` today; the matrix is kept beside
it for the questions it answers. The one-frame staleness is what §11.1 already accepts, and the
cover's padding is what absorbs it.

**The matrices out.** `Frame` carries the mode and the matrix beside `projection`; `declare_if` takes
the mode; `CameraBlock` gains the `.for_mode(mode)` call it was written for; the Mercator branch of
`tile_matrix` emits placement alone with the depth nudge in `[14]` when the mode is consumer; and the
cover and label placement take the consumer's matrix where the mode says so. The 28 `tile_matrix`
call sites thread through one `projection` argument already, so the mode rides the same path rather
than touching each one.

**The C surface.** Two calls modeled on `tessella_set_projection`: one to set a view's camera mode,
one to publish a camera, which takes the matrix and the derived map camera together. `MapState` owns
the `ReverseChannel` rather than widening `tessella_map_regions`, which keeps a frozen four-word
struct frozen. That serves an in-process consumer, which is every consumer today. A cross-process one
would map the strip as a third region later, and nothing above changes when it does.

Producer-camera stays the default, so every existing consumer and every parity number is untouched
by construction.

## The same question, asked about one family

`3e526e8` gave fill extrusions their own Mercator projection. mbgl keeps two a frame and asks for
the near-clipped one in exactly one tweaker, because an extrusion is the one family that resolves in
the depth buffer and so the one that cares how much of it is left at the far end of a pitched view.
`near_clipped_tile_matrix` is what an extrusion drawable is placed by now.

Under a consumer's camera the producer does not build a projection at all, so it has no near plane
to move. Three ways out:

- **Say it on the wire.** A drawable that wants a near-clipped projection is marked, and the
  consumer builds a second projection for those draws. Honest, and it asks the consumer for
  arithmetic it may not want.
- **Let the consumer decide.** Its depth buffer, its precision problem: a 3D engine already has a
  near plane chosen for its own content, and an extrusion is no worse served by it than a model is.
- **Leave extrusions on the producer's projection.** They would then be placed in tessella's clip
  space while everything else is placed in the scene's, which is not a map in a world at all.

The decision above makes the second the consistent one: the consumer publishes the projection it
draws with, so the near plane in it is the one its depth buffer was sized for. The first stays open
for a consumer that wants mbgl's extrusions exactly, and a flag costs nothing if one ever does.

## What the consumer owes

Stated here so the obligation is agreed rather than discovered:

- Publish the camera every frame, before the tick that reads it: the matrix and the map camera
  derived from it, in one write.
- Multiply the scene's view-projection by the node's transform and then by the placement, in that
  order, in the vertex stage.
- Keep the depth nudge: it arrives in the placement's `[14]` as the globe's does, and it is what
  separates a fill from its outline.
- Decide what flat layers do against the scene's depth. They are painter-ordered among themselves
  and write no depth, so they need a depth *test* against scene objects or they will paint over a
  model standing on the map.

## What it is worth

The consumer measures itself against `mbgl-render` and holds the sweep at 92 / 232 / 19 / 141 / 154
gross pixels, `heat_p`, `hill_p` and `relief_p` at zero, and `puck_p` at 0 / 4 / 0. None of that
changes: producer-camera mode is untouched, and a consumer-camera view is measured by whether the
map lands where the scene's camera says it should, which is a different question from whether it
matches mbgl.

What it buys is the thing DR-9 was decided for and one more besides: pan-to-photon latency drops to
the consumer's own render latency, and a map becomes something a 3D engine can put in a world under
any camera it chooses, rather than composite over one.
