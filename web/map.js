// Driving a tessella map from JavaScript.
//
// The producer is a wasm module with a flat C ABI: `#[no_mangle] extern "C"`, no wasm-bindgen, no
// glue module. So there is nothing to import and nothing generated -- the module is instantiated
// with no imports at all, and everything below is calls into it and reads out of `memory.buffer`.
//
// A hosted map fetches nothing itself. It writes down what it needs, this fetches it, and the
// bytes go back through the ticket. That is the only arrangement a browser can have, and it is
// also the one a caller with its own cache or its own idea of when a fetch is allowed wants.

import { Ring, Slabs } from "./ring.js";

/** What `tessella_result` calls success. */
const OK = 0;

/** Where a hosted map is told to look, and what to call the answer. */
const DEFAULT_RING_BYTES = 1 << 22;

/**
 * The largest body this consumer will hand over, and the largest style it will take.
 *
 * Not a guess at what is reasonable but a bound on what is *safe*: the scratch block below the
 * module's heap is a fixed reservation, and a body larger than it would be written straight over
 * the producer's heap. Every byte here came off a network from an origin that is not a trusted
 * party, so the size is checked rather than hoped for -- `tessella-storage` caps a resource at ten
 * mebibytes for the same reason, and this is the reservation that cap fits inside.
 */
const MAX_BODY_BYTES = 16 << 20;
const MAX_STYLE_BYTES = (1 << 20) - 4096;

/**
 * A live map.
 *
 * Not a renderer. This gets records out of the producer and leaves drawing to whatever wants
 * them, because the two questions are separable and the memory-view story is the one worth
 * proving first.
 */
export class TessellaMap {
  /**
   * @param {WebAssembly.Instance} instance
   * @param {number} handle
   */
  constructor(instance, handle) {
    this.wasm = instance.exports;
    this.handle = handle;
    this.memory = this.wasm.memory;
    this.encoder = new TextEncoder();
    this.decoder = new TextDecoder();

    const regions = this.#regions();
    this.ring = new Ring(this.memory, regions.ring, regions.ringLen);
    this.slabs = new Slabs(this.memory, regions.slabs, regions.slabsLen);
  }

  /**
   * Instantiates the module and creates a hosted map.
   *
   * @param {BufferSource} moduleBytes  the compiled `.wasm`
   * @param {string} style              the style document, as JSON
   * @param {{latitude?: number, longitude?: number, zoom?: number,
   *          width?: number, height?: number, ringBytes?: number}} [options]
   */
  static async create(moduleBytes, style, options = {}) {
    // No imports. A producer that needed any would need bindings to generate them, which is what
    // keeping wasm-bindgen out of it buys.
    const { instance } = await WebAssembly.instantiate(moduleBytes, {});
    const wasm = instance.exports;

    const {
      latitude = 0,
      longitude = 0,
      zoom = 0,
      width = 1024,
      height = 768,
      ringBytes = DEFAULT_RING_BYTES,
    } = options;

    // The producer has no allocator export, so the config and the style go somewhere this side
    // owns -- which on wasm means somewhere inside the module's own memory. `__heap_base` is
    // where its heap starts and nothing below it is in use, so the page before it is scratch that
    // the producer will never allocate over.
    const scratch = scratchAt(wasm);
    const styleBytes = new TextEncoder().encode(style);
    if (styleBytes.length > MAX_STYLE_BYTES) {
      throw new Error(`the style is ${styleBytes.length} bytes; the most this can pass is ${MAX_STYLE_BYTES}`);
    }
    new Uint8Array(wasm.memory.buffer, scratch.style, styleBytes.length).set(styleBytes);

    const config = new DataView(wasm.memory.buffer, scratch.config, 40);
    config.setUint32(0, scratch.style, true); // style_json
    config.setUint32(4, styleBytes.length, true); // style_json_len
    config.setUint32(8, width, true);
    config.setUint32(12, height, true);
    config.setUint32(16, ringBytes, true); // ring_capacity
    config.setUint32(20, 0, true); // slab_capacity: the default

    const out = scratch.out;
    const status = wasm.tessella_create_hosted(scratch.config, latitude, longitude, zoom, out);
    if (status !== OK) {
      throw new Error(`tessella_create_hosted answered ${status}`);
    }
    const handle = new DataView(wasm.memory.buffer).getUint32(out, true);
    if (handle === 0) {
      throw new Error("tessella_create_hosted answered OK without a handle");
    }
    return new TessellaMap(instance, handle);
  }

  /** Where the ring and the slab region are, as byte offsets into linear memory. */
  #regions() {
    const at = scratchAt(this.wasm).regions;
    if (this.wasm.tessella_regions(this.handle, at) !== OK) {
      throw new Error("tessella_regions refused");
    }
    const view = new DataView(this.memory.buffer);
    return {
      ring: view.getUint32(at, true),
      ringLen: view.getUint32(at + 4, true),
      slabs: view.getUint32(at + 8, true),
      slabsLen: view.getUint32(at + 12, true),
    };
  }

  /** Moves the camera. */
  setCamera(latitude, longitude, zoom, bearing = 0, pitch = 0) {
    return this.wasm.tessella_set_camera(this.handle, latitude, longitude, zoom, bearing, pitch);
  }

  /** How much work is still in flight. Zero means nothing further is coming without a tick. */
  get pending() {
    return this.wasm.tessella_pending(this.handle);
  }

  /**
   * One frame: tick, hand the fetches out, take whatever records came back.
   *
   * `fetch` is whatever the caller wants -- the browser's, a cache, or a table in a test. It is
   * given a URL and answers `{status, body}`, or throws to say the fetch did not happen at all,
   * which is a different thing from an origin saying no.
   *
   * @param {(url: string) => Promise<{status: number, body: Uint8Array}>} fetchOne
   */
  async tick(fetchOne) {
    const status = this.wasm.tessella_tick(this.handle);
    if (status !== OK) {
      throw new Error(`tessella_tick answered ${status}`);
    }
    await this.#serve(fetchOne);
    return this.ring.drain();
  }

  /** Answers everything the map asked for on this tick. */
  async #serve(fetchOne) {
    const scratch = scratchAt(this.wasm);
    const asked = [];
    for (;;) {
      if (
        this.wasm.tessella_take_request(
          this.handle,
          scratch.ticket,
          scratch.url,
          scratch.urlLen,
        ) !== OK
      ) {
        throw new Error("tessella_take_request refused");
      }
      const view = new DataView(this.memory.buffer);
      // Ticket zero is what a map with nothing to fetch says. Never a real ticket, so there is no
      // other value it could be confused with.
      const ticket = view.getBigUint64(scratch.ticket, true);
      if (ticket === 0n) {
        break;
      }
      const at = view.getUint32(scratch.url, true);
      const len = view.getUint32(scratch.urlLen, true);
      asked.push([ticket, this.decoder.decode(new Uint8Array(this.memory.buffer, at, len))]);
    }

    // Fetched together. They do not depend on each other, and a browser will happily have six of
    // them in the air, which is most of what makes a cold start quick.
    const answers = await Promise.all(
      asked.map(async ([ticket, url]) => {
        try {
          return [ticket, await fetchOne(url)];
        } catch {
          return [ticket, null];
        }
      }),
    );

    // Handed over one at a time, *after* the fetches. There is one scratch buffer, so two
    // concurrent answers writing into it would each hand the producer the other's bytes -- which
    // with one tile in flight looks like it works and with two is a tile drawn from a manifest.
    for (const [ticket, answer] of answers) {
      if (answer === null) {
        this.wasm.tessella_fail_request(this.handle, ticket);
        continue;
      }
      const body = answer.body ?? new Uint8Array(0);
      // Refused rather than truncated. A body this side cannot hold is a fetch that did not
      // happen as far as the map is concerned, and truncating one would hand the producer a tile
      // that decodes to something the origin never sent.
      if (body.length > MAX_BODY_BYTES) {
        this.wasm.tessella_fail_request(this.handle, ticket);
        continue;
      }
      const into = scratchAt(this.wasm).body;
      new Uint8Array(this.memory.buffer, into, body.length).set(body);
      this.wasm.tessella_answer(this.handle, ticket, answer.status, into, body.length);
    }
  }

  /** Releases the map. */
  destroy() {
    this.wasm.tessella_destroy(this.handle);
    this.handle = 0;
  }
}

/**
 * Scratch inside the module's memory, below where its allocator will ever reach.
 *
 * The producer exports no allocator, deliberately: §19.2 keeps the surface to `extern "C"` and an
 * allocator export is a second ABI to keep honest. `__heap_base` is where the module's heap
 * starts, so everything below it belongs to the linker's static data and nothing above this
 * consumer's own reservation will be handed out. Carving a fixed block under it is the smallest
 * arrangement that needs nothing from the producer.
 *
 * Sized for one style document and one fetched body at a time, which is what this consumer holds.
 */
function scratchAt(wasm) {
  const base = wasm.__heap_base.valueOf();
  // Grown up front rather than on demand: growing detaches every view over `memory.buffer`, and
  // doing it in the middle of writing one is the bug that arrangement invites.
  const need = base + SCRATCH.total;
  const have = wasm.memory.buffer.byteLength;
  if (have < need) {
    wasm.memory.grow(Math.ceil((need - have) / 65536));
  }
  return {
    config: base + SCRATCH.config,
    out: base + SCRATCH.out,
    regions: base + SCRATCH.regions,
    ticket: base + SCRATCH.ticket,
    url: base + SCRATCH.url,
    urlLen: base + SCRATCH.urlLen,
    style: base + SCRATCH.style,
    body: base + SCRATCH.body,
  };
}

/** Where each scratch field sits, relative to `__heap_base`. */
const SCRATCH = Object.freeze({
  config: 0,
  out: 64,
  regions: 128,
  ticket: 192,
  url: 256,
  urlLen: 320,
  style: 4096,
  body: 1 << 20,
  total: (1 << 20) + (16 << 20),
});
