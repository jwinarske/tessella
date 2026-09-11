// The four-view zoom sweep, run where a browser would run a map: off the main thread.
//
// Producer, fetch loop and consumer all live here. One instance of the module holds four hosted
// maps; one WebGL2 context on one transferred canvas draws them into four panes and presents
// them together, so the views are frame-locked by construction rather than by coordination.
//
// # What a frame is
//
//   answer what has landed -> advance x4 -> camera x4 -> tick x4 -> drain and absorb x4
//     -> take requests -> draw four panes
//
// Timed in four parts: each map's tick, the consumer's drain and absorb, and the draw calls. The
// maps share built tiles, so the first to reach a new tile in a frame builds it and the others
// find it built: the ticks are uneven by design, and the first map's carries most of a crossing.
// On one thread what the frame spends is their sum. A maximum would be the cost only if four
// producers ran at once, and here they cannot.
//
// # Two clocks
//
// Clock A tells every map a fixed step whatever the wall did, and waits for every fetch before the
// next frame: the same answers land in the same frames every run, so the stream is reproducible
// and can be diffed against a native serial run. Clock B tells the maps the time that actually
// passed and answers fetches when they land: that is what somebody looking at it gets.

import { TessellaMap } from "./map.js";
import { FillRenderer } from "./gl.js";
import { caching, MemoryStore } from "./cache.js";
import { FetchHost } from "./bench/fetch-host.js";
import { Scene } from "./bench/scene.js";
import { fingerprint, Fnv64 } from "./bench/trace.js";
import { KIND } from "./abi.js";
import { fourViews, sweepZooms, SWEEP_LOW } from "./bench/sweep.js";
import { summary } from "./bench/stats.js";

const OK = 0;
const RING_FULL = 5;
const REGION_FULL = 7;

/** The clear color, and the style's background: a hole in any pane is unmistakable. */
const HOLE = [1, 0, 1, 1];

const say = (line) => postMessage({ type: "line", line });

self.onmessage = async ({ data }) => {
  if (data.type !== "start") {
    return;
  }
  try {
    postMessage({ type: "done", report: await run(data.canvas, data.config) });
  } catch (error) {
    postMessage({ type: "error", error: String(error && error.stack ? error.stack : error) });
  }
};

/** The next frame: the worker's own vsync where it has one. */
const pacing = typeof self.requestAnimationFrame === "function" ? "raf" : "timeout";
const nextFrame =
  pacing === "raf"
    ? () => new Promise((resolve) => self.requestAnimationFrame(resolve))
    : () => new Promise((resolve) => setTimeout(resolve, 0));

async function run(canvas, config) {
  const {
    clock,
    steps,
    tickMs,
    trace,
    moduleUrl,
    styleUrl,
    tileOrigin,
    pane,
    ringBytes,
    slabBytes,
    samples,
    maxSettle,
    grow,
  } = config;

  const moduleBytes = await (await fetch(moduleUrl)).arrayBuffer();
  const style = await (await fetch(styleUrl)).text();
  const coverageLayer = JSON.parse(style).layers.findIndex((layer) => layer.id === "earth");

  // One instance, four maps. The pool, the memory and the scratch block are the instance's; the
  // rings, slab regions and fetch tables are each map's.
  const { instance } = await WebAssembly.instantiate(moduleBytes, {});
  const memory = instance.exports.memory;
  const views = fourViews(pane.width, pane.height);
  const maps = views.map((view) =>
    TessellaMap.hosted(instance, style, {
      latitude: view.latitude,
      longitude: view.longitude,
      zoom: SWEEP_LOW,
      width: pane.width,
      height: pane.height,
      ringBytes,
      slabBytes,
    }),
  );
  const scenes = maps.map(() => new Scene());

  const gl = canvas?.getContext("webgl2", { antialias: false, depth: false, stencil: false });
  const renderers = gl
    ? maps.map(() => {
        const renderer = new FillRenderer(gl);
        renderer.background = HOLE;
        return renderer;
      })
    : null;
  // Pane 0 top left, reading order after it. GL's origin is the bottom left.
  const panes = views.map((_, i) => ({
    x: (i % 2) * pane.width,
    y: (1 - Math.floor(i / 2)) * pane.height,
    width: pane.width,
    height: pane.height,
  }));

  // The style names an origin that does not exist, so the native driver can use the same
  // document byte for byte. The host puts the real one in.
  const network = async (url) => {
    const response = await fetch(url);
    return {
      status: response.status,
      body: new Uint8Array(await response.arrayBuffer()),
      headers: response.headers,
    };
  };
  const host = new FetchHost(caching(network, new MemoryStore()), {
    rewrite: (url) => url.replace("http://tiles.invalid", tileOrigin),
  });

  const lines = [];
  let lastStart = null;

  /** One frame. Returns its counters. */
  async function frame(index, phase, zoom) {
    await nextFrame();
    const started = performance.now();
    const rafInterval = lastStart === null ? null : started - lastStart;
    const elapsed = clock === "A" || lastStart === null ? tickMs : started - lastStart;
    lastStart = started;

    host.deliver(maps);
    for (const map of maps) {
      map.advance(elapsed);
    }
    maps.forEach((map, i) => map.setCamera(views[i].latitude, views[i].longitude, zoom));

    const tick = [];
    const status = [];
    for (const map of maps) {
      const t = performance.now();
      status.push(map.step());
      tick.push(performance.now() - t);
    }

    let t = performance.now();
    const records = maps.map((map) => map.ring.drain());
    if (renderers) {
      maps.forEach((map, i) => renderers[i].absorb(map, records[i]));
    }
    const absorb = performance.now() - t;

    // Outside every timed span: none of this is work a consumer does.
    const prints = trace ? maps.map((map, i) => fingerprint(map, records[i])) : null;
    if (index === config.records || config.records === "all") {
      maps.forEach((map, i) =>
        records[i].forEach((record) => say(`RECORD ${index} ${i} ${describeRecord(map, record)}`)),
      );
    }
    maps.forEach((_, i) => scenes[i].absorb(memory, records[i]));
    const covered = scenes.map((scene) => scene.covered(coverageLayer, samples[0], samples[1]));
    const fills = scenes.map((scene) => scene.fills());
    const pending = maps.map((map) => map.pending);

    host.collect(maps);

    let drawn = null;
    t = performance.now();
    if (renderers) {
      drawn = renderers.map((renderer, i) => {
        const p = panes[i];
        return renderer.drawViewport(p.x, p.y, p.width, p.height);
      });
    }
    const draw = performance.now() - t;
    const fetches = host.takeFrameCounters();

    const line = {
      frame: index,
      phase,
      clock,
      zoom,
      tick_ms: tick.map(round),
      absorb_ms: round(absorb),
      draw_ms: round(draw),
      raf_interval_ms: rafInterval === null ? null : round(rafInterval),
      records: records.reduce((n, r) => n + r.length, 0),
      status,
      ring_full: status.filter((s) => s === RING_FULL).length,
      region_full: status.filter((s) => s === REGION_FULL).length,
      // Anything else a tick can answer is the map failing, not the consumer being behind.
      tick_errors: status.filter((s) => s !== OK && s !== RING_FULL && s !== REGION_FULL).length,
      fills,
      drawn,
      blank: fills.filter((n) => n === 0).length,
      covered,
      pending,
      fetches: fetches.fetches,
      fetch_hits_coalesced: fetches.coalesced,
      cache_hits: fetches.cacheHits,
      tickets: fetches.tickets,
      mem_pages: memory.buffer.byteLength >> 16,
      gpu_ms: null,
    };
    lines.push(line);
    say("FRAME " + JSON.stringify(line));
    if (prints) {
      say(
        "TRACE " +
          JSON.stringify({
            frame: index,
            phase,
            maps: prints.map((print, i) => ({ status: status[i], pending: pending[i], ...print })),
          }),
      );
    }

    // Run A waits for every answer before the next frame, which is what makes the frames the
    // answers land in a property of the run rather than of the network.
    if (clock === "A") {
      await host.settle();
    }
    return { records: line.records, pending };
  }

  // Cold start at the bottom of the sweep, until every map is quiet: nothing emitted, nothing
  // outstanding. Not gated -- a cold map is blank until its first tiles land, and that is the
  // settle number rather than a failure.
  let index = 0;
  let settleFrames = null;
  for (; index < maxSettle; index++) {
    const { records, pending } = await frame(index, "settle", SWEEP_LOW);
    if (records === 0 && pending.every((p) => p === 0) && !host.busy) {
      settleFrames = index + 1;
      index++;
      break;
    }
  }
  if (settleFrames === null) {
    throw new Error(`the four maps did not settle within ${maxSettle} frames`);
  }

  // The memory grows under the consumer before the sweep starts. `memory.buffer` is replaced when
  // it does, and any view held across it is detached -- so a consumer that kept one fails the
  // first sweep frame here, rather than at whatever point the producer first grows mid-sweep.
  let growth = null;
  if (grow) {
    const before = memory.buffer.byteLength >> 16;
    memory.grow(1);
    growth = { before, after: memory.buffer.byteLength >> 16 };
  }

  const zooms = sweepZooms(steps);
  for (const zoom of zooms) {
    await frame(index++, "sweep", zoom);
  }

  // The pixels, where there is a GL to have drawn them: the last frame drawn again and read back
  // in the same task, before it is presented and the buffer is gone. Each pane's count of the
  // clear color is its holes.
  let pixels = null;
  if (renderers) {
    const holes = panes.map((p, i) => {
      renderers[i].drawViewport(p.x, p.y, p.width, p.height);
      const out = new Uint8Array(p.width * p.height * 4);
      gl.readPixels(p.x, p.y, p.width, p.height, gl.RGBA, gl.UNSIGNED_BYTE, out);
      let count = 0;
      for (let k = 0; k < out.length; k += 4) {
        if (out[k] === 255 && out[k + 1] === 0 && out[k + 2] === 255) {
          count++;
        }
      }
      return count;
    });
    pixels = { holes, panePixels: pane.width * pane.height };
  }

  const sweep = lines.filter((line) => line.phase === "sweep");
  const sum = (xs) => xs.reduce((a, b) => a + b, 0);
  const report = {
    clock,
    steps,
    tick_ms_step: clock === "A" ? tickMs : null,
    frames: sweep.length,
    settle_frames: settleFrames,
    pacing,
    webgl2: !!gl,
    scene: {
      style: "web/bench/style.json",
      fixture: "tests/mvt-fixtures/protomaps-berlin-14-8802-5373.mvt, served at every tile",
      coverage_layer: coverageLayer,
      views,
      zooms,
      pane,
      ring_bytes: ringBytes,
      slab_bytes: slabBytes || "default",
    },
    timings: {
      tick_ms_sum: summary(sweep.map((l) => sum(l.tick_ms))),
      tick_ms_max: summary(sweep.map((l) => Math.max(...l.tick_ms))),
      tick_ms_by_map: [0, 1, 2, 3].map((i) => summary(sweep.map((l) => l.tick_ms[i]))),
      absorb_ms: summary(sweep.map((l) => l.absorb_ms)),
      draw_ms: summary(sweep.map((l) => l.draw_ms)),
      // What the frame spends on one thread.
      frame_cpu_ms: summary(sweep.map((l) => sum(l.tick_ms) + l.absorb_ms + l.draw_ms)),
      // The slowest tick instead of their sum, reported beside it. It is the frame's cost only if
      // the four ticks could overlap, which on one thread they cannot.
      max_tick_frame_ms: summary(
        sweep.map((l) => Math.max(...l.tick_ms) + l.absorb_ms + l.draw_ms),
      ),
      raf_interval_ms: summary(sweep.map((l) => l.raf_interval_ms)),
    },
    ring_full: sum(sweep.map((l) => l.ring_full)),
    region_full: sum(sweep.map((l) => l.region_full)),
    tick_errors: sum(lines.map((l) => l.tick_errors)),
    blank: sum(sweep.map((l) => l.blank)),
    uncovered_frames: sweep.filter((l) => l.covered.some((c) => !c)).length,
    fetch: {
      tickets: host.totals.tickets,
      distinct_urls: host.distinct.size,
      origin_fetches: host.totals.fetches,
      coalesced: host.totals.coalesced,
      cache_hits: host.totals.cacheHits,
      failed: host.totals.failed,
      sharing_ratio: host.distinct.size === 0 ? 1 : round(host.totals.tickets / host.distinct.size),
    },
    mem_pages_peak: Math.max(...lines.map((l) => l.mem_pages)),
    timer: { isolated: self.crossOriginIsolated === true, resolution_ms: timerResolution() },
    growth,
    pixels,
    gpu_ms: null,
  };
  return report;
}

/**
 * The smallest step `performance.now()` takes here.
 *
 * Reported because it bounds every number above it. A browser coarsens the clock unless the page
 * is cross-origin isolated -- Firefox to a whole millisecond -- and a tick timed with a
 * millisecond clock reads 0 or 1 whatever it cost.
 */
function timerResolution() {
  let smallest = Infinity;
  const began = performance.now();
  let last = began;
  // Bounded by the clock itself: a few milliseconds of steps is enough to see the smallest, and a
  // clock that never moves at all is reported as never having stepped.
  while (last - began < 5 && smallest > 0.001) {
    const now = performance.now();
    if (now > last) {
      smallest = Math.min(smallest, now - last);
      last = now;
    }
  }
  return Number.isFinite(smallest) ? round(smallest) : null;
}

/** One record, as `sweep_trace --records` prints it, for finding what a trace diff is about. */
function describeRecord(map, record) {
  const kind = Object.keys(KIND).find((name) => KIND[name] === record.kind) ?? record.kind;
  const fixed = new Uint8Array(map.memory.buffer, record.fixedAt, record.recordLen);
  const one = new Fnv64()
    .bytes(fixed)
    .bytes(new Uint8Array(map.memory.buffer, record.payloadAt, record.payloadLen));
  const hex = [...fixed].map((b) => b.toString(16).padStart(2, "0")).join("");
  return `${kind} ${record.recordLen} ${record.payloadLen} ${one.hex()} ${hex}`;
}

function round(x) {
  return Math.round(x * 1000) / 1000;
}
