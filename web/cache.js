// Not fetching the same bytes twice.
//
// # Where this lives, and why it is not in the producer
//
// §19.2 puts the web's cache "at the same boundary `CachingFileSource` already uses". That
// boundary moved. On this target the producer holds no `FileSource` at all -- `TileSource` is
// generic over a transport, the browser's is the host-driven one, and the fetching is done out
// here. So the boundary between "want these bytes" and "go and get them" *is* `fetchOne`, and
// that is what this wraps.
//
// It could not be in the producer in any case: OPFS is asynchronous and reaching it needs
// bindings, which §19.2 rules out for the same reason it rules out `wasm-bindgen` generally.
//
// # Freshness, which the producer cannot see
//
// `tessella_answer` carries a status and bytes. It does not carry headers, so the producer has no
// `Cache-Control` to reason about and a cache built inside it would have to invent an expiry.
// Out here the real `Response` is in hand, so the origin's own freshness is what decides -- which
// makes this the *better* place for it rather than merely the possible one.

/// Hex of the SHA-256 of a key, which is what a file in a flat directory can be called.
async function digest(text) {
  const bytes = new TextEncoder().encode(text);
  const hash = await crypto.subtle.digest("SHA-256", bytes);
  return [...new Uint8Array(hash)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

/// One cached response: what the origin said, and when it stops being true.
///
/// Framed rather than stored raw. An OPFS write is not atomic, so a tab closed mid-write leaves a
/// short file; a length in the header is what turns that into a miss instead of a truncated tile.
const HEADER = 16;

function frame(status, expiresAt, body) {
  const out = new Uint8Array(HEADER + body.length);
  const view = new DataView(out.buffer);
  view.setUint16(0, status, true);
  view.setUint32(4, body.length, true);
  view.setFloat64(8, expiresAt, true);
  out.set(body, HEADER);
  return out;
}

function unframe(bytes) {
  if (bytes.length < HEADER) {
    return null;
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const status = view.getUint16(0, true);
  const length = view.getUint32(4, true);
  const expiresAt = view.getFloat64(8, true);
  // A file shorter than it claims is a write that did not finish.
  if (bytes.length !== HEADER + length) {
    return null;
  }
  return { status, expiresAt, body: bytes.subarray(HEADER) };
}

/// A store in the Origin Private File System, which survives a reload.
export class OpfsStore {
  /** @param {FileSystemDirectoryHandle} directory */
  constructor(directory) {
    this.directory = directory;
  }

  /**
   * Opens (or creates) a directory to cache in.
   *
   * Answers `null` where there is no OPFS -- a browser without it, or Node -- so a caller can
   * fall back rather than branch on feature detection itself.
   */
  static async open(name = "tessella-cache") {
    if (!globalThis.navigator?.storage?.getDirectory || !globalThis.crypto?.subtle) {
      return null;
    }
    try {
      const root = await navigator.storage.getDirectory();
      return new OpfsStore(await root.getDirectoryHandle(name, { create: true }));
    } catch {
      return null;
    }
  }

  async get(key) {
    try {
      const handle = await this.directory.getFileHandle(await digest(key));
      const file = await handle.getFile();
      return new Uint8Array(await file.arrayBuffer());
    } catch {
      // A miss and an unreadable entry are the same thing to a caller: go and fetch it.
      return null;
    }
  }

  async put(key, bytes) {
    try {
      const handle = await this.directory.getFileHandle(await digest(key), { create: true });
      const writable = await handle.createWritable();
      await writable.write(bytes);
      await writable.close();
    } catch {
      // A full or unavailable disk is not a reason to fail a fetch that already succeeded.
    }
  }
}

/// A store that forgets when the page does. The fallback, and what a test uses.
export class MemoryStore {
  constructor() {
    this.entries = new Map();
  }

  async get(key) {
    return this.entries.get(key) ?? null;
  }

  async put(key, bytes) {
    this.entries.set(key, bytes);
  }
}

/**
 * When the origin said this stops being fresh, as milliseconds since the epoch.
 *
 * `Cache-Control: max-age` first and `Expires` second, which is the order mbgl reads them in and
 * the order the HTTP spec gives. `no-store` means never keep it; nothing at all means the same,
 * because guessing a lifetime for a resource that did not state one is how a map ends up drawing
 * last week's tiles.
 */
export function freshUntil(response, now) {
  const control = response.headers?.get?.("cache-control") ?? "";
  if (/(^|,)\s*no-store/i.test(control)) {
    return 0;
  }
  const maxAge = /(^|,)\s*max-age\s*=\s*(\d+)/i.exec(control);
  if (maxAge) {
    return now + Number(maxAge[2]) * 1000;
  }
  const expires = response.headers?.get?.("expires");
  if (expires) {
    const at = Date.parse(expires);
    return Number.isNaN(at) ? 0 : at;
  }
  return 0;
}

/**
 * Wraps a fetch so a resource the store holds and still trusts is not fetched again.
 *
 * @param {(url: string) => Promise<{status: number, body: Uint8Array, headers?: Headers}>} fetchOne
 * @param {{get: (k: string) => Promise<Uint8Array|null>, put: (k: string, v: Uint8Array) => Promise<void>}} store
 * @param {() => number} [clock] milliseconds since the epoch; injectable so expiry is testable
 */
export function caching(fetchOne, store, clock = Date.now) {
  return async (url) => {
    const held = unframe((await store.get(url)) ?? new Uint8Array(0));
    if (held && held.expiresAt > clock()) {
      return { status: held.status, body: held.body, cached: true };
    }

    const answer = await fetchOne(url);
    const expiresAt = freshUntil(answer, clock());
    // Stored only if the origin said it may be. A response with no freshness is served and
    // forgotten, which costs a fetch next time and never serves something stale.
    if (expiresAt > clock()) {
      await store.put(url, frame(answer.status, expiresAt, answer.body ?? new Uint8Array(0)));
    }
    return { ...answer, cached: false };
  };
}
