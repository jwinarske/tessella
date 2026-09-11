// A fingerprint of what a map published, comparable with the native driver's.
//
// Run A's claim is that the browser's record stream is the native serial one. The streams are
// megabytes, so what is compared is a hash per map per frame: FNV-1a 64 over every record's kind,
// fixed part and payload, and a second one over the slab bytes the frame's geometry names. The
// second matters because a record names its vertices by handle -- two streams can agree on every
// record and still disagree on every vertex.
//
// # Except the camera, which is carried whole
//
// A `tsl_camera_update` is f64 matrices computed with `tan`, `exp`, `ln` and `atan`, and
// `tessella-tile` takes those from the platform's libm on purpose: bit-exact agreement with the
// C++ oracle, which links the same one, is the projection's contract. wasm's libm is not glibc's,
// and the two disagree in the last bits of a handful of those doubles -- nothing else, over the
// whole sweep. So the camera records are left out of the stream hash (their kind and lengths stay
// in it) and carried as hex instead, and the harness holds them to "every byte identical except
// the f64 fields, and those within a few ulps". Hashing them would turn five ulps into a failure
// that says nothing about which bytes or by how much.
//
// Both sides feed the hash the same bytes in the same order; `examples/sweep_trace.rs` is the
// other half, and a change here is a change there.

import { KIND, LAYOUT } from "../abi.js";
import { geometryAdd, attributes } from "../ring.js";

/** Where an integer is laid out little-endian to be hashed, so hashing one allocates nothing. */
const SCRATCH = new Uint8Array(4);
const SCRATCH_VIEW = new DataView(SCRATCH.buffer);

/**
 * FNV-1a, 64-bit, in four 16-bit limbs.
 *
 * `BigInt` per byte is correct and far too slow for megabytes a frame. The prime is
 * `2^40 + 0x1b3`, so multiplying is `h * 0x1b3` plus `h` shifted up 40 bits, which in limbs is
 * the low two shifted into the high two by eight.
 */
export class Fnv64 {
  constructor() {
    // 0xcbf29ce484222325
    this.h0 = 0x2325;
    this.h1 = 0x8422;
    this.h2 = 0x9ce4;
    this.h3 = 0xcbf2;
  }

  /** @param {Uint8Array} bytes */
  bytes(bytes) {
    let { h0, h1, h2, h3 } = this;
    for (let i = 0; i < bytes.length; i++) {
      h0 ^= bytes[i];
      const t0 = h0 * 0x1b3;
      let t1 = h1 * 0x1b3;
      let t2 = h2 * 0x1b3 + (h0 << 8);
      const t3 = h3 * 0x1b3 + (h1 << 8);
      t1 += t0 >>> 16;
      h0 = t0 & 0xffff;
      t2 += t1 >>> 16;
      h1 = t1 & 0xffff;
      h3 = (t3 + (t2 >>> 16)) & 0xffff;
      h2 = t2 & 0xffff;
    }
    Object.assign(this, { h0, h1, h2, h3 });
    return this;
  }

  u16(value) {
    SCRATCH_VIEW.setUint16(0, value, true);
    return this.bytes(SCRATCH.subarray(0, 2));
  }

  u32(value) {
    SCRATCH_VIEW.setUint32(0, value >>> 0, true);
    return this.bytes(SCRATCH);
  }

  /** As sixteen hex digits, which is how the native side prints a `u64`. */
  hex() {
    return [this.h3, this.h2, this.h1, this.h0]
      .map((l) => l.toString(16).padStart(4, "0"))
      .join("");
  }
}

/**
 * One map's frame, fingerprinted.
 *
 * @param {import("../map.js").TessellaMap} map
 * @param {{kind: number, fixedAt: number, recordLen: number, payloadAt: number,
 *          payloadLen: number}[]} records
 */
export function fingerprint(map, records) {
  const stream = new Fnv64();
  const slabs = new Fnv64();
  const cameras = [];
  const buffer = map.memory.buffer;
  for (const record of records) {
    const fixed = new Uint8Array(buffer, record.fixedAt, record.recordLen);
    stream.u16(record.kind);
    stream.u32(record.recordLen);
    if (record.kind === KIND.CAMERA_UPDATE) {
      cameras.push(hex(fixed));
    } else {
      stream.bytes(fixed);
    }
    stream.u32(record.payloadLen);
    stream.bytes(new Uint8Array(buffer, record.payloadAt, record.payloadLen));

    if (record.kind === KIND.GEOMETRY_ADD) {
      const add = geometryAdd(map.memory, record);
      for (const ref of [
        add.indexes,
        ...attributes(map.memory, record, add).map((a) => a.source),
      ]) {
        const bytes = map.slabs.resolve(ref);
        // A reference that names nothing is written as a length no real one can have, so a
        // stream where it resolves and one where it does not cannot hash alike.
        if (bytes === null) {
          slabs.u32(0xffffffff);
        } else {
          slabs.u32(bytes.length);
          slabs.bytes(bytes);
        }
      }
    }
  }
  return { records: records.length, hash: stream.hex(), slabs: slabs.hex(), cameras };
}

function hex(bytes) {
  let out = "";
  for (const byte of bytes) {
    out += byte.toString(16).padStart(2, "0");
  }
  return out;
}

/**
 * How two camera records differ: the largest ulp distance over their f64 fields, and whether any
 * other byte differs at all.
 *
 * The f64 fields are the ones before `light`, which is where the layout says they end -- taken
 * from `abi.js`, like every other offset here, rather than counted.
 *
 * @param {string} a  hex, as `fingerprint` carries it
 * @param {string} b
 */
export function cameraDistance(a, b) {
  const x = hexBytes(a);
  const y = hexBytes(b);
  if (x.length !== y.length) {
    return { ulps: Infinity, otherBytes: Math.abs(x.length - y.length) };
  }
  const floats = LAYOUT.tsl_camera_update.at.light;
  const vx = new DataView(x.buffer);
  const vy = new DataView(y.buffer);
  let ulps = 0n;
  for (let at = 0; at < floats; at += 8) {
    const d = ordered(vx.getBigInt64(at, true)) - ordered(vy.getBigInt64(at, true));
    const distance = d < 0n ? -d : d;
    if (distance > ulps) {
      ulps = distance;
    }
  }
  let otherBytes = 0;
  for (let at = floats; at < x.length; at++) {
    if (x[at] !== y[at]) {
      otherBytes++;
    }
  }
  return { ulps: Number(ulps), otherBytes };
}

/** An f64's bits as an integer that orders the way the doubles do, so a difference counts ulps. */
function ordered(bits) {
  return bits < 0n ? -0x8000000000000000n - bits : bits;
}

function hexBytes(text) {
  const out = new Uint8Array(text.length / 2);
  for (let i = 0; i < out.length; i++) {
    out[i] = parseInt(text.slice(i * 2, i * 2 + 2), 16);
  }
  return out;
}
