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

/**
 * `TESSELLA_OFF_THE_MAP`, which the screen conversions answer for a point that is not on the map.
 *
 * Not a failure: a pitched camera's upper screen is sky and a globe does not fill its viewport.
 * Reported rather than clamped, because the answer available in both cases is worse than none --
 * mbgl's own above-the-horizon answer is the near point, a coordinate the pixel is not over.
 */
const OFF_THE_MAP = 14;
/**
 * `TESSELLA_TOO_SMALL`, which a query answers when its buffer will not hold the whole document.
 *
 * Not a truncation: half a JSON document is a syntax error rather than a smaller answer, so nothing
 * is written and the length is reported instead.
 */
const TOO_SMALL = 19;

/**
 * `sizeof(tessella_config)` on wasm32: eight fields, four bytes each.
 *
 * A pointer and a `size_t` are both 32 bits here, and the two `uint32_t`s need no padding beside
 * them, so the struct is flat. Stated once rather than spelled into the two views that write it.
 */
const CONFIG_BYTES = 32;

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
   *          width?: number, height?: number, ringBytes?: number, slabBytes?: number}} [options]
   */
  static async create(moduleBytes, style, options = {}) {
    // No imports. A producer that needed any would need bindings to generate them, which is what
    // keeping wasm-bindgen out of it buys.
    const { instance } = await WebAssembly.instantiate(moduleBytes, {});
    return TessellaMap.hosted(instance, style, options);
  }

  /**
   * Creates a hosted map in an instance that already exists.
   *
   * Several maps can live in one instance: each has its own ring, slab region and fetch table,
   * and they share the instance's memory, its pool and its scratch block. The scratch block is
   * why every call below is synchronous from write to return -- a second map writing into it
   * between the two would hand the first one the other's bytes.
   *
   * @param {WebAssembly.Instance} instance
   * @param {string} style
   * @param {Parameters<typeof TessellaMap.create>[2]} [options]
   */
  static hosted(instance, style, options = {}) {
    const wasm = instance.exports;

    const {
      latitude = 0,
      longitude = 0,
      zoom = 0,
      width = 1024,
      height = 768,
      ringBytes = DEFAULT_RING_BYTES,
      slabBytes = 0,
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

    // Zeroed first, then every field written. The struct has grown twice -- `slab_capacity`, then
    // `cache_path` and its length -- and a consumer that writes only the fields it knows about
    // leaves the rest reading whatever is in the scratch. Today that is zero: nothing else writes
    // those bytes and a fresh instance starts zeroed, which is a property of the instance rather
    // than of this code. The scratch block is shared by every map in an instance (see `hosted`),
    // so the next thing to write near it decides whether a stale tail becomes a `cache_path` the
    // producer refuses -- which is what the test that pokes this slot before creating is for.
    //
    // Eight fields of four bytes on wasm32, where a pointer and a `size_t` are both 32 bits.
    new Uint8Array(wasm.memory.buffer, scratch.config, CONFIG_BYTES).fill(0);
    const config = new DataView(wasm.memory.buffer, scratch.config, CONFIG_BYTES);
    config.setUint32(0, scratch.style, true); // style_json
    config.setUint32(4, styleBytes.length, true); // style_json_len
    config.setUint32(8, width, true);
    config.setUint32(12, height, true);
    config.setUint32(16, ringBytes, true); // ring_capacity
    config.setUint32(20, slabBytes, true); // slab_capacity: zero is the producer's default
    // cache_path at 24 and its length at 28 stay zero: there is no filesystem here to keep a store
    // in, and a non-null path is answered with TESSELLA_NO_CACHE rather than ignored.

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

  /**
   * Which coordinate a screen pixel is over, or `null` when it is over nothing.
   *
   * Viewport pixels, `y` down from the top left, which is where a pointer event arrives in. `null`
   * is `TESSELLA_OFF_THE_MAP`: a pitched camera's upper screen is sky and a globe does not fill its
   * viewport, and both are conditions of the pixel rather than failures -- a wheel handler that
   * threw on them would break at high pitch.
   *
   * This is what a wheel-zoom about the cursor is built from: the coordinate under the pointer,
   * held fixed while the zoom changes.
   */
  screenToGeo(x, y) {
    const at = scratchAt(this.wasm).geo;
    const status = this.wasm.tessella_screen_to_geo(this.handle, x, y, at, at + 8);
    if (status === OFF_THE_MAP) {
      return null;
    }
    if (status !== OK) {
      throw new Error(`tessella_screen_to_geo answered ${status}`);
    }
    const view = new DataView(this.memory.buffer);
    return { latitude: view.getFloat64(at, true), longitude: view.getFloat64(at + 8, true) };
  }

  /**
   * Which features are drawn under a screen rectangle, topmost first.
   *
   * `x0, y0` and `x1, y1` are opposite corners in the same viewport pixels every other call here
   * takes -- y down from the top edge, which is where a `PointerEvent` already measures it. Equal
   * corners are a tap, which is the ordinary case: `queryRenderedFeatures(event.offsetX,
   * event.offsetY)`.
   *
   * `layers`, when given, keeps only those layer ids. That is how a host says which of the things
   * under the finger it is willing to act on, rather than filtering the answer afterwards.
   *
   * Returns the `features` array of the GeoJSON `FeatureCollection` the producer writes: each entry
   * has its `properties`, and its `layer`, `source`, `sourceLayer` and `geometryType` beside them,
   * where MapLibre's own query puts them. `geometry` is always `null` -- a query keeps the identity
   * and the properties, and a host that wants geometry has it already, keyed by the `id` here.
   *
   * Empty for a tap on nothing, which is not an error, and empty for a point that is not on the map
   * at all -- a pitched camera's sky, or a globe's surround.
   *
   * Only what is on screen now: a feature in a tile that has not arrived is not returned, and a
   * label a collision suppressed is not returned either.
   */
  queryRenderedFeatures(x0, y0, x1 = x0, y1 = y0, layers = []) {
    const scratch = scratchAt(this.wasm);
    // The layer names, written into the staging block: the pointer array first, then the length
    // array, then the bytes. Laid out in that order so each array is contiguous, which is what the
    // ABI reads.
    const encoder = new TextEncoder();
    const encoded = layers.map((name) => encoder.encode(name));
    const pointerBytes = encoded.length * 4;
    const lengthBytes = encoded.length * 4;
    const textAt = scratch.queryArgs + pointerBytes + lengthBytes;
    const textBytes = encoded.reduce((total, bytes) => total + bytes.length, 0);
    if (pointerBytes + lengthBytes + textBytes > 1 << 16) {
      throw new Error(`${layers.length} layer names do not fit the query's staging block`);
    }
    {
      const view = new DataView(this.memory.buffer);
      let at = textAt;
      encoded.forEach((bytes, index) => {
        view.setUint32(scratch.queryArgs + index * 4, at, true);
        view.setUint32(scratch.queryArgs + pointerBytes + index * 4, bytes.length, true);
        new Uint8Array(this.memory.buffer, at, bytes.length).set(bytes);
        at += bytes.length;
      });
    }
    const ids = encoded.length === 0 ? 0 : scratch.queryArgs;
    const lens = encoded.length === 0 ? 0 : scratch.queryArgs + pointerBytes;

    const room = SCRATCH.total - SCRATCH.query;
    const status = this.wasm.tessella_query_rendered_features(
      this.handle,
      x0,
      y0,
      x1,
      y1,
      ids,
      lens,
      encoded.length,
      scratch.query,
      room,
      scratch.queryLen,
    );
    if (status === OFF_THE_MAP) {
      return [];
    }
    if (status === TOO_SMALL) {
      // The scratch block is fixed, so this is a real limit rather than a retry: the answer is
      // bigger than the megabyte reserved for it. Said plainly, with the size, because the fix is a
      // smaller rectangle or fewer layers and nothing the caller can do to the buffer.
      const needed = new DataView(this.memory.buffer).getUint32(scratch.queryLen, true);
      throw new Error(
        `a query answered ${needed} bytes and the scratch block holds ${room}; ask about a smaller rectangle`,
      );
    }
    if (status !== OK) {
      throw new Error(`tessella_query_rendered_features answered ${status}`);
    }
    const length = new DataView(this.memory.buffer).getUint32(scratch.queryLen, true);
    if (length === 0) {
      return [];
    }
    const bytes = new Uint8Array(this.memory.buffer, scratch.query, length);
    // Decoded from a copy, because `TextDecoder` over a view of the wasm memory is fine today and
    // the view is invalidated by any growth -- and `JSON.parse` is the only thing between here and
    // returning, which cannot grow it. The slice keeps that from being something to remember.
    return JSON.parse(new TextDecoder().decode(bytes.slice())).features;
  }

  /**
   * Where a coordinate lands on the screen, in the same viewport pixels, or `null` for one the
   * camera cannot see.
   *
   * `null` is a coordinate behind a pitched camera or on a globe's far side. Both have a pixel the
   * projection would hand back and neither is on the map, which is why they are not a position.
   */
  geoToScreen(latitude, longitude) {
    const at = scratchAt(this.wasm).geo;
    const status = this.wasm.tessella_geo_to_screen(this.handle, latitude, longitude, at, at + 8);
    if (status === OFF_THE_MAP) {
      return null;
    }
    if (status !== OK) {
      throw new Error(`tessella_geo_to_screen answered ${status}`);
    }
    const view = new DataView(this.memory.buffer);
    return { x: view.getFloat64(at, true), y: view.getFloat64(at + 8, true) };
  }

  /** Tells the map how much time has passed. The only clock it has. */
  advance(elapsedMillis) {
    return this.wasm.tessella_advance(this.handle, elapsedMillis);
  }

  /**
   * Emits a frame, and answers the status rather than throwing on it.
   *
   * `tick` throws on anything but success, which is right for a caller that has no use for the
   * difference. A caller counting how often the ring was full does -- that status is the consumer
   * being behind, not the map failing.
   */
  step() {
    return this.wasm.tessella_tick(this.handle);
  }

  /**
   * How much work is still in flight. Zero means nothing further is coming without a tick.
   *
   * The count comes back through a pointer and the return is a status, like every other call
   * here. Reading the return as the count instead is a mistake that answers `1` for ever --
   * `NullArgument`, because the out pointer was `undefined` -- which looks exactly like a map
   * with one thing permanently outstanding. It cost an afternoon of looking for a stuck tile.
   */
  get pending() {
    const at = scratchAt(this.wasm).pending;
    if (this.wasm.tessella_pending(this.handle, at) !== OK) {
      throw new Error("tessella_pending refused");
    }
    return Number(new DataView(this.memory.buffer).getBigUint64(at, true));
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
    const asked = this.takeRequests();

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
        this.fail(ticket);
      } else {
        this.answer(ticket, answer.status, answer.body ?? new Uint8Array(0));
      }
    }
  }

  /**
   * Everything the map wants fetched, as `[ticket, url]` pairs, in the order it asked.
   *
   * @returns {[bigint, string][]}
   */
  takeRequests() {
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
    return asked;
  }

  /**
   * Hands a fetched body to the ticket that asked for it.
   *
   * Synchronous from the copy to the call, and that is the whole of what keeps the one scratch
   * block safe: nothing else can run between the body landing in it and the producer taking its
   * own copy.
   */
  answer(ticket, status, body = new Uint8Array(0)) {
    // Refused rather than truncated. A body this side cannot hold is a fetch that did not happen
    // as far as the map is concerned, and truncating one would hand the producer a tile that
    // decodes to something the origin never sent.
    if (body.length > MAX_BODY_BYTES) {
      return this.fail(ticket);
    }
    const into = scratchAt(this.wasm).body;
    new Uint8Array(this.memory.buffer, into, body.length).set(body);
    return this.wasm.tessella_answer(this.handle, ticket, status, into, body.length);
  }

  /** Says a ticket's fetch did not happen at all. */
  fail(ticket) {
    return this.wasm.tessella_fail_request(this.handle, ticket);
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
    geo: base + SCRATCH.geo,
    queryLen: base + SCRATCH.queryLen,
    queryArgs: base + SCRATCH.queryArgs,
    query: base + SCRATCH.query,
    pending: base + SCRATCH.pending,
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
  pending: 384,
  // Two doubles, for the pair of out parameters the screen conversions write. Sixteen bytes in a
  // slot of sixty-four, like every other slot here.
  geo: 448,
  url: 256,
  urlLen: 320,
  // One `usize`, for the length a query reports.
  queryLen: 512,
  style: 4096,
  body: 1 << 20,
  // A query's answer, and the arrays naming the layers it is given. Past `body` rather than carved
  // out of the low slots because the answer is a whole JSON document: a tap is a few hundred bytes
  // and a box over a dense layer is not, so it gets a megabyte and the pointer arrays get the
  // sixty-four kilobytes in front of it.
  queryArgs: (1 << 20) + (16 << 20),
  query: (1 << 20) + (16 << 20) + (1 << 16),
  total: (1 << 20) + (16 << 20) + (1 << 16) + (1 << 20),
});
