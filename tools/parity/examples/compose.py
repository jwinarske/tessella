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
            A `setData` may name a URL instead of a document; it is read here, through the
            proxy, into the document it names -- which is what lets an example whose document is
            fetched and large be a fixture at all. An `addImage` names a URL too, and its picture
            is fetched to a file beside the style, because bytes cannot live in the script.

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


def resolved(script: list, example: str) -> list:
    """`setData` by URL, read once here into the document it names.

    Both renderers are handed one file, so whatever a `setData` names has to be in it. A URL
    could be left for each to fetch -- the oracle does that with `GeoJSONSource::setURL` -- but
    the probe has no file source of its own, and giving it one would mean two fetches of one
    document, each free to get something else. Reading it here keeps the fixture declarative: it
    names the document the example fetches, and the snapshot holds the bytes.
    """
    out = []
    for operation in script:
        if len(operation) >= 3 and operation[0] == "addImage" and isinstance(operation[2], str):
            # A picture is bytes and cannot live in the script, so it is fetched to a file beside
            # the style and the operation names that. Both renderers then read one file, which is
            # the rule the rest of the script follows.
            url = proxied(operation[2])
            if not url.startswith("http://127.0.0.1:"):
                sys.exit(f"{example}: addImage {operation[2]!r} is not an http or https URL")
            # The URL was just checked to be the loopback proxy, so no other scheme reaches here.
            with urllib.request.urlopen(url, timeout=60) as response:  # noqa: S310
                picture = response.read()
            path = f"{sys.argv[2]}.{operation[1]}.image"
            with open(path, "wb") as image:
                image.write(picture)
            out.append([operation[0], operation[1], path, *operation[3:]])
            continue
        if len(operation) == 3 and operation[0] == "setData" and isinstance(operation[2], str):
            # Through the proxy, as everything else is: a record run stores it and a replay
            # serves it, so the document is the one the example was recorded against.
            url = proxied(operation[2])
            if not url.startswith("http://127.0.0.1:"):
                sys.exit(f"{example}: setData {operation[2]!r} is not an http or https URL")
            # The URL was just checked to be the loopback proxy, so no other scheme reaches here.
            with urllib.request.urlopen(url, timeout=60) as response:  # noqa: S310
                document = json.loads(response.read())
            out.append([operation[0], operation[1], document])
            continue
        out.append(operation)
    return out


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
        script = resolved(json.loads(rewrite(json.dumps(fixture["script"]).encode())), example)
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
