// The four-view zoom sweep, under headless Firefox, held to its acceptance table.
//
//   node web/test/bench.mjs                      run A, diffed against native; the CI form
//   node web/test/bench.mjs --clock B --runs 6   run B six times, interleaved with native
//   node web/test/bench.mjs --clock B --budget   ... and gate on the frame budget
//   node web/test/bench.mjs --serve              serve the page for another browser, and wait
//
// Run A is the correctness run. Every map is told a fixed step and every fetch is waited for, so
// the stream is a property of the producer and not of the network, and it is diffed against the
// native serial producer given the same style, cameras, steps and tile: any difference stops the
// run. Coverage, flatness, blank frames and ring occupancy are gated on it too, none of which
// need pixels -- which is what lets this run on a runner with no GPU.
//
// Run B is the timing run, on the wall clock. Its budget numbers mean something only on a named
// machine and browser build, which is why `--budget` is asked for rather than assumed.
//
// Expects the wasm module to be built, as `browser.mjs` does:
//   cargo build -p tessella-ffi --target wasm32-unknown-unknown --release

import { execFileSync, spawnSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { gzipSync } from "node:zlib";

import { serve, firefox, root } from "./firefox.mjs";
import { evaluate, absent } from "../bench/gates.js";
import { summary } from "../bench/stats.js";
import { cameraDistance } from "../bench/trace.js";

const args = process.argv.slice(2);
const flag = (name) => args.includes(name);
const value = (name, fallback) => {
  const at = args.indexOf(name);
  return at >= 0 && at + 1 < args.length ? args[at + 1] : fallback;
};

const clock = value("--clock", "A") === "B" ? "B" : "A";
const steps = Number(value("--steps", "33"));
const runs = Number(value("--runs", clock === "A" ? "1" : "6"));
const tickMs = 16.667;

const MODULE = "target/wasm32-unknown-unknown/release/tessella_ffi.wasm";
const TILE = readFileSync(join(root, "tests/mvt-fixtures/protomaps-berlin-14-8802-5373.mvt"));

/** The tile route: one fixture at every tile, as `tools/tile-server` serves it. */
function route(path) {
  return /^\/bench-tiles\/\d+\/\d+\/\d+\.pbf$/.test(path)
    ? { type: "application/x-protobuf", body: TILE }
    : null;
}

/**
 * The module's size, three ways: as built, with the custom sections (debug info, names) dropped,
 * and gzipped after that. The middle one is what a user downloads before any compression, and a
 * release build carrying DWARF is several times it.
 */
function moduleSize(bytes) {
  const code = stripCustomSections(bytes);
  return {
    built: bytes.length,
    stripped: code.length,
    gzipped: gzipSync(code, { level: 9 }).length,
  };
}

/**
 * A wasm module without its custom sections (id 0), which nothing executes.
 *
 * Refuses a module that ends inside a section header or claims more bytes than it has, rather
 * than reading past the end -- a truncated build would otherwise spin in the length loop.
 */
function stripCustomSections(bytes) {
  const out = [bytes.subarray(0, 8)];
  let at = 8;
  while (at < bytes.length) {
    const start = at;
    const id = bytes[at++];
    let size = 0;
    let shift = 0;
    for (;;) {
      if (at >= bytes.length || shift > 28) {
        throw new Error(`the module ends inside a section header at byte ${start}`);
      }
      const byte = bytes[at++];
      size += (byte & 0x7f) * 2 ** shift;
      shift += 7;
      if (byte < 0x80) break;
    }
    at += size;
    if (at > bytes.length) {
      throw new Error(`the section at byte ${start} claims more bytes than the module has`);
    }
    if (id !== 0) {
      out.push(bytes.subarray(start, at));
    }
  }
  return Buffer.concat(out);
}

/** `wasm-opt -O3`, where it is installed. The module the page loads is whichever this answers. */
function optimize() {
  const probe = spawnSync("wasm-opt", ["--version"], { encoding: "utf8" });
  if (probe.status !== 0) {
    return {
      module: MODULE,
      note: "WASM-OPT NOT RUN: not installed; the release build is measured",
    };
  }
  const optimized = MODULE.replace(/\.wasm$/, ".opt.wasm");
  execFileSync("wasm-opt", ["-O3", join(root, MODULE), "-o", join(root, optimized)]);
  return { module: optimized, note: `wasm-opt -O3 (${probe.stdout.trim()})` };
}

/** The native serial producer over the same sweep: the oracle's values and a trace per frame. */
function native() {
  const run = spawnSync(
    "cargo",
    [
      "run",
      "-q",
      "--release",
      "-p",
      "tessella-ffi",
      "--example",
      "sweep_trace",
      "--",
      "--steps",
      String(steps),
      "--tick-ms",
      String(tickMs),
    ],
    {
      cwd: root,
      encoding: "utf8",
      env: { ...process.env, TESSELLA_WORKERS: "0" },
      maxBuffer: 1 << 28,
    },
  );
  if (run.status !== 0) {
    throw new Error(`the native trace failed:\n${run.stderr}`);
  }
  const lines = run.stdout.split("\n");
  const oracle = JSON.parse(lines.find((l) => l.startsWith("ORACLE ")).slice("ORACLE ".length));
  const traces = lines.filter((l) => l.startsWith("TRACE ")).map((l) => JSON.parse(l.slice(6)));
  return { oracle, traces };
}

/** One run of the page: its frames, its traces and its report. */
async function browse(port, module) {
  const query = new URLSearchParams({
    clock,
    steps: String(steps),
    tickMs: String(tickMs),
    module: `/${module}`,
  });
  return firefox(`http://127.0.0.1:${port}/web/bench.html?${query}`, {
    timeoutMs: 300_000,
    windowSize: "1320,1200",
    until: (out) => {
      const lines = out.split("\n");
      if (!lines.includes("DONE")) {
        return undefined;
      }
      const error = lines.find((l) => l.startsWith("ERROR "));
      const report = lines.find((l) => l.startsWith("REPORT "));
      return {
        error: error ? JSON.parse(error.slice(6)) : null,
        report: report ? JSON.parse(report.slice(7)) : null,
        frames: lines.filter((l) => l.startsWith("FRAME ")).map((l) => JSON.parse(l.slice(6))),
        traces: lines.filter((l) => l.startsWith("TRACE ")).map((l) => JSON.parse(l.slice(6))),
      };
    },
  });
}

/**
 * How far a camera's doubles may be from native's, in ulps.
 *
 * They come from the platform's libm, and wasm's and glibc's disagree in the last bits: five ulps
 * at most over the whole sweep, on this machine, in the projection's depth terms and the pixel
 * scale. Sixteen is that with headroom. A camera that is wrong rather than differently rounded is
 * wrong by far more -- a matrix built for the wrong zoom differs in its exponent.
 */
const CAMERA_ULPS = 16;

/**
 * How the browser's trace compares with native's: the first difference that fails the gate, if
 * any, and what the camera records did.
 */
function compare(browser, nativeTraces) {
  const fields = ["status", "pending", "records", "hash", "slabs"];
  const cameras = { records: 0, differing: 0, maxUlps: 0 };
  const frames = Math.max(browser.length, nativeTraces.length);
  for (let i = 0; i < frames; i++) {
    const b = browser[i];
    const n = nativeTraces[i];
    const where = `frame ${i} (${b?.phase ?? n?.phase})`;
    if (!b || !n) {
      const stopped = Math.min(browser.length, nativeTraces.length);
      return {
        difference: `${b ? "native" : "the browser"} stopped at ${stopped} frames`,
        cameras,
      };
    }
    if (b.phase !== n.phase) {
      return {
        difference: `${where}: the browser is in ${b.phase}, native in ${n.phase}`,
        cameras,
      };
    }
    for (let m = 0; m < 4; m++) {
      for (const field of fields) {
        if (b.maps[m][field] !== n.maps[m][field]) {
          const said = `${field} is ${b.maps[m][field]} in the browser and ${n.maps[m][field]} natively`;
          return { difference: `${where}, map ${m}: ${said}`, cameras };
        }
      }
      const [bc, nc] = [b.maps[m].cameras, n.maps[m].cameras];
      if (bc.length !== nc.length) {
        return {
          difference: `${where}, map ${m}: ${bc.length} cameras here, ${nc.length} natively`,
          cameras,
        };
      }
      for (let c = 0; c < bc.length; c++) {
        cameras.records++;
        if (bc[c] === nc[c]) {
          continue;
        }
        cameras.differing++;
        const { ulps, otherBytes } = cameraDistance(bc[c], nc[c]);
        cameras.maxUlps = Math.max(cameras.maxUlps, ulps);
        if (otherBytes > 0 || ulps > CAMERA_ULPS) {
          const said =
            otherBytes > 0 ? `${otherBytes} non-f64 bytes differ` : `a double is ${ulps} ulps off`;
          return { difference: `${where}, map ${m}, camera ${c}: ${said}`, cameras };
        }
      }
    }
  }
  return { difference: null, cameras };
}

/** Whether the JavaScript port of the sweep is the oracle's, value for value. */
function oracleDifference(report, oracle) {
  const views = report.scene.views;
  for (let i = 0; i < 4; i++) {
    for (const key of ["latitude", "longitude"]) {
      if (views[i][key] !== oracle.views[i][key]) {
        return `view ${i} ${key}: ${views[i][key]} here, ${oracle.views[i][key]} in sweep.rs`;
      }
    }
  }
  if (report.scene.zooms.length !== oracle.zooms.length) {
    return `${report.scene.zooms.length} zooms here, ${oracle.zooms.length} in sweep.rs`;
  }
  const at = report.scene.zooms.findIndex((z, i) => z !== oracle.zooms[i]);
  return at < 0
    ? null
    : `zoom ${at}: ${report.scene.zooms[at]} here, ${oracle.zooms[at]} in sweep.rs`;
}

function describe(report) {
  const t = report.timings;
  const line = (name, s) =>
    s ? `${name} p50 ${s.p50} p95 ${s.p95} p99 ${s.p99} max ${s.max}` : `${name} -`;
  return [
    `clock ${report.clock}: ${report.frames} sweep frames, settled in ${report.settle_frames}, ` +
      `${report.pacing} pacing, webgl2 ${report.webgl2}`,
    `  ${line("frame cpu (sum of ticks + absorb + draw) ms", t.frame_cpu_ms)}`,
    `  ${line("max tick + absorb + draw ms", t.max_tick_frame_ms)}`,
    `  ${line("ticks (sum of four) ms", t.tick_ms_sum)}`,
    `  ${line("absorb ms", t.absorb_ms)}`,
    `  ${line("draw ms (cpu, submit only)", t.draw_ms)}`,
    `  ${line("raf interval ms", t.raf_interval_ms)}`,
    `  timer: ${report.timer.resolution_ms} ms resolution, cross-origin isolated ${report.timer.isolated}`,
    `  fetch: ${report.fetch.origin_fetches} origin fetches for ${report.fetch.distinct_urls} distinct urls, ` +
      `${report.fetch.tickets} tickets, ${report.fetch.coalesced} coalesced, ${report.fetch.cache_hits} cache hits, ` +
      `sharing ratio ${report.fetch.sharing_ratio}`,
    `  memory: peak ${report.mem_pages_peak} pages (${(report.mem_pages_peak / 16).toFixed(1)} MiB)` +
      (report.growth
        ? `, grew ${report.growth.before} -> ${report.growth.after} before the sweep`
        : ""),
  ].join("\n");
}

const { module, note } = optimize();
const { server, port } = await serve({ route, isolated: true });

if (flag("--serve")) {
  console.log(`bench: http://127.0.0.1:${port}/web/bench.html?module=/${module}`);
  console.log(`bench: http://127.0.0.1:${port}/web/bench.html?clock=A&module=/${module}`);
  console.log("bench: serving until interrupted");
} else {
  let failed = false;
  try {
    const size = moduleSize(readFileSync(join(root, module)));
    console.log(
      `bench: module ${module}: ${size.built} bytes built, ${size.stripped} without custom sections, ` +
        `${size.gzipped} gzipped; ${note}`,
    );
    console.log(
      "bench: scene: four_views() centers at 640x480, the protomaps-berlin z14 fixture served at " +
        "every tile, fills only",
    );

    const nativeRuns = [];
    const browserRuns = [];
    for (let run = 0; run < runs; run++) {
      // Interleaved, so whatever drifts on the machine over the runs is charged to both.
      const n = native();
      nativeRuns.push(n);
      const b = await browse(port, module);
      if (b.error || !b.report) {
        throw new Error(`the sweep failed in the browser:\n${b.error ?? "no report"}`);
      }
      browserRuns.push(b);
      console.log(describe(b.report));

      const port_ = oracleDifference(b.report, n.oracle);
      if (port_) {
        console.error(`bench: the JavaScript sweep is not sweep.rs: ${port_}`);
        failed = true;
      }
      if (clock === "A") {
        const { difference, cameras } = compare(b.traces, n.traces);
        if (difference) {
          console.error(`bench: determinism: FAIL: ${difference}`);
          failed = true;
        } else {
          console.log(
            `bench: determinism: pass: ${b.traces.length} frames x 4 maps; every record but the camera and every ` +
              `slab byte identical to native serial; cameras within ${CAMERA_ULPS} ulps`,
          );
        }
        // The strict statement, answered as asked, whatever the gate made of it.
        console.log(
          `bench: BYTE-IDENTICAL TO NATIVE: ${cameras.differing === 0 && !difference ? "yes" : "NO"}: ` +
            `${cameras.differing} of ${cameras.records} camera records differ, by at most ${cameras.maxUlps} ulps ` +
            `in their f64 fields (libm: wasm's is not glibc's)`,
        );
      }

      for (const row of evaluate(b.report, { budget: flag("--budget") })) {
        const verdict = row.pass === null ? "reported" : row.pass ? "pass" : "FAIL";
        const gated = row.gating ? "" : " (not gating)";
        console.log(
          `bench: ${row.property}: ${verdict}${gated}: ${row.statement}: ${JSON.stringify(row.value)}`,
        );
        if (row.gating && row.pass === false) {
          failed = true;
        }
      }
    }

    // Native beside it: the same producer, serially, on the same machine, over the same frames.
    const nativeTicks = summary(
      nativeRuns.flatMap((n) =>
        n.traces
          .filter((t) => t.phase === "sweep")
          .map((t) => t.tick_ms.reduce((a, b) => a + b, 0)),
      ),
    );
    const browserTicks = summary(
      browserRuns.flatMap((b) =>
        b.frames
          .filter((f) => f.phase === "sweep")
          .map((f) => f.tick_ms.reduce((a, c) => a + c, 0)),
      ),
    );
    console.log(
      `bench: ticks (sum of four) over ${runs} run(s): browser p50 ${browserTicks.p50} p99 ${browserTicks.p99} ` +
        `max ${browserTicks.max} ms; native serial p50 ${nativeTicks.p50} p99 ${nativeTicks.p99} max ${nativeTicks.max} ms` +
        (clock === "A" ? " (clock A: fetches awaited, so these are not what a user sees)" : ""),
    );
    for (const line of absent(browserRuns[0].report)) {
      console.log(`bench: ${line}`);
    }
    if (!flag("--budget")) {
      console.log(
        `bench: FRAME BUDGET NOT GATED: reported only; gate with --clock B --budget on a named machine`,
      );
    }
    console.log(`bench: SETTLE TIME: frames only; the wall-clock quiet-tail protocol is not run`);
    if (flag("--json")) {
      const path = value("--json");
      if (!path || path.startsWith("--")) {
        throw new Error("--json needs a path to write to");
      }
      writeFileSync(path, JSON.stringify({ runs: browserRuns.map((b) => b.report) }, null, 2));
    }
  } catch (error) {
    console.error(`bench: ${error.message ?? error}`);
    failed = true;
  } finally {
    server.close();
  }
  process.exit(failed ? 1 : 0);
}
