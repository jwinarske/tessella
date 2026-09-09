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

import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { createReadStream, statSync } from "node:fs";
import { mkdtemp, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, normalize } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../..", import.meta.url));

/** The last of a browser's output, which is where its complaint is. */
function tail(text) {
  return text.split("\n").slice(-40).join("\n");
}

/** Enough of a content type for the three things this serves. */
const TYPES = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".wasm": "application/wasm",
  ".mvt": "application/vnd.mapbox-vector-tile",
};

/** Serves the repository, read-only, on a port the caller is told. */
function serve() {
  const server = createServer((request, response) => {
    // Resolved against the root and checked, because a test server that can be walked out of is
    // still a server that can be walked out of.
    const asked = normalize(decodeURIComponent(new URL(request.url, "http://x").pathname));
    const path = join(root, asked);
    if (!path.startsWith(root)) {
      response.writeHead(403).end();
      return;
    }
    let size;
    try {
      size = statSync(path).size;
    } catch {
      response.writeHead(404).end();
      return;
    }
    const dot = path.lastIndexOf(".");
    response.writeHead(200, {
      "content-type": TYPES[path.slice(dot)] ?? "application/octet-stream",
      "content-length": size,
    });
    createReadStream(path).pipe(response);
  });
  return new Promise((resolve) => {
    server.listen(0, "127.0.0.1", () => resolve({ server, port: server.address().port }));
  });
}

/** Runs the page and returns whatever it dumped. */
async function run(port) {
  const profile = await mkdtemp(join(tmpdir(), "tessella-ff-"));
  // `dump` is off by default, and it is the channel the page reports through.
  // `dump` is the channel the page reports through, and the slow-script watchdog would otherwise
  // stop a module that awaits four hundred ticks before it finished the first one.
  await writeFile(
    join(profile, "user.js"),
    [
      'user_pref("browser.dom.window.dump.enabled", true);',
      'user_pref("dom.max_script_run_time", 0);',
      // A runner with no GPU still has Mesa's software rasteriser; these are what let Firefox
      // reach it rather than refusing WebGL outright.
      'user_pref("webgl.force-enabled", true);',
      'user_pref("gfx.webrender.software", true);',
      "",
    ].join("\n"),
  );
  try {
    const firefox = spawn(
      process.env.FIREFOX ?? "firefox",
      [
        "--profile", profile,
        "--no-remote",
        "--headless",
        "--window-size=512,512",
        `http://127.0.0.1:${port}/web/test/browser.html`,
      ],
      { env: { ...process.env, MOZ_HEADLESS: "1", LIBGL_ALWAYS_SOFTWARE: "1" } },
    );

    // Killed on the answer rather than waited on. Without `--screenshot` a browser has no reason
    // to exit, and with it the page is torn down at load -- which is before an async module that
    // drives four hundred ticks has finished its first one.
    let out = "";
    const done = new Promise((resolve, reject) => {
      const bell = setTimeout(() => {
        firefox.kill("SIGKILL");
        reject(new Error(`the browser did not report within two minutes:\n${tail(out)}`));
      }, 120_000);
      const watch = (chunk) => {
        out += chunk;
        const line = out.split("\n").find((l) => l.startsWith("RESULT "));
        if (line) {
          clearTimeout(bell);
          firefox.kill("SIGKILL");
          resolve(JSON.parse(line.slice("RESULT ".length)));
        }
      };
      firefox.stdout.on("data", watch);
      firefox.stderr.on("data", watch);
      firefox.on("error", reject);
      firefox.on("exit", () => {
        clearTimeout(bell);
        const line = out.split("\n").find((l) => l.startsWith("RESULT "));
        if (line) {
          resolve(JSON.parse(line.slice("RESULT ".length)));
        } else {
          reject(new Error(`the page reported nothing:\n${tail(out)}`));
        }
      });
    });
    return await done;
  } finally {
    await rm(profile, { recursive: true, force: true });
  }
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
  // The pixels. This is the only thing in the whole suite that checks the renderer draws rather
  // than that the records arrive, so it is worth being specific: a frame of pure background is
  // what a renderer that binds nothing produces, and a frame of pure water is what one that
  // ignores the holes produces. Both have to be wrong.
  if (!result.painted) {
    console.error("browser: no webgl2, so the drawing is not being checked here");
    process.exit(1);
  }
  const { drawn, water, ground } = result.painted;
  if (drawn < 1) {
    console.error(`browser: the renderer drew ${drawn} drawables`);
    process.exit(1);
  }
  const total = water + ground;
  if (water < total / 10 || ground < total / 10) {
    console.error(
      `browser: the frame is ${water} water and ${ground} background pixels; one of them is missing`,
    );
    process.exit(1);
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
      `${result.painted.drawn} drawables, ${result.painted.water} water px, ` +
      `${result.painted.ground} background px`,
  );
} finally {
  server.close();
}
