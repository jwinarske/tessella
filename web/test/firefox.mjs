// What the browser harnesses share: a server for the repository, and a headless Firefox pointed at
// a page on it, read through `dump()`.
//
// Headless Firefox, because it is what this machine has and because `dump()` gives a plain text
// channel out of the page. A screenshot would mean reading numbers out of an image.

import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { createReadStream, statSync } from "node:fs";
import { mkdtemp, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, normalize } from "node:path";
import { fileURLToPath } from "node:url";

export const root = fileURLToPath(new URL("../..", import.meta.url));

/** The last of a browser's output, which is where its complaint is. */
export function tail(text) {
  return text.split("\n").slice(-40).join("\n");
}

/** Enough of a content type for the things this serves. */
const TYPES = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".json": "application/json",
  ".wasm": "application/wasm",
  ".mvt": "application/vnd.mapbox-vector-tile",
};

/**
 * Serves the repository, read-only, on a port the caller is told.
 *
 * `route` is asked first, with the path, and answers `{type, body}` for a path it serves or null
 * to leave it to the file system.
 *
 * `isolated` sends the two headers that make a page cross-origin isolated. A timing page wants
 * that: without it every browser coarsens `performance.now()` -- Firefox to a whole millisecond --
 * and a frame's parts are fractions of one.
 *
 * @param {{route?: (path: string) => ({type: string, body: Uint8Array} | null),
 *          isolated?: boolean}} [options]
 */
export function serve({ route = () => null, isolated = false } = {}) {
  const isolation = isolated
    ? {
        "cross-origin-opener-policy": "same-origin",
        "cross-origin-embedder-policy": "require-corp",
      }
    : {};
  const server = createServer((request, response) => {
    // Resolved against the root and checked, because a test server that can be walked out of is
    // still a server that can be walked out of.
    const asked = normalize(decodeURIComponent(new URL(request.url, "http://x").pathname));
    // Freshness, because the cache stores only what an origin said it may. A server that says
    // nothing is a server whose responses are served once and forgotten, which is correct and
    // would make a cache check vacuous.
    const fresh = "max-age=60";
    const routed = route(asked);
    if (routed) {
      response.writeHead(200, {
        "content-type": routed.type,
        "content-length": routed.body.length,
        "cache-control": fresh,
        ...isolation,
      });
      response.end(routed.body);
      return;
    }
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
      "cache-control": fresh,
      ...isolation,
    });
    createReadStream(path).pipe(response);
  });
  return new Promise((resolve) => {
    server.listen(0, "127.0.0.1", () => resolve({ server, port: server.address().port }));
  });
}

/**
 * Runs a page in headless Firefox until `until` answers something other than undefined.
 *
 * `until` is given every whole line the browser has printed so far, each time more arrives, and its
 * answer is what this resolves with. The browser is killed on the answer rather than waited on:
 * without `--screenshot` a browser has no reason to exit, and with it the page is torn down at
 * load -- which is before an async module that drives four hundred ticks has finished its first.
 *
 * @template T
 * @param {string} url
 * @param {{until: (out: string) => T | undefined, timeoutMs?: number, windowSize?: string}} options
 * @returns {Promise<T>}
 */
export async function firefox(url, { until, timeoutMs = 120_000, windowSize = "512,512" }) {
  const profile = await mkdtemp(join(tmpdir(), "tessella-ff-"));
  await writeFile(
    join(profile, "user.js"),
    [
      // `dump` is the channel the page reports through, and the slow-script watchdog would
      // otherwise stop a module that awaits four hundred ticks before it finished the first one.
      'user_pref("browser.dom.window.dump.enabled", true);',
      'user_pref("dom.max_script_run_time", 0);',
      // A runner with no GPU still has Mesa's software rasteriser; these are what let Firefox
      // reach it rather than refusing WebGL outright.
      'user_pref("webgl.force-enabled", true);',
      'user_pref("gfx.webrender.software", true);',
      // A software rasteriser is a "major performance caveat", and refusing the context over
      // that is exactly what a headless runner does not want.
      'user_pref("webgl.disable-fail-if-major-performance-caveat", true);',
      'user_pref("webgl.enable-surface-texture", false);',
      'user_pref("gfx.webrender.all", true);',
      "",
    ].join("\n"),
  );
  try {
    const browser = spawn(
      process.env.FIREFOX ?? "firefox",
      ["--profile", profile, "--no-remote", "--headless", `--window-size=${windowSize}`, url],
      {
        env: {
          ...process.env,
          MOZ_HEADLESS: "1",
          // Mesa's software path, named rather than hoped for: a runner has no GPU, and
          // llvmpipe is what it has instead.
          LIBGL_ALWAYS_SOFTWARE: "1",
          GALLIUM_DRIVER: "llvmpipe",
        },
      },
    );

    let out = "";
    return await new Promise((resolve, reject) => {
      const bell = setTimeout(() => {
        browser.kill("SIGKILL");
        reject(new Error(`the browser did not report within ${timeoutMs / 1000} s:\n${tail(out)}`));
      }, timeoutMs);
      const check = () => {
        // Whole lines only: a chunk can end halfway through one, and half a JSON line is not
        // an answer.
        const answer = until(out.slice(0, out.lastIndexOf("\n") + 1));
        if (answer !== undefined) {
          clearTimeout(bell);
          browser.kill("SIGKILL");
          resolve(answer);
          return true;
        }
        return false;
      };
      const watch = (chunk) => {
        out += chunk;
        check();
      };
      browser.stdout.on("data", watch);
      browser.stderr.on("data", watch);
      browser.on("error", reject);
      browser.on("exit", () => {
        clearTimeout(bell);
        if (!check()) {
          reject(new Error(`the page reported nothing:\n${tail(out)}`));
        }
      });
    });
  } finally {
    await rm(profile, { recursive: true, force: true });
  }
}
