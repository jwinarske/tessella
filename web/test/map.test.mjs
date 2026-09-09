// Drives a real map, from the real module, with no browser.
//
// §19.2 wants a consumer that reads the ring out of `memory.buffer` to prove the memory-view
// story end to end. This is that, minus the drawing: the module is the one CI builds, the style
// and the tile are the fixtures the Rust suite uses, and every fetch is answered from disk. What
// a browser adds is `fetch` and a canvas.
//
// Run with `node --test web/test`.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";

import { TessellaMap } from "../map.js";
import { geometryAdd } from "../ring.js";
import { KIND } from "../abi.js";

const root = fileURLToPath(new URL("../..", import.meta.url));
const MODULE = `${root}target/wasm32-unknown-unknown/release/tessella_ffi.wasm`;
const TILE = `${root}tests/mvt-fixtures/real-world-0-0-0.mvt`;

const STYLE = JSON.stringify({
  version: 8,
  sources: {
    v: {
      type: "vector",
      tiles: ["http://host.invalid/{z}/{x}/{y}.pbf"],
      minzoom: 0,
      maxzoom: 6,
    },
  },
  layers: [
    { id: "bg", type: "background", paint: { "background-color": "#101418" } },
    {
      id: "water",
      type: "fill",
      source: "v",
      "source-layer": "water",
      paint: { "fill-color": "#3050c0" },
    },
  ],
});

/** The host: one tile fixture, and a 404 for anything else. */
async function origin(tile) {
  const bytes = await readFile(tile);
  const asked = [];
  return {
    asked,
    async fetch(url) {
      asked.push(url);
      if (url.endsWith(".pbf")) {
        return { status: 200, body: new Uint8Array(bytes) };
      }
      // An origin saying no, which is an answer: the map draws around a hole rather than failing.
      return { status: 404, body: new Uint8Array(0) };
    },
  };
}

test("a map draws from bytes the host fetched, read out of linear memory", async () => {
  const module = await readFile(MODULE);
  const host = await origin(TILE);
  const map = await TessellaMap.create(module, STYLE, {
    latitude: 0,
    longitude: 0,
    zoom: 0,
    width: 512,
    height: 512,
  });

  const seen = [];
  // Bounded, so a map that never settles fails rather than hangs.
  for (let frame = 0; frame < 400; frame++) {
    seen.push(...(await map.tick(host.fetch)));
    if (seen.some((record) => record.kind === KIND.GEOMETRY_ADD) && map.pending === 0) {
      break;
    }
  }

  assert.ok(host.asked.length > 0, "the map never asked the host for anything");
  assert.ok(
    host.asked.some((url) => url.endsWith("/0/0/0.pbf")),
    `the tile was never asked for: ${host.asked}`,
  );

  const geometry = seen.filter((record) => record.kind === KIND.GEOMETRY_ADD);
  assert.ok(geometry.length > 0, `no geometry was published; saw ${seen.length} records`);

  // The point of the whole arrangement: the vertices are in the producer's memory and this side
  // reads them where they lie. Resolving the index slab is what proves the handle-to-bytes rule
  // holds across the ABI rather than only inside Rust.
  let vertices = 0;
  let indexed = 0;
  for (const record of geometry) {
    const add = geometryAdd(map.memory, record);
    vertices += add.vertexCount;
    const indexes = map.slabs.resolve(add.indexes);
    assert.ok(indexes !== null, "an index slab handle resolved to nothing");
    indexed += indexes.length;
  }
  assert.ok(vertices > 0, "geometry was published with no vertices in it");
  assert.ok(indexed > 0, "no index bytes were reachable through the slab table");
  // Zero, and asserted rather than assumed. `tessella_pending` reports through a pointer and
  // returns a status, so calling it as though it returned the count answers `1` for ever --
  // `NullArgument` -- which is indistinguishable from a map with one thing stuck in flight. This
  // is the assertion that tells the two apart.
  assert.equal(map.pending, 0, "the map still reports work in flight after settling");

  map.destroy();
});

test("a map with nothing to fetch asks for nothing", async () => {
  const module = await readFile(MODULE);
  const asked = [];
  const map = await TessellaMap.create(
    module,
    JSON.stringify({
      version: 8,
      sources: {},
      layers: [{ id: "bg", type: "background", paint: { "background-color": "#101418" } }],
    }),
    { width: 256, height: 256 },
  );

  for (let frame = 0; frame < 8; frame++) {
    await map.tick(async (url) => {
      asked.push(url);
      return { status: 404, body: new Uint8Array(0) };
    });
  }
  assert.deepEqual(asked, [], "a style with no sources went to the network");
  map.destroy();
});
