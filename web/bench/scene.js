// What one view is showing, as the stream says it, with no GPU.
//
// Two of the benchmark's gates are statements about the frame and not about pixels: every view
// fully covered, and no view blank. A runner with no GPU still has to be able to check both, so
// this follows the records a renderer would and answers them from the stream alone.
//
// Coverage is read off the clip sets. A `tsl_stencil_tiles` names every tile a layer group draws
// -- the cover, plus any ancestor standing in for a child that has not arrived -- with the matrix
// that takes the tile's square to clip space. Where those squares leave part of clip space
// uncovered the view has a hole, whatever a renderer would have done about it. The scene has to
// put that layer in every tile for the statement to mean "the tiles cover the view" rather than
// "the layer does", which is why the benchmark's style is one fixture whose `earth` polygon
// covers the whole tile.

import { KIND, LAYOUT, BUILTIN } from "../abi.js";

const ORDER = LAYOUT.tsl_order_entry;
const SPAN = LAYOUT.tsl_span;
const STENCIL = LAYOUT.tsl_stencil_tiles;
const STENCIL_TILE = LAYOUT.tsl_stencil_tile;
const ADD = LAYOUT.tsl_geometry_add;

/** Tile-local units across a tile, which is what a mask quad spans. mbgl's `util::EXTENT`. */
const EXTENT = 8192;

export class Scene {
  constructor() {
    /** Live geometry, by id, and the shader family each was announced with. */
    this.live = new Map();
    /** The current draw list's geometry ids. */
    this.order = [];
    /** The latest clip set per layer index, as clip-space quads. */
    this.clips = new Map();
  }

  /** @param {WebAssembly.Memory} memory */
  absorb(memory, records) {
    const view = new DataView(memory.buffer);
    for (const record of records) {
      if (record.kind === KIND.GEOMETRY_ADD) {
        const id = view.getBigUint64(record.fixedAt + ADD.at.geometry, true);
        this.live.set(id, view.getInt32(record.fixedAt + ADD.at.builtin_shader, true));
      } else if (record.kind === KIND.GEOMETRY_REMOVE) {
        this.live.delete(view.getBigUint64(record.fixedAt, true));
      } else if (record.kind === KIND.ORDER_UPDATE) {
        const at = record.fixedAt + LAYOUT.tsl_order_update.at.entries;
        const offset = view.getUint32(at + SPAN.at.offset, true);
        const count = view.getUint32(at + SPAN.at.count, true);
        this.order = [];
        for (let i = 0; i < count; i++) {
          this.order.push(view.getBigUint64(record.payloadAt + offset + i * ORDER.size, true));
        }
      } else if (record.kind === KIND.STENCIL_TILES) {
        const layer = view.getInt32(record.fixedAt + STENCIL.at.layer_index, true);
        const at = record.fixedAt + STENCIL.at.tiles;
        const offset = view.getUint32(at + SPAN.at.offset, true);
        const count = view.getUint32(at + SPAN.at.count, true);
        const quads = [];
        for (let i = 0; i < count; i++) {
          const tile = record.payloadAt + offset + i * STENCIL_TILE.size;
          const m = new Float64Array(16);
          for (let k = 0; k < 16; k++) {
            m[k] = view.getFloat32(tile + STENCIL_TILE.at.matrix + k * 4, true);
          }
          quads.push(quad(m));
        }
        // Replaced whole, like the order: a clip set is the layer's tiles now, not additions.
        this.clips.set(layer, quads);
      }
    }
  }

  /**
   * How many entries of the draw list are fills whose geometry is live.
   *
   * Zero is a blank view: what is on screen is the clear color and nothing the style drew.
   */
  fills() {
    let count = 0;
    for (const id of this.order) {
      if (this.live.get(id) === BUILTIN.FILL_SHADER) {
        count++;
      }
    }
    return count;
  }

  /**
   * Whether the layer's tiles cover the whole view, sampled on a grid of pixel centers.
   *
   * @param {number} layer    the layer index whose clip set is the statement
   * @param {number} columns  samples across
   * @param {number} rows     samples down
   */
  covered(layer, columns, rows) {
    const quads = this.clips.get(layer);
    if (!quads || quads.length === 0) {
      return false;
    }
    for (let r = 0; r < rows; r++) {
      const y = -1 + (2 * (r + 0.5)) / rows;
      for (let c = 0; c < columns; c++) {
        const x = -1 + (2 * (c + 0.5)) / columns;
        if (!quads.some((q) => inside(q, x, y))) {
          return false;
        }
      }
    }
    return true;
  }
}

/** A tile's square in clip space: its four corners, after the divide. */
function quad(m) {
  const corner = (x, y) => {
    // Column-major, as the record carries it.
    const cx = m[0] * x + m[4] * y + m[12];
    const cy = m[1] * x + m[5] * y + m[13];
    const cw = m[3] * x + m[7] * y + m[15];
    return [cx / cw, cy / cw];
  };
  return [corner(0, 0), corner(EXTENT, 0), corner(EXTENT, EXTENT), corner(0, EXTENT)];
}

/**
 * Whether a point is inside a convex quad, edges included.
 *
 * The sign of each edge's cross product, taken consistently, whichever way the quad winds -- a
 * tile matrix flips y, so a square that winds one way in tile space winds the other on screen.
 */
function inside(quad, x, y) {
  let sign = 0;
  for (let i = 0; i < 4; i++) {
    const [ax, ay] = quad[i];
    const [bx, by] = quad[(i + 1) % 4];
    const cross = (bx - ax) * (y - ay) - (by - ay) * (x - ax);
    if (cross === 0) {
      continue;
    }
    if (sign === 0) {
      sign = Math.sign(cross);
    } else if (Math.sign(cross) !== sign) {
      return false;
    }
  }
  return true;
}
