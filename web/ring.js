// Reading the capture stream out of a `WebAssembly.Memory`.
//
// The whole point of the arrangement: the producer writes geometry into its own linear memory and
// the consumer reads it there. Nothing is serialized, nothing is copied to make it reachable, and
// on wasm "its own linear memory" is `memory.buffer` -- an ArrayBuffer this side already has.
//
// Every offset comes from `abi.js`, which is generated from the Rust types by the same call that
// writes the C header's static assertions. Nothing here is hand-counted, because JavaScript has
// no compiler to catch it if it were.

import { LAYOUT, RECORD_FLAG_SKIP, PAYLOAD_ALIGN, ABI_REV } from "./abi.js";

const CONTROL = LAYOUT.tsl_ring_control;
const HEADER = LAYOUT.tsl_record_header;
const REGION = LAYOUT.tsl_slab_region;
const ENTRY = LAYOUT.tsl_slab_entry;
const REF = LAYOUT.tsl_slab_ref;

/** Rounds up the way the producer pads a record's fixed part. */
function alignUp(value, to) {
  return Math.ceil(value / to) * to;
}

/**
 * A view over the producer's memory that survives the memory growing.
 *
 * `memory.buffer` is *replaced* when a `WebAssembly.Memory` grows, and every `DataView` over the
 * old one is detached -- reading through a stale view throws, which is the good case, and is the
 * failure a consumer written against a native mapping would never meet. So the view is rebuilt
 * whenever the buffer identity changes rather than held.
 */
class Memory {
  /** @param {WebAssembly.Memory} memory */
  constructor(memory) {
    this.memory = memory;
    this.buffer = null;
    this.view = null;
  }

  /** @returns {DataView} */
  get at() {
    if (this.buffer !== this.memory.buffer) {
      this.buffer = this.memory.buffer;
      this.view = new DataView(this.buffer);
    }
    return this.view;
  }

  /** The bytes at `[offset, offset + length)`, copied out. */
  bytes(offset, length) {
    return new Uint8Array(this.memory.buffer, offset, length).slice();
  }
}

/** A range inside a record's payload, as `tsl_span` describes one. */
function span(view, at) {
  const SPAN = LAYOUT.tsl_span;
  return {
    offset: view.getUint32(at + SPAN.at.offset, true),
    count: view.getUint32(at + SPAN.at.count, true),
  };
}

/** A reference into the slab region, as `tsl_slab_ref` describes one. */
function slabRef(view, at) {
  return {
    slab: view.getUint32(at + REF.at.slab, true),
    offset: view.getUint32(at + REF.at.offset, true),
    length: view.getUint32(at + REF.at.length, true),
  };
}

/**
 * The slab region: a table of byte ranges every `tsl_slab_ref` resolves against.
 *
 * The handle indexes the table. That rule reached the generated header only because writing the C
 * consumer made its absence visible, and it is repeated here for the same reason: a consumer that
 * inferred it from the layout would have inferred it right and had nothing to check itself
 * against.
 */
export class Slabs {
  /**
   * @param {WebAssembly.Memory} memory
   * @param {number} pointer  where the region starts in linear memory
   * @param {number} length   how long it is
   */
  constructor(memory, pointer, length) {
    this.memory = new Memory(memory);
    this.pointer = pointer;
    this.length = length;
  }

  /** How many slabs the table covers. */
  get count() {
    return this.memory.at.getUint32(this.pointer + REGION.at.count, true);
  }

  /**
   * The bytes a reference names, or null if it names nothing.
   *
   * A handle the table does not cover has no meaning, and answering null is what turns a producer
   * fault into a diagnosis rather than a wild read.
   */
  resolve(ref) {
    if (this.memory.at.getUint32(this.pointer + REGION.at.abi_rev, true) !== ABI_REV) {
      return null;
    }
    if (ref.slab >= this.count) {
      return null;
    }
    const entry = this.pointer + REGION.size + ref.slab * ENTRY.size;
    if (entry + ENTRY.size > this.pointer + this.length) {
      return null;
    }
    const base = Number(this.memory.at.getBigUint64(entry + ENTRY.at.offset, true));
    const len = Number(this.memory.at.getBigUint64(entry + ENTRY.at.length, true));
    if (ref.offset + ref.length > len) {
      return null;
    }
    return this.memory.bytes(this.pointer + base + ref.offset, ref.length);
  }
}

/**
 * The ring: a control block, then a power-of-two data region.
 *
 * `head` and `tail` are free-running byte counters rather than indices, so full and empty are
 * never ambiguous. This reads `head` once per pass, as a consumer racing a live producer must --
 * a head that advanced mid-pass belongs to the next one.
 */
export class Ring {
  /**
   * @param {WebAssembly.Memory} memory
   * @param {number} pointer  where the control block starts in linear memory
   * @param {number} length   control block plus data
   */
  constructor(memory, pointer, length) {
    this.memory = new Memory(memory);
    this.pointer = pointer;
    this.length = length;
  }

  /** The revision the producer wrote this region with. */
  get revision() {
    return this.memory.at.getUint32(this.pointer + CONTROL.at.abi_rev, true);
  }

  /** Bytes in the data region. Always a power of two. */
  get capacity() {
    return Number(this.memory.at.getBigUint64(this.pointer + CONTROL.at.capacity, true));
  }

  /** Bytes ever written. */
  get head() {
    return Number(this.memory.at.getBigUint64(this.pointer + CONTROL.at.head, true));
  }

  /** Bytes ever consumed. */
  get tail() {
    return Number(this.memory.at.getBigUint64(this.pointer + CONTROL.at.tail, true));
  }

  set tail(value) {
    this.memory.at.setBigUint64(this.pointer + CONTROL.at.tail, BigInt(value), true);
  }

  /**
   * Every record the producer has published and this consumer has not taken.
   *
   * Publishes `tail` at the end, which is what gives the producer its space back. A consumer that
   * never published it would run a fixed-size ring into a wall.
   *
   * @returns {{kind: number, fixedAt: number, recordLen: number, payloadAt: number,
   *            payloadLen: number}[]}
   */
  drain() {
    if (this.revision !== ABI_REV) {
      throw new Error(`ring is ABI revision ${this.revision}, this consumer speaks ${ABI_REV}`);
    }
    const data = this.pointer + CONTROL.size;
    const capacity = this.capacity;
    const head = this.head;
    let cursor = this.tail;
    const out = [];

    while (cursor < head) {
      const offset = cursor % capacity;
      // Records never straddle the wrap: the producer covers the remainder with a skip record
      // rather than splitting one, so a header that would straddle is a producer fault.
      if (capacity - offset < HEADER.size) {
        throw new Error(`a record header straddles the wrap at ${cursor}`);
      }
      const at = data + offset;
      const view = this.memory.at;
      const kind = view.getUint16(at + HEADER.at.kind, true);
      const flags = view.getUint16(at + HEADER.at.flags, true);
      const recordLen = view.getUint32(at + HEADER.at.record_len, true);
      const payloadLen = view.getUint32(at + HEADER.at.payload_len, true);
      const totalLen = view.getUint32(at + HEADER.at.total_len, true);

      if (totalLen < HEADER.size || totalLen > capacity) {
        throw new Error(`record at ${cursor} claims ${totalLen} bytes`);
      }
      cursor += totalLen;
      if (flags & RECORD_FLAG_SKIP) {
        continue;
      }
      const body = alignUp(recordLen, PAYLOAD_ALIGN);
      if (HEADER.size + body + payloadLen > totalLen) {
        throw new Error(`record at ${cursor} overruns itself`);
      }
      out.push({
        kind,
        fixedAt: at + HEADER.size,
        recordLen,
        payloadAt: at + HEADER.size + body,
        payloadLen,
      });
    }

    this.tail = cursor;
    return out;
  }
}

/**
 * One `tsl_geometry_add`, as far as a fill needs it.
 *
 * Not the whole record: what a first consumer draws is triangles, which is the vertex bytes, the
 * index bytes and how many vertices there are. The rest of the envelope is read by a consumer
 * that draws more than fills.
 */
export function geometryAdd(memory, record) {
  const add = LAYOUT.tsl_geometry_add;
  const view = new DataView(memory.buffer);
  return {
    id: view.getBigUint64(record.fixedAt + add.at.geometry, true),
    vertexCount: view.getUint32(record.fixedAt + add.at.vertex_count, true),
    indexes: slabRef(view, record.fixedAt + add.at.indexes),
    // A span into this record's payload, not a slab handle. The two are eight and twelve bytes
    // and both start with a small integer, so reading one as the other is quiet until something
    // resolves it -- which is exactly what happened before a renderer needed the attributes.
    attrs: span(view, record.fixedAt + add.at.attrs),
    builtinShader: view.getUint32(record.fixedAt + add.at.builtin_shader, true),
  };
}

/**
 * The vertex attributes a geometry declares, read out of its payload.
 *
 * Each names a slab holding the bytes and how to step through them, so a consumer binds a buffer
 * rather than copying vertices anywhere.
 */
export function attributes(memory, record, add) {
  const DESC = LAYOUT.tsl_attribute_desc;
  const view = new DataView(memory.buffer);
  const out = [];
  for (let i = 0; i < add.attrs.count; i++) {
    const at = record.payloadAt + add.attrs.offset + i * DESC.size;
    out.push({
      id: view.getUint32(at + DESC.at.attr_id, true),
      source: slabRef(view, at + DESC.at.source),
      offset: view.getUint32(at + DESC.at.offset, true),
      vertexOffset: view.getUint32(at + DESC.at.vertex_offset, true),
      stride: view.getUint32(at + DESC.at.stride, true),
      dataType: view.getUint8(at + DESC.at.data_type),
    });
  }
  return out;
}
