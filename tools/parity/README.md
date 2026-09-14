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
`assets.py` on 8081). `env.sh` says where each is expected and every path is an override.

## What the numbers mean

`gross N of M` counts pixels whose per-channel difference exceeds 48. Not a mean: a mean hides a
hundred wrong pixels among a million right ones, and the question is how many pixels a person
would call different. Pass 12 as a third argument to `gross.py` for the second lens, which is
what to use when a change is supposed to move nothing at all.

The sweep's numbers as of 2026-09-13, which is the gate:

    families_p  z14 p0    24 of 786432
    families_p  z14 p60   45
    families_p  z16 p0     5
    families_p  z16 p60   52
    families_p  z9  p0    30 of 2160000

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

      annot_p  z14 p0     4 of 786432
      annot_p  z16 p0     1
      annot_p  z14 p60  576

  z14 p60 is the one to look at, and it is not an annotation fault. It is the fill *outline*: on a
  pitched edge our line covers about a third of what the oracle's does, over the same two rows.
  No other scene here sets `fill-outline-color`, so this scene is the first thing to gate that
  path at all. Not in the sweep until it is closed.

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

## Why the scenes are here and the frames are not

A scene is a question and belongs with the code it asks about. A rendered frame is an answer to
one run of it, is megabytes, and is regenerated in seconds — including the oracle's, which
`parity.sh` re-renders every time rather than trusting a stored PNG whose camera nobody wrote
down. Everything a run produces goes to `PARITY_WORK`, outside the tree.

No tile or glyph data is vendored here. The scenes name servers; what those serve stays where it
is licensed.
