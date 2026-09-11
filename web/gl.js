// Drawing the capture stream with WebGL2.
//
// Fills only. That is the smallest thing that is a *map* rather than a demonstration -- water,
// land, parks and buildings are fills -- and it exercises every part of the arrangement: the draw
// order, the per-drawable matrix, the layer's evaluated paint, and vertex buffers built straight
// out of the producer's linear memory.
//
// The draw list is the producer's, not this side's. `tsl_order_update` carries every drawable in
// the order it should be drawn, with the buffer slot its matrix lives at, so a consumer does not
// sort, group, or decide anything -- it binds and draws. That is the whole point of the record
// stream: the hard decisions were made where the style was.

import { KIND, UBO, BUILTIN, STRIDE, ATTRIBUTE_TYPE, LAYOUT, BLOCK } from "./abi.js";
import { geometryAdd, attributes } from "./ring.js";

const ORDER = LAYOUT.tsl_order_entry;
const UBO_UPDATE = LAYOUT.tsl_ubo_update;
const SPAN = LAYOUT.tsl_span;
// The paint block, which is a *uniform block* rather than a record struct -- mbgl declares it and
// the producer packs it, so its offsets live in `BLOCK` and not `LAYOUT`.
const FILL_PROPS = BLOCK.FillEvaluatedPropsUBO;

/** The position attribute a fill declares. */
const FILL_POSITION = UBO.ID_FILL_POS_VERTEX_ATTRIBUTE;

const VERTEX = `#version 300 es
// The matrix is the producer's, per drawable: it takes tile-local coordinates to clip space, and
// carries the tile's place in the world with it. Identity is not a neutral substitute -- it puts
// tile coordinates straight into clip space, where they cover the viewport.
uniform mat4 u_matrix;
in vec2 a_position;
void main() {
  gl_Position = u_matrix * vec4(a_position, 0.0, 1.0);
}`;

const FRAGMENT = `#version 300 es
precision mediump float;
uniform vec4 u_color;
out vec4 fragment;
void main() {
  fragment = u_color;
}`;

function compile(gl, type, source) {
  const shader = gl.createShader(type);
  gl.shaderSource(shader, source);
  gl.compileShader(shader);
  if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) {
    throw new Error(`shader: ${gl.getShaderInfoLog(shader)}`);
  }
  return shader;
}

function link(gl) {
  const program = gl.createProgram();
  gl.attachShader(program, compile(gl, gl.VERTEX_SHADER, VERTEX));
  gl.attachShader(program, compile(gl, gl.FRAGMENT_SHADER, FRAGMENT));
  gl.linkProgram(program);
  if (!gl.getProgramParameter(program, gl.LINK_STATUS)) {
    throw new Error(`program: ${gl.getProgramInfoLog(program)}`);
  }
  return program;
}

/** How a vertex attribute's type maps onto `vertexAttribPointer`. */
function attributeFormat(gl, dataType) {
  switch (dataType) {
    case ATTRIBUTE_TYPE.SHORT2:
      return { size: 2, type: gl.SHORT, normalized: false };
    case ATTRIBUTE_TYPE.USHORT2:
      return { size: 2, type: gl.UNSIGNED_SHORT, normalized: false };
    case ATTRIBUTE_TYPE.FLOAT2:
      return { size: 2, type: gl.FLOAT, normalized: false };
    default:
      return null;
  }
}

/**
 * Draws what the producer publishes.
 *
 * Holds GL buffers keyed by the producer's geometry id, so a tile that stays on screen is
 * uploaded once and drawn every frame -- which is what the id is for.
 */
export class FillRenderer {
  /** @param {WebGL2RenderingContext} gl */
  constructor(gl) {
    this.gl = gl;
    this.program = link(gl);
    this.uMatrix = gl.getUniformLocation(this.program, "u_matrix");
    this.uColor = gl.getUniformLocation(this.program, "u_color");
    /** @type {Map<string, {vao: WebGLVertexArrayObject, buffers: WebGLBuffer[], indexes: number}>} */
    this.geometry = new Map();
    /** @type {Map<string, Uint8Array>} */
    this.uniforms = new Map();
    /** @type {{geometry: bigint, layer: number, uboIndex: number}[]} */
    this.order = [];
    this.background = [0, 0, 0, 1];
  }

  /** Where a layer's buffer for one slot is kept. */
  static #key(layer, slot) {
    return `${layer}:${slot}`;
  }

  /**
   * Takes a tick's records.
   *
   * @param {import("./map.js").TessellaMap} map
   * @param {{kind: number, fixedAt: number, payloadAt: number, payloadLen: number}[]} records
   */
  absorb(map, records) {
    const gl = this.gl;
    // One view for the pass. Records do not grow the memory, so the buffer cannot be swapped out
    // from under it here -- and a view per record is an allocation per record on the hot path.
    const view = new DataView(map.memory.buffer);
    for (const record of records) {
      if (record.kind === KIND.UBO_UPDATE) {
        const layer = view.getInt32(record.fixedAt + UBO_UPDATE.at.layer_index, true);
        const slot = view.getUint32(record.fixedAt + UBO_UPDATE.at.slot, true);
        const at = record.fixedAt + UBO_UPDATE.at.data;
        const offset = view.getUint32(at + SPAN.at.offset, true);
        const count = view.getUint32(at + SPAN.at.count, true);
        this.uniforms.set(
          FillRenderer.#key(layer, slot),
          new Uint8Array(map.memory.buffer, record.payloadAt + offset, count).slice(),
        );
      } else if (record.kind === KIND.ORDER_UPDATE) {
        // Replaced whole. An order update is the frame's draw list, not an addition to it.
        this.order = [];
        const at = record.fixedAt + LAYOUT.tsl_order_update.at.entries;
        const offset = view.getUint32(at + SPAN.at.offset, true);
        const count = view.getUint32(at + SPAN.at.count, true);
        for (let i = 0; i < count; i++) {
          const entry = record.payloadAt + offset + i * ORDER.size;
          this.order.push({
            geometry: view.getBigUint64(entry + ORDER.at.geometry, true),
            layer: view.getUint32(entry + ORDER.at.layer_index, true),
            uboIndex: view.getUint32(entry + ORDER.at.ubo_index, true),
          });
        }
      } else if (record.kind === KIND.GEOMETRY_ADD) {
        this.#upload(map, record);
      } else if (record.kind === KIND.GEOMETRY_REMOVE) {
        const id = view.getBigUint64(record.fixedAt, true).toString();
        const held = this.geometry.get(id);
        if (held) {
          // The buffers too, not just the array object. Deleting the VAO alone leaves the vertex
          // and index buffers allocated for the life of the context, which on a map that pans is
          // every tile ever seen.
          gl.deleteVertexArray(held.vao);
          for (const buffer of held.buffers) {
            gl.deleteBuffer(buffer);
          }
          this.geometry.delete(id);
        }
      }
    }
  }

  /** Builds the buffers for one geometry, if it is a fill this can draw. */
  #upload(map, record) {
    const gl = this.gl;
    const add = geometryAdd(map.memory, record);
    if (add.builtinShader !== BUILTIN.FILL_SHADER) {
      return;
    }
    const position = attributes(map.memory, record, add).find((a) => a.id === FILL_POSITION);
    const format = position && attributeFormat(gl, position.dataType);
    const vertices = position && map.slabs.resolve(position.source);
    const indexes = map.slabs.resolve(add.indexes);
    if (!format || !vertices || !indexes) {
      return;
    }

    const vao = gl.createVertexArray();
    gl.bindVertexArray(vao);

    const vertexBuffer = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, vertexBuffer);
    gl.bufferData(gl.ARRAY_BUFFER, vertices, gl.STATIC_DRAW);
    const slot = gl.getAttribLocation(this.program, "a_position");
    gl.enableVertexAttribArray(slot);
    gl.vertexAttribPointer(
      slot,
      format.size,
      format.type,
      format.normalized,
      position.stride,
      position.offset + position.vertexOffset * position.stride,
    );

    const indexBuffer = gl.createBuffer();
    gl.bindBuffer(gl.ELEMENT_ARRAY_BUFFER, indexBuffer);
    gl.bufferData(gl.ELEMENT_ARRAY_BUFFER, indexes, gl.STATIC_DRAW);

    gl.bindVertexArray(null);
    // Indices are 16-bit, which is what the producer's triangulation emits.
    this.geometry.set(add.id.toString(), {
      vao,
      buffers: [vertexBuffer, indexBuffer],
      indexes: indexes.length / 2,
    });
  }

  /** The layer's evaluated fill color, or null if it has not arrived. */
  #colorOf(layer) {
    const bytes = this.uniforms.get(FillRenderer.#key(layer, UBO.ID_FILL_EVALUATED_PROPS_UBO));
    if (!bytes || bytes.length < FILL_PROPS.at.color + 16) {
      return null;
    }
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    return [0, 1, 2, 3].map((i) => view.getFloat32(FILL_PROPS.at.color + i * 4, true));
  }

  /** The matrix this drawable is placed by, or null if its buffer has not arrived. */
  #matrixOf(layer, uboIndex) {
    const bytes = this.uniforms.get(FillRenderer.#key(layer, UBO.ID_FILL_DRAWABLE_UBO));
    const at = uboIndex * STRIDE.FILL_DRAWABLE_UNION_UBO;
    if (!bytes || at + 64 > bytes.length) {
      return null;
    }
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    return new Float32Array(16).map((_, i) => view.getFloat32(at + i * 4, true));
  }

  /** Draws the current order into the whole canvas. */
  draw(width, height) {
    return this.drawViewport(0, 0, width, height);
  }

  /**
   * Draws the current order into one rectangle of the canvas, leaving the rest alone.
   *
   * Several renderers sharing one context each take a rectangle, which is how several views are
   * drawn in one frame and presented together. The scissor is what confines the clear: a
   * viewport alone bounds where triangles land, not what `clear` touches, so without it each
   * view would wipe the ones drawn before it.
   */
  drawViewport(x, y, width, height) {
    const gl = this.gl;
    gl.viewport(x, y, width, height);
    gl.enable(gl.SCISSOR_TEST);
    gl.scissor(x, y, width, height);
    gl.clearColor(...this.background);
    gl.clear(gl.COLOR_BUFFER_BIT);
    gl.useProgram(this.program);
    gl.disable(gl.DEPTH_TEST);
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.ONE, gl.ONE_MINUS_SRC_ALPHA);

    let drawn = 0;
    for (const entry of this.order) {
      const held = this.geometry.get(entry.geometry.toString());
      if (!held) {
        continue;
      }
      const matrix = this.#matrixOf(entry.layer, entry.uboIndex);
      const color = this.#colorOf(entry.layer);
      // Skipped rather than defaulted. An identity matrix covers the viewport and a black fill
      // hides what is under it, so either substitute looks like a bug somewhere else.
      if (!matrix || !color) {
        continue;
      }
      gl.uniformMatrix4fv(this.uMatrix, false, matrix);
      // The producer's colors are premultiplied, which is what the blend above expects.
      gl.uniform4fv(this.uColor, color);
      gl.bindVertexArray(held.vao);
      gl.drawElements(gl.TRIANGLES, held.indexes, gl.UNSIGNED_SHORT, 0);
      drawn++;
    }
    gl.bindVertexArray(null);
    gl.disable(gl.SCISSOR_TEST);
    return drawn;
  }
}
