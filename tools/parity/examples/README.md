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
- `script` — what the example does *after* its style has loaded, which no stylesheet can say: a
  source handed new data, or the camera moved. Written in the render tests' own vocabulary, an
  array with the operation's name first, because `render-test/parser.cpp` already names these
  calls and a second spelling of them would be a second thing to keep right:

      "script": [["setData", "point", {"type": "Point", "coordinates": [0, 20]}],
                 ["setCenter", [-74.0, 40.7]], ["setZoom", 15.5],
                 ["setBearing", -17.6], ["setPitch", 45]]

  `compose.py` writes it beside the style and both renderers read *that one file* -- `--script`
  to the oracle, `TSF_SCRIPT` to the probe. One file rather than one each, because two renderers
  handed different instructions is the one failure a gross number cannot show.

  An example that animates is asked at the ends of its animation, and a script is how the far end
  is reached: `animate-a-point` moves its point a quarter turn around its circle.

  A `setData` may name a URL rather than a document, and `compose.py` reads it -- through the
  proxy, so a record run stores it and a replay serves it -- into the document it names. That is
  what lets an example whose document is fetched and large be a fixture at all:
  `update-a-feature-in-realtime` names a 462 KB hike the repository does not carry.

Adding on load and writing the same sources and layers into the document are the same style by the
time a frame is drawn, which is what lets these run without any call the header does not have.
Examples that mutate the style after load need `script` and the calls behind it: `setData` is
there, and the rest of the render tests' vocabulary is not yet.

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
