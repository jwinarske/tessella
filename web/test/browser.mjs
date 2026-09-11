// Runs the consumer in a real browser and reads back what it saw.
//
// Node proves the memory view; this proves the half Node cannot. A browser instantiates the
// module from a network response, answers the map's requests with its own `fetch`, and manages
// the `WebAssembly.Memory` itself -- and every one of those is a place the arrangement could be
// wrong in a way that never shows under Node.
//
// Headless Firefox, because it is what this machine has and because `dump()` gives a plain text
// channel out of the page. A screenshot would mean reading numbers out of an image.
//
//   node web/test/browser.mjs

import { serve, firefox } from "./firefox.mjs";

/** Runs the page and returns what it reported. */
function run(port) {
  return firefox(`http://127.0.0.1:${port}/web/test/browser.html`, {
    until: (out) => {
      const line = out.split("\n").find((l) => l.startsWith("RESULT "));
      return line ? JSON.parse(line.slice("RESULT ".length)) : undefined;
    },
  });
}

const { server, port } = await serve();
try {
  const result = await run(port);
  if (!result.ok) {
    console.error(`the consumer failed in the browser:\n${result.error}`);
    process.exit(1);
  }
  // Asserted rather than printed. A harness that only reported would pass on a map that drew
  // nothing, which is the failure it exists to catch.
  const wanted = { geometry: 1, vertices: 1, indexBytes: 1, asked: 1 };
  for (const [name, least] of Object.entries(wanted)) {
    if (!(result[name] >= least)) {
      console.error(`browser: ${name} was ${result[name]}, wanted at least ${least}`);
      process.exit(1);
    }
  }
  // The pixels, where there is a GL to draw them with.
  //
  // There is not, on a GitHub runner: it has Firefox but no GPU, and installing Mesa and naming
  // llvmpipe did not get a WebGL2 context out of it either -- the browser there is snap-confined
  // and does not see the host's drivers. So this checks the drawing wherever a GL exists and says
  // so when one does not, rather than failing a lane over the runner's graphics stack or, worse,
  // passing quietly and implying the renderer was checked.
  if (result.painted) {
    const { drawn, water, ground } = result.painted;
    if (drawn < 1) {
      console.error(`browser: the renderer drew ${drawn} drawables`);
      process.exit(1);
    }
    // Two failures have to be impossible: a frame of pure background is what a renderer that
    // binds nothing produces, and a frame of pure water is what one that ignores the holes
    // produces -- which is the bug the MVT v1 repair fixed, seen from the other end.
    const total = water + ground;
    if (water < total / 10 || ground < total / 10) {
      console.error(
        `browser: the frame is ${water} water and ${ground} background pixels; one is missing`,
      );
      process.exit(1);
    }
  }

  // The cache, which only a browser can exercise: OPFS is not in Node, and the fallback there is
  // a Map, which proves nothing about the file system.
  if (result.cache) {
    if (!result.cache.opfs) {
      console.error("browser: no OPFS, so the cache is not being checked here");
    } else if (!result.cache.hit) {
      console.error("browser: a second read of the same url went to the origin again");
      process.exit(1);
    } else if (!result.cache.sameBytes) {
      console.error("browser: the cached body differed from the fetched one");
      process.exit(1);
    }
  }

  // A settled map has nothing outstanding. Worth its own check because the failure it catches --
  // reading a status where a count was meant -- reports a plausible number rather than an error.
  if (result.pending !== 0) {
    console.error(`browser: the map still reports ${result.pending} outstanding after settling`);
    process.exit(1);
  }
  console.log(
    `browser: ${result.records} records, ${result.geometry} geometry, ${result.vertices} vertices, ` +
      `${result.indexBytes} index bytes, ${result.asked} fetches, ` +
      (result.painted
        ? `${result.painted.drawn} drawables, ${result.painted.water} water px, ` +
          `${result.painted.ground} background px`
        : "PIXELS NOT CHECKED: no webgl2 here") +
      (result.cache?.opfs ? `, opfs ${result.cache.hit ? "hit" : "MISS"}` : ", no opfs"),
  );
} finally {
  server.close();
}
