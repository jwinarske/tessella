# The MapLibre GL JS examples, as parity fixtures

The 139 pages at maplibre.org/maplibre-gl-js/docs/examples are demos of a JavaScript `Map`, not a
conformance suite. The ones whose content is a style plus a camera are asked here the way
`families_p` is: both renderers, the same camera, gross pixels.

    tools/parity/build.sh                       # once, as for the sweep
    PARITY_RECORD=1 tools/parity/examples.sh    # the first time: snapshot what the origins serve
    tools/parity/examples.sh [slug...]          # every time after: replay, no network

## A fixture

`<slug>/fixture.json`, named for `test/examples/<slug>.html` upstream:

- `base` — the style URL the example constructs its map with.
- `sources`, `layers` — what its `load` handler adds, in call order. `before` on a layer is the
  `addLayer` second argument; `first-text-symbol` is the "beneath the first label" idiom.
- `cameras` — settled frames to compare. An example that animates is asked at the ends of its
  animation, because `mbgl-render` draws a still picture.
- `size` — `[width, height]`, 1024x768 when absent.

Adding on load and writing the same sources and layers into the document are the same style by the
time a frame is drawn, which is what lets these run without any call the header does not have.
Examples that mutate the style after load wait for those calls, and for the oracle to replay them.

## The snapshot

Every URL either renderer reaches -- the base style, TileJSON, tiles, glyphs, sprites, GeoJSON --
goes through `proxy.py` on `PARITY_PROXY_PORT`. Recording stores what the origin served in
`PARITY_EXAMPLES_DATA`, outside the tree for the reason the parity README gives, and writes the
example's `manifest.txt`: status, content hash and URL for everything it served. Replay serves only
from the snapshot; a URL it does not hold is a `MISS` and fails the example, and a served set that
differs from the manifest is reported as drift.

The snapshot is keyed on a canonical spelling of the URL, because the two renderers escape a font
stack in a glyph URL differently and would otherwise record one range twice.

## Reading a number

A gross count here is the same metric as the sweep's and is recorded rather than required to be
zero. Where it is large, the question is the one every parity entry in `plan.md` asks: which
layer, which tile, which population -- not the total.
