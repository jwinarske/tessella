#!/usr/bin/env python3
# SPDX-License-Identifier: BSD-2-Clause
"""Turns an example's fixture into the style both renderers are handed, and lists its cameras.

    compose.py <fixture.json> <out-style.json>

An example is a base style plus what its `load` handler adds. Adding on load and writing the same
sources and layers into the document are the same style by the time a frame is drawn, so the
fixture says what was added and this writes it in -- which is what lets these run with no FFI.

Fixture fields:
  example   the upstream slug, `test/examples/<example>.html`
  base      the style URL the example constructs its map with, or null for none
  sources   sources added on load, by id
  layers    layers added on load, in call order; `before` is a layer id, or `first-text-symbol`
            for the "insert beneath the first label" idiom several examples use
  cameras   [{lat, lon, zoom, pitch, bearing}], each a settled frame to compare
  size      [width, height]; 1024x768 when absent, the sweep's street-zoom size
  script    what the example does after its style loads, as render-test operations --
            `["setData", id, <document>]` and the four camera properties. Written beside the
            style as `<out>.script.json`, and handed to both renderers: `--script` to the
            oracle, `TSF_SCRIPT` to the probe. Both read that one file, because a harness that
            lowered it into two would be free to lower it differently for each.

Prints one line per camera: `lat lon zoom width height pitch bearing`, parity.sh's arguments.
Every absolute URL, in the base and in what the fixture adds, is routed through the proxy.
"""

import json
import os
import sys
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from proxy import rewrite  # noqa: E402


def proxied(url: str) -> str:
    return rewrite(url.encode()).decode()


def insert_index(layers: list, before: str | None, example: str) -> int:
    if before is None:
        return len(layers)
    if before == "first-text-symbol":
        for index, layer in enumerate(layers):
            if layer.get("type") == "symbol" and "text-field" in layer.get("layout", {}):
                return index
        return len(layers)
    for index, layer in enumerate(layers):
        if layer.get("id") == before:
            return index
    sys.exit(f"{example}: no layer {before!r} to insert before")


def main() -> None:
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    with open(sys.argv[1]) as fixture_file:
        fixture = json.load(fixture_file)
    example = fixture.get("example", sys.argv[1])
    if not fixture.get("cameras"):
        sys.exit(f"{example}: a fixture needs at least one camera")

    if fixture.get("base"):
        base = proxied(fixture["base"])
        if not base.startswith("http://127.0.0.1:"):
            sys.exit(f"{example}: base {fixture['base']!r} is not an http or https URL")
        # The URL was just checked to be the loopback proxy, so no other scheme reaches urlopen.
        with urllib.request.urlopen(base, timeout=60) as response:  # noqa: S310
            style = json.loads(response.read())
    else:
        style = {"version": 8, "sources": {}, "layers": []}

    added = json.loads(
        rewrite(
            json.dumps(
                {"sources": fixture.get("sources", {}), "layers": fixture.get("layers", [])}
            ).encode()
        )
    )
    style.setdefault("sources", {}).update(added["sources"])

    layers = style.setdefault("layers", [])
    for layer in added["layers"]:
        before = layer.pop("before", None)
        layers.insert(insert_index(layers, before, example), layer)

    with open(sys.argv[2], "w") as out:
        json.dump(style, out, indent=1)

    # Beside the style, and only when the fixture has one: `examples.sh` hands over whichever
    # file is there, so an example that mutates nothing runs exactly as it did before.
    script_path = sys.argv[2] + ".script.json"
    if fixture.get("script"):
        script = json.loads(rewrite(json.dumps(fixture["script"]).encode()))
        with open(script_path, "w") as out:
            json.dump(script, out, indent=1)
    elif os.path.exists(script_path):
        # A fixture that had a script and lost one must not keep replaying the old one.
        os.remove(script_path)

    width, height = fixture.get("size", [1024, 768])
    for camera in fixture["cameras"]:
        # mbgl-render's argument parser reads a leading minus as a flag, so a bearing is written
        # in [0, 360) rather than as the example spells it.
        bearing = camera.get("bearing", 0) % 360
        print(
            camera["lat"],
            camera["lon"],
            camera["zoom"],
            width,
            height,
            camera.get("pitch", 0),
            f"{bearing:g}",
        )


if __name__ == "__main__":
    main()
