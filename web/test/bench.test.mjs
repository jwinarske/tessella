// The benchmark's pieces, without a browser.
//
// `bench.mjs` runs the sweep under Firefox and holds it to the acceptance table; these check the
// parts that table leans on, each against something it can be wrong about: the sweep port against
// the formula it came from, the hash against published vectors, the fetch host against a double
// fetch, coverage against a hole. And four maps in one instance, which is the arrangement the
// benchmark rests on and which nothing else runs.
//
// Run with `node --test web/test`.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";

import { TessellaMap } from "../map.js";
import { KIND, LAYOUT, BUILTIN } from "../abi.js";
import { caching, MemoryStore } from "../cache.js";
import { fourViews, sweepZooms, SWEEP_LOW, SWEEP_HIGH } from "../bench/sweep.js";
import { Fnv64, cameraDistance } from "../bench/trace.js";
import { FetchHost } from "../bench/fetch-host.js";
import { Scene } from "../bench/scene.js";
import { summary } from "../bench/stats.js";
import { evaluate, absent } from "../bench/gates.js";

const root = fileURLToPath(new URL("../..", import.meta.url));

test("the sweep goes up and back once, visiting the top once", () => {
  const zooms = sweepZooms(33);
  assert.equal(zooms.length, 65);
  assert.equal(zooms[0], SWEEP_LOW);
  assert.equal(zooms[32], SWEEP_HIGH);
  assert.equal(zooms[64], SWEEP_LOW);
  assert.equal(zooms.filter((z) => z === SWEEP_HIGH).length, 1);
  assert.deepEqual(zooms.slice(33), zooms.slice(0, 32).reverse());
  // `steps.max(2)`: fewer is the two ends.
  assert.deepEqual(sweepZooms(1), [SWEEP_LOW, SWEEP_HIGH, SWEEP_LOW]);
});

test("the views are offset unequally on the two axes", () => {
  const [a, b, c, d] = fourViews(640, 480);
  assert.equal(b.longitude, a.longitude + 0.01);
  assert.equal(c.latitude, a.latitude + 0.006);
  assert.deepEqual([d.longitude, d.latitude], [b.longitude, c.latitude]);
  assert.ok(fourViews(640, 480).every((v) => v.width === 640 && v.height === 480));
});

test("the hash is FNV-1a 64", () => {
  const hash = (text) => new Fnv64().bytes(new TextEncoder().encode(text)).hex();
  assert.equal(hash(""), "cbf29ce484222325");
  assert.equal(hash("a"), "af63dc4c8601ec8c");
  assert.equal(hash("foobar"), "85944171f73967e8");
});

test("a camera differing in its doubles is measured in ulps, and in any other byte is not", () => {
  const size = LAYOUT.tsl_camera_update.size;
  const base = new Uint8Array(size);
  const view = new DataView(base.buffer);
  view.setFloat64(80, 1.5, true);
  const hex = (bytes) => [...bytes].map((b) => b.toString(16).padStart(2, "0")).join("");

  const nudged = base.slice();
  const bits = new DataView(nudged.buffer);
  bits.setBigUint64(80, bits.getBigUint64(80, true) + 3n, true);
  assert.deepEqual(cameraDistance(hex(base), hex(nudged)), { ulps: 3, otherBytes: 0 });

  const other = base.slice();
  other[LAYOUT.tsl_camera_update.at.frame_no] = 1;
  assert.deepEqual(cameraDistance(hex(base), hex(other)), { ulps: 0, otherBytes: 1 });
});

test("percentiles index the sorted samples without interpolating", () => {
  const samples = Array.from({ length: 101 }, (_, i) => 100 - i);
  assert.deepEqual(summary(samples), { n: 101, p50: 50, p95: 95, p99: 99, max: 100 });
  assert.equal(summary([null, NaN]), null);
});

/** A map as the fetch host sees one: requests out, answers in. */
function fakeMap(requests) {
  const answered = [];
  return {
    answered,
    takeRequests: () => requests.splice(0),
    answer: (ticket, status, body) => answered.push({ ticket, status, body: [...body] }),
    fail: (ticket) => answered.push({ ticket, failed: true }),
  };
}

test("two maps asking for one url cost one fetch, and both are answered in the order they asked", async () => {
  let calls = 0;
  const host = new FetchHost(
    async (url) => {
      calls++;
      return { status: 200, body: new TextEncoder().encode(url) };
    },
    { rewrite: (url) => url.replace("http://tiles.invalid", "") },
  );
  const a = fakeMap([
    [1n, "http://tiles.invalid/a"],
    [2n, "http://tiles.invalid/b"],
  ]);
  const b = fakeMap([[7n, "http://tiles.invalid/a"]]);

  host.collect([a, b]);
  assert.equal(host.busy, true);
  await host.settle();
  host.deliver([a, b]);

  assert.equal(calls, 2);
  assert.deepEqual(
    a.answered.map((x) => x.ticket),
    [1n, 2n],
  );
  assert.deepEqual(
    b.answered.map((x) => x.ticket),
    [7n],
  );
  assert.deepEqual(b.answered[0].body, a.answered[0].body);
  assert.deepEqual(new TextDecoder().decode(new Uint8Array(a.answered[0].body)), "/a");
  assert.equal(host.busy, false);
  assert.deepEqual(host.totals, { tickets: 3, fetches: 2, coalesced: 1, cacheHits: 0, failed: 0 });
});

test("a url asked for again after it landed is answered from the cache, not the origin", async () => {
  let calls = 0;
  const origin = async () => {
    calls++;
    return {
      status: 200,
      body: new Uint8Array([1]),
      headers: new Headers({ "cache-control": "max-age=60" }),
    };
  };
  const host = new FetchHost(caching(origin, new MemoryStore()));
  const requests = [[1n, "u"]];
  const map = fakeMap(requests);
  host.collect([map]);
  await host.settle();
  host.deliver([map]);
  requests.push([2n, "u"]);
  host.collect([map]);
  await host.settle();
  host.deliver([map]);

  assert.equal(calls, 1);
  assert.deepEqual(
    map.answered.map((x) => x.ticket),
    [1n, 2n],
  );
  assert.deepEqual(host.totals, { tickets: 2, fetches: 1, coalesced: 0, cacheHits: 1, failed: 0 });
  assert.equal(host.distinct.size, 1);
});

test("a fetch still in the air is not answered, and one that could not happen is failed", async () => {
  let release;
  const slow = new Promise((resolve) => (release = resolve));
  const host = new FetchHost((url) =>
    url === "slow" ? slow : Promise.reject(new Error("no connection")),
  );
  const map = fakeMap([
    [1n, "slow"],
    [2n, "broken"],
  ]);
  host.collect([map]);
  await new Promise((resolve) => setTimeout(resolve, 0));
  host.deliver([map]);
  assert.deepEqual(map.answered, [{ ticket: 2n, failed: true }]);

  release({ status: 200, body: new Uint8Array(0) });
  await host.settle();
  host.deliver([map]);
  assert.deepEqual(
    map.answered.map((x) => x.ticket),
    [2n, 1n],
  );
  assert.equal(host.totals.failed, 1);
});

/**
 * A fake memory holding one `tsl_stencil_tiles` record over the given matrices, and a draw list
 * naming geometry of the given shaders.
 */
function sceneRecords(matrices, shaders) {
  const memory = new WebAssembly.Memory({ initial: 1 });
  const view = new DataView(memory.buffer);
  const records = [];
  let at = 0;
  const reserve = (bytes) => {
    const start = at;
    at += Math.ceil(bytes / 8) * 8;
    return start;
  };

  // The clip set.
  const stencil = reserve(LAYOUT.tsl_stencil_tiles.size);
  const tiles = reserve(matrices.length * LAYOUT.tsl_stencil_tile.size);
  view.setInt32(stencil + LAYOUT.tsl_stencil_tiles.at.layer_index, 1, true);
  view.setUint32(stencil + LAYOUT.tsl_stencil_tiles.at.tiles + LAYOUT.tsl_span.at.offset, 0, true);
  view.setUint32(
    stencil + LAYOUT.tsl_stencil_tiles.at.tiles + LAYOUT.tsl_span.at.count,
    matrices.length,
    true,
  );
  matrices.forEach((m, i) =>
    m.forEach((value, k) =>
      view.setFloat32(
        tiles + i * LAYOUT.tsl_stencil_tile.size + LAYOUT.tsl_stencil_tile.at.matrix + k * 4,
        value,
        true,
      ),
    ),
  );
  records.push({ kind: KIND.STENCIL_TILES, fixedAt: stencil, payloadAt: tiles });

  // The geometry, and a draw list naming all of it.
  shaders.forEach((shader, i) => {
    const add = reserve(LAYOUT.tsl_geometry_add.size);
    view.setBigUint64(add + LAYOUT.tsl_geometry_add.at.geometry, BigInt(i + 1), true);
    view.setInt32(add + LAYOUT.tsl_geometry_add.at.builtin_shader, shader, true);
    records.push({ kind: KIND.GEOMETRY_ADD, fixedAt: add });
  });
  const order = reserve(LAYOUT.tsl_order_update.size);
  const entries = reserve(shaders.length * LAYOUT.tsl_order_entry.size);
  view.setUint32(
    order + LAYOUT.tsl_order_update.at.entries + LAYOUT.tsl_span.at.count,
    shaders.length,
    true,
  );
  shaders.forEach((_, i) =>
    view.setBigUint64(
      entries + i * LAYOUT.tsl_order_entry.size + LAYOUT.tsl_order_entry.at.geometry,
      BigInt(i + 1),
      true,
    ),
  );
  records.push({ kind: KIND.ORDER_UPDATE, fixedAt: order, payloadAt: entries });
  return { memory, records };
}

/** A column-major matrix taking the tile's 0..8192 square to clip x in [x0, x1], y in [y0, y1]. */
function placing(x0, x1, y0, y1) {
  const m = new Array(16).fill(0);
  m[0] = (x1 - x0) / 8192;
  m[5] = (y0 - y1) / 8192; // tile y runs down, clip y up
  m[10] = 1;
  m[12] = x0;
  m[13] = y1;
  m[15] = 1;
  return m;
}

test("a view is covered when its tiles tile clip space, and not when one is missing", () => {
  const whole = [
    placing(-1, 0, -1, 0),
    placing(0, 1, -1, 0),
    placing(-1, 0, 0, 1),
    placing(0, 1, 0, 1),
  ];
  const covered = sceneRecords(whole, []);
  const scene = new Scene();
  scene.absorb(covered.memory, covered.records);
  assert.equal(scene.covered(1, 32, 24), true);

  const holed = sceneRecords(whole.slice(0, 3), []);
  const missing = new Scene();
  missing.absorb(holed.memory, holed.records);
  assert.equal(missing.covered(1, 32, 24), false);
  // A layer with no clip set says nothing is covered, rather than that everything is.
  assert.equal(scene.covered(2, 32, 24), false);
});

test("a view whose draw list names no live fill is blank", () => {
  const filled = sceneRecords([], [BUILTIN.BACKGROUND_SHADER, BUILTIN.FILL_SHADER]);
  const scene = new Scene();
  scene.absorb(filled.memory, filled.records);
  assert.equal(scene.fills(), 1);

  const background = sceneRecords([], [BUILTIN.BACKGROUND_SHADER]);
  const blank = new Scene();
  blank.absorb(background.memory, background.records);
  assert.equal(blank.fills(), 0);
});

test("the gates fail what they are for and name what is not measured", () => {
  const report = {
    clock: "A",
    timings: { frame_cpu_ms: { p99: 40 } },
    uncovered_frames: 1,
    ring_full: 0,
    region_full: 0,
    blank: 0,
    fetch: { origin_fetches: 5, distinct_urls: 4, failed: 0 },
    pixels: null,
  };
  const rows = Object.fromEntries(evaluate(report).map((row) => [row.property, row]));
  assert.equal(rows.coverage.pass, false);
  assert.equal(rows.flatness.pass, false);
  assert.equal(rows["blank frames"].pass, true);
  // Clock A says nothing about the budget, and a budget is never gated unless asked.
  assert.equal(rows["frame budget"].pass, null);
  assert.equal(evaluate({ ...report, clock: "B" })[0].gating, false);
  assert.ok(absent(report).includes("PIXELS NOT CHECKED: no webgl2 here"));
  assert.ok(absent(report).some((line) => line.startsWith("SYMBOL POPS: NOT MEASURED")));
});

test("a run with no sweep frames is reported rather than crashed on", () => {
  // What the settle protocol asks for: a cold start, its quiet tail, and no sweep at all. Every
  // percentile summary is null then, and both the page and the harness read them.
  const report = {
    clock: "B",
    frames: 0,
    settle_frames: 4,
    settle: { frames: 4, quiet_frames: 10, tail_records: 0, first_quiet_ms: 92, wall_ms: 250 },
    timings: { frame_cpu_ms: null, tick_ms_sum: null, absorb_ms: null, draw_ms: null },
    uncovered_frames: 0,
    ring_full: 0,
    region_full: 0,
    blank: 0,
    tick_errors: 0,
    fetch: { origin_fetches: 12, distinct_urls: 12, failed: 0 },
    pixels: null,
  };
  const rows = Object.fromEntries(evaluate(report).map((row) => [row.property, row]));
  // Nothing to take a percentile of is not a budget failure.
  assert.equal(rows["frame budget"].pass, null);
  assert.equal(rows["frame budget"].value, null);
  assert.equal(rows.coverage.pass, true);
  assert.ok(absent(report).length > 0);
});

test("four hosted maps share one instance, and each draws its own view of the sweep's scene", async () => {
  const module = await readFile(`${root}target/wasm32-unknown-unknown/release/tessella_ffi.wasm`);
  const tile = await readFile(`${root}tests/mvt-fixtures/protomaps-berlin-14-8802-5373.mvt`);
  const style = await readFile(`${root}web/bench/style.json`, "utf8");
  const { instance } = await WebAssembly.instantiate(module, {});
  const views = fourViews(640, 480);
  const maps = views.map((v) => TessellaMap.hosted(instance, style, { ...v, zoom: SWEEP_LOW }));
  const scenes = maps.map(() => new Scene());
  let fetched = 0;
  const host = new FetchHost(async () => {
    fetched++;
    return { status: 200, body: tile };
  });

  let settled = false;
  for (let frame = 0; frame < 60 && !settled; frame++) {
    host.deliver(maps);
    maps.forEach((map) => assert.equal(map.step(), 0));
    const records = maps.map((map) => map.ring.drain());
    maps.forEach((map, i) => scenes[i].absorb(map.memory, records[i]));
    settled =
      records.every((r) => r.length === 0) && maps.every((m) => m.pending === 0) && frame > 0;
    host.collect(maps);
    await host.settle();
  }

  assert.ok(settled, "the four maps never settled");
  assert.ok(
    scenes.every((s) => s.covered(1, 32, 24)),
    "a view was left with a hole",
  );
  assert.ok(
    scenes.every((s) => s.fills() > 0),
    "a view settled blank",
  );
  // Overlapping views ask for the same tiles, and the host fetched each once.
  assert.equal(fetched, host.distinct.size);
  assert.ok(host.totals.coalesced > 0, "four overlapping views shared no fetch");
  // Distinct maps, distinct rings: one instance does not mean one stream.
  assert.equal(new Set(maps.map((m) => m.ring.pointer)).size, 4);
  maps.forEach((map) => map.destroy());
});
