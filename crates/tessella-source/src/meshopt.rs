//! The `EXT_meshopt_compression` bitstream, transcribed from meshoptimizer.
//!
//! # Where the specification stops
//!
//! `EXT_meshopt_compression` is a Khronos extension and its *container* is specified — the modes,
//! the filters, the fallback-buffer arrangement, all of which [`crate::gltf`] reads. The
//! **bitstream** is not: the extension defers it to meshoptimizer, so the reference
//! implementation is the specification. This is transcribed from `vertexcodec.cpp`,
//! `indexcodec.cpp` and `vertexfilter.cpp`, the way everything else here is transcribed from
//! maplibre-native.
//!
//! Only the *scalar* paths are ported. The reference carries SSE, AVX512, NEON and WASM variants
//! of the same functions, and each is an optimisation of the scalar one rather than a different
//! answer — meshoptimizer's own tests check them against each other. Porting the scalar form
//! keeps this `forbid(unsafe_code)` and portable, which the SIMD forms could not be.
//!
//! # Why this is transcribed rather than depended upon
//!
//! `meshopt-rs` exists and is a careful port. It was not taken because every byte here comes off
//! a network into a decoder, and every other decoder this workspace carries is free of unsafe,
//! which `#![forbid(unsafe_code)]` holds it to. `meshopt-rs` uses `from_raw_parts` to reinterpret typed slices
//! and a union for float punning, neither of which is *wrong*, but neither of which this build
//! has to accept when the reference is available and the scalar decoders are three hundred lines.
//!
//! # Bounds
//!
//! The reference returns an error code rather than trusting its input, and every one of those
//! checks is kept. A decoder for attacker-influenceable bytes that indexed on a count from the
//! same bytes would be the whole vulnerability.

use alloc::vec;
use alloc::vec::Vec;

/// Why a stream did not decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MeshoptError {
    /// The header byte is not this codec's, or names a version this does not implement.
    #[error("the stream header is not a meshopt {what} stream of a known version")]
    Header {
        /// Which codec was expected.
        what: &'static str,
    },
    /// The stream ended before the data it promised.
    #[error("the meshopt stream ends early")]
    Truncated,
    /// The stream had bytes left over where the format fixes the tail exactly.
    ///
    /// Not pedantry: the vertex codec's tail *is* the initial predictor state, so a stream whose
    /// tail is the wrong size has been decoded against the wrong starting vertex and every value
    /// in it is wrong by a constant.
    #[error("the meshopt stream has a {actual}-byte tail where {expected} was required")]
    Tail {
        /// What was found.
        actual: usize,
        /// What the format fixes.
        expected: usize,
    },
    /// A size the codec cannot express.
    #[error("a vertex size of {0} is not a multiple of four in 1..=256")]
    VertexSize(usize),
    /// An index the stream produced points outside the vertices it was decoded with.
    #[error("the stream produced index {index}, past the {count} vertices declared")]
    IndexOutOfRange {
        /// What it produced.
        index: u32,
        /// How many there are.
        count: usize,
    },
}

const VERTEX_HEADER: u8 = 0xa0;
const VERTEX_BLOCK_SIZE_BYTES: usize = 8192;
const VERTEX_BLOCK_MAX_SIZE: usize = 256;
const BYTE_GROUP_SIZE: usize = 16;
const BYTE_GROUP_DECODE_LIMIT: usize = 24;
const TAIL_MAX_SIZE: usize = 32;

/// How many vertices one block holds, for a given vertex size.
///
/// The scratch buffer is fixed, so a wider vertex means fewer per block; the result is then
/// truncated to a multiple of the byte-group size because each byte is encoded as a group and a
/// misaligned block wastes the remainder.
const fn vertex_block_size(vertex_size: usize) -> usize {
    let mut result = VERTEX_BLOCK_SIZE_BYTES / vertex_size;
    result &= !(BYTE_GROUP_SIZE - 1);
    if result < VERTEX_BLOCK_MAX_SIZE {
        result
    } else {
        VERTEX_BLOCK_MAX_SIZE
    }
}

/// Undoes the zigzag that maps a signed delta onto an unsigned byte.
const fn unzigzag8(value: u8) -> u8 {
    // `-(v & 1)` as a mask, which is the reference's `-(v & 1)` written without a cast chain.
    (0u8.wrapping_sub(value & 1)) ^ (value >> 1)
}

/// Decodes one sixteen-byte group at the given bit width.
///
/// The four widths are the whole of the byte codec: zero bits means the group is all zeros, two
/// and four bits pack several values per byte with an escape to a sentinel that reads a full byte
/// from a second cursor, and eight bits is the bytes themselves. Returning the new position of
/// *both* cursors is why this reads awkwardly — the escape stream runs behind the packed one.
fn decode_bytes_group(data: &[u8], out: &mut [u8], bitslog2: u8) -> Option<usize> {
    match bitslog2 {
        0 => {
            out[..BYTE_GROUP_SIZE].fill(0);
            Some(0)
        }
        1 => decode_packed(data, out, 2),
        2 => decode_packed(data, out, 4),
        3 => {
            let group = data.get(..BYTE_GROUP_SIZE)?;
            out[..BYTE_GROUP_SIZE].copy_from_slice(group);
            Some(BYTE_GROUP_SIZE)
        }
        _ => None,
    }
}

/// The two- and four-bit cases, which share everything but their width.
///
/// `bits` values are packed most-significant-first into each byte, and the all-ones value is an
/// escape meaning "take the next byte from the overflow cursor". The overflow cursor starts after
/// the packed bytes, which is why the two advance independently.
fn decode_packed(data: &[u8], out: &mut [u8], bits: u32) -> Option<usize> {
    let per_byte = 8 / bits as usize;
    let packed = BYTE_GROUP_SIZE / per_byte;
    let sentinel = (1u16 << bits) - 1;

    let mut overflow = packed;
    let mut written = 0usize;
    for index in 0..packed {
        let mut byte = *data.get(index)?;
        for _ in 0..per_byte {
            #[allow(clippy::cast_possible_truncation)]
            let value = u16::from(byte >> (8 - bits));
            byte = byte.wrapping_shl(bits);
            out[written] = if value == sentinel {
                let escaped = *data.get(overflow)?;
                overflow += 1;
                escaped
            } else {
                #[allow(clippy::cast_possible_truncation)]
                {
                    value as u8
                }
            };
            written += 1;
        }
    }
    Some(overflow)
}

/// Decodes `buffer_size` bytes of one transposed component stream.
fn decode_bytes(data: &[u8], out: &mut [u8]) -> Option<usize> {
    let buffer_size = out.len();
    debug_assert!(buffer_size.is_multiple_of(BYTE_GROUP_SIZE));

    // Two bits of width per group, four groups to a header byte.
    let header_size = buffer_size.div_ceil(BYTE_GROUP_SIZE).div_ceil(4);
    let header = data.get(..header_size)?;
    let mut at = header_size;

    for start in (0..buffer_size).step_by(BYTE_GROUP_SIZE) {
        // The reference requires the decode limit to be *available* before each group rather
        // than checking each read: a group's escape cursor can run up to eight bytes past its
        // packed bytes, and the limit is what makes that safe to attempt.
        if data.len().checked_sub(at)? < BYTE_GROUP_DECODE_LIMIT {
            return None;
        }
        let group = start / BYTE_GROUP_SIZE;
        let bitslog2 = (header[group / 4] >> ((group % 4) * 2)) & 3;
        let consumed = decode_bytes_group(
            &data[at..],
            &mut out[start..start + BYTE_GROUP_SIZE],
            bitslog2,
        )?;
        at += consumed;
    }
    Some(at)
}

/// Decodes one block of vertices, transposed and delta-coded against the previous vertex.
///
/// The transposition is the codec's central idea: all the first bytes of every vertex are stored
/// together, then all the second bytes, and so on. Neighbouring vertices differ little, so a
/// column of one byte's worth of a coordinate is nearly constant and compresses to almost
/// nothing — where the interleaved form would mix a high byte's stability with a low byte's noise.
fn decode_vertex_block(
    data: &[u8],
    out: &mut [u8],
    vertex_count: usize,
    vertex_size: usize,
    last_vertex: &mut [u8],
) -> Option<usize> {
    debug_assert!(vertex_count > 0 && vertex_count <= VERTEX_BLOCK_MAX_SIZE);

    let mut buffer = vec![0u8; VERTEX_BLOCK_MAX_SIZE];
    let mut transposed = vec![0u8; vertex_count * vertex_size];
    let aligned = vertex_count.div_ceil(BYTE_GROUP_SIZE) * BYTE_GROUP_SIZE;

    let mut at = 0usize;
    for (component, previous_slot) in last_vertex.iter_mut().enumerate().take(vertex_size) {
        let consumed = decode_bytes(&data[at..], &mut buffer[..aligned])?;
        at += consumed;

        // Each column is a delta chain against the same component of the previous vertex, which
        // for the first block is the stream's tail — see `decode_vertex_buffer`.
        let mut previous = *previous_slot;
        let mut offset = component;
        for slot in buffer.iter().take(vertex_count) {
            let value = unzigzag8(*slot).wrapping_add(previous);
            transposed[offset] = value;
            previous = value;
            offset += vertex_size;
        }
    }

    out[..vertex_count * vertex_size].copy_from_slice(&transposed);
    last_vertex[..vertex_size]
        .copy_from_slice(&transposed[vertex_size * (vertex_count - 1)..vertex_count * vertex_size]);
    Some(at)
}

/// Decodes a meshopt vertex buffer into `vertex_count` vertices of `vertex_size` bytes.
///
/// # The tail is the initial predictor
///
/// The stream ends with one vertex's worth of bytes, and that is not padding: it is the value the
/// *first* block's deltas are taken against. So the decoder reads the end of the stream before it
/// reads the beginning, and a stream whose tail is the wrong size decodes every vertex wrong by a
/// constant rather than failing — which is why the tail size is checked exactly.
///
/// # Errors
///
/// [`MeshoptError`] when the header is not this codec's, the vertex size is one it cannot
/// express, the stream ends early, or the tail is not exactly the size the format fixes.
pub fn decode_vertex_buffer(
    vertex_count: usize,
    vertex_size: usize,
    buffer: &[u8],
) -> Result<Vec<u8>, MeshoptError> {
    if vertex_size == 0 || vertex_size > 256 || !vertex_size.is_multiple_of(4) {
        return Err(MeshoptError::VertexSize(vertex_size));
    }
    if buffer.len() < 1 + vertex_size {
        return Err(MeshoptError::Truncated);
    }

    let header = buffer[0];
    if header & 0xf0 != VERTEX_HEADER || header & 0x0f != 0 {
        return Err(MeshoptError::Header { what: "vertex" });
    }

    let mut last_vertex = vec![0u8; 256];
    last_vertex[..vertex_size].copy_from_slice(&buffer[buffer.len() - vertex_size..]);

    let mut out = vec![0u8; vertex_count * vertex_size];
    let block = vertex_block_size(vertex_size);
    let mut at = 1usize;
    let mut offset = 0usize;

    while offset < vertex_count {
        let size = block.min(vertex_count - offset);
        let consumed = decode_vertex_block(
            &buffer[at..],
            &mut out[offset * vertex_size..],
            size,
            vertex_size,
            &mut last_vertex,
        )
        .ok_or(MeshoptError::Truncated)?;
        at += consumed;
        offset += size;
    }

    let expected = TAIL_MAX_SIZE.max(vertex_size);
    let actual = buffer.len() - at;
    if actual != expected {
        return Err(MeshoptError::Tail { actual, expected });
    }
    Ok(out)
}

/// Undoes the exponential filter: `2^e * m`, with a shared exponent per component.
///
/// The value is a signed 24-bit mantissa and a signed 8-bit exponent in one word. The reference
/// builds `2^e` by placing the biased exponent directly into a float's bits and multiplying —
/// which is `ldexp` without the library call, and is transcribed as such because the rounding of
/// the multiply is part of the result.
pub fn decode_filter_exponential(data: &mut [u8]) {
    for word in data.as_chunks_mut::<4>().0 {
        let value = u32::from_le_bytes(*word);
        // Sign-extend the low 24 bits, and take the top 8 as a signed exponent.
        #[allow(clippy::cast_possible_wrap)]
        let mantissa = ((value << 8) as i32) >> 8;
        #[allow(clippy::cast_possible_wrap)]
        let exponent = (value as i32) >> 24;

        // `2^e` as bits, then one multiply. `from_bits` where the reference uses a union: the
        // arithmetic is identical and this needs no unsafe to express it.
        #[allow(clippy::cast_sign_loss)]
        let scale = f32::from_bits(((exponent + 127) as u32) << 23);
        #[allow(clippy::cast_precision_loss)]
        let result = scale * mantissa as f32;
        *word = result.to_bits().to_le_bytes();
    }
}

/// Undoes the octahedral filter for signed-byte normals.
///
/// A unit vector stored as two signed components with the third reconstructed, which is how a
/// normal costs three bytes instead of twelve. The `z < 0` fold is what makes the mapping cover
/// the whole sphere rather than one hemisphere.
pub fn decode_filter_octahedral8(data: &mut [u8]) {
    const MAX: f32 = 127.0;
    for quad in data.as_chunks_mut::<4>().0 {
        #[allow(clippy::cast_possible_wrap)]
        let mut x = f32::from(quad[0] as i8);
        #[allow(clippy::cast_possible_wrap)]
        let mut y = f32::from(quad[1] as i8);
        #[allow(clippy::cast_possible_wrap)]
        let z = f32::from(quad[2] as i8) - x.abs() - y.abs();

        let fold = if z >= 0.0 { 0.0 } else { z };
        x += if x >= 0.0 { fold } else { -fold };
        y += if y >= 0.0 { fold } else { -fold };

        let length = (x * x + y * y + z * z).sqrt();
        let scale = MAX / length;

        // Round away from zero, which is what the reference's `+ (v >= 0 ? 0.5 : -0.5)` does.
        let round = |value: f32| -> i8 {
            #[allow(clippy::cast_possible_truncation)]
            let rounded = (value * scale + if value >= 0.0 { 0.5 } else { -0.5 }) as i32;
            #[allow(clippy::cast_possible_truncation)]
            {
                rounded.clamp(-128, 127) as i8
            }
        };
        #[allow(clippy::cast_sign_loss)]
        {
            quad[0] = round(x) as u8;
            quad[1] = round(y) as u8;
            quad[2] = round(z) as u8;
        }
    }
}

const INDEX_HEADER: u8 = 0xe0;
const FIFO: usize = 16;

/// Reads meshoptimizer's variable-length integer, low group first.
fn decode_vbyte(data: &[u8], at: &mut usize) -> Option<u32> {
    let mut result = 0u32;
    let mut shift = 0u32;
    // Five groups of seven bits is thirty-five, which is every u32 and one bit spare. The
    // reference stops there too rather than looping: a sixth group would be a stream that cannot
    // be describing an index.
    for _ in 0..5 {
        let byte = *data.get(*at)?;
        *at += 1;
        result |= u32::from(byte & 127) << shift;
        if byte < 128 {
            return Some(result);
        }
        shift += 7;
    }
    Some(result)
}

/// Reads one delta-coded free index.
fn decode_index(data: &[u8], at: &mut usize, last: u32) -> Option<u32> {
    let value = decode_vbyte(data, at)?;
    // Zigzag, so a small negative delta is a small number.
    let delta = (value >> 1) ^ 0u32.wrapping_sub(value & 1);
    Some(last.wrapping_add(delta))
}

/// Decodes a meshopt triangle index buffer.
///
/// # What the codec is doing
///
/// A triangle list that has been ordered for the vertex cache reuses its vertices in a tight
/// window, and the codec exploits exactly that: two sixteen-entry FIFOs remember recently emitted
/// edges and vertices, and most triangles are coded as "the edge I saw `n` triangles ago, plus a
/// vertex I saw `m` vertices ago" — one byte for a whole triangle. Only a vertex that is genuinely
/// new costs a delta-coded integer.
///
/// The FIFO pushes have to match the encoder step for step; a decoder that pushed in a different
/// order produces a mesh whose triangles are all valid indices and all in the wrong places. That
/// is why the pushes below are transcribed in the reference's own order rather than tidied.
///
/// # Errors
///
/// [`MeshoptError`] when the header is wrong, the stream is too short for the triangles it
/// claims, or the data cursor does not land exactly on the auxiliary table — which is the
/// codec's own end-of-stream check.
pub fn decode_index_buffer(index_count: usize, buffer: &[u8]) -> Result<Vec<u32>, MeshoptError> {
    if !index_count.is_multiple_of(3) {
        return Err(MeshoptError::Header { what: "index" });
    }
    // A header, one byte per triangle, and the sixteen-byte auxiliary table.
    if buffer.len() < 1 + index_count / 3 + 16 {
        return Err(MeshoptError::Truncated);
    }
    let header = buffer[0];
    if header & 0xf0 != INDEX_HEADER {
        return Err(MeshoptError::Header { what: "index" });
    }
    let version = header & 0x0f;
    if version > 1 {
        return Err(MeshoptError::Header { what: "index" });
    }
    // Version 1 reserves two more escape codes, so the threshold below moves with it.
    let fecmax = if version >= 1 { 13u8 } else { 15 };

    let mut edge_fifo = [[0u32; 2]; FIFO];
    let mut vertex_fifo = [0u32; FIFO];
    let mut edge_offset = 0usize;
    let mut vertex_offset = 0usize;
    let mut next = 0u32;
    let mut last = 0u32;

    let code_start = 1usize;
    let data_start = code_start + index_count / 3;
    let safe_end = buffer.len() - 16;
    let table = &buffer[safe_end..];

    let mut out = vec![0u32; index_count];
    let mut at = data_start;

    let push_vertex = |fifo: &mut [u32; FIFO], offset: &mut usize, v: u32, advance: bool| {
        fifo[*offset] = v;
        *offset = (*offset + usize::from(advance)) & 15;
    };
    let push_edge = |fifo: &mut [[u32; 2]; FIFO], offset: &mut usize, a: u32, b: u32| {
        fifo[*offset] = [a, b];
        *offset = (*offset + 1) & 15;
    };

    for triangle in 0..index_count / 3 {
        if at > safe_end {
            return Err(MeshoptError::Truncated);
        }
        let code = *buffer
            .get(code_start + triangle)
            .ok_or(MeshoptError::Truncated)?;
        let i = triangle * 3;

        if code < 0xf0 {
            let fe = usize::from(code >> 4);
            let slot = (edge_offset.wrapping_sub(1).wrapping_sub(fe)) & 15;
            let (a, b) = (edge_fifo[slot][0], edge_fifo[slot][1]);
            let fec = code & 15;

            let c = if fec < fecmax {
                let slot = (vertex_offset.wrapping_sub(1).wrapping_sub(usize::from(fec))) & 15;
                let c = if fec == 0 { next } else { vertex_fifo[slot] };
                if fec == 0 {
                    next += 1;
                }
                push_vertex(&mut vertex_fifo, &mut vertex_offset, c, fec == 0);
                c
            } else {
                // 13 and 14 decode as -1 and +1 against the last free index; 15 is a full delta.
                let c = if fec != 15 {
                    #[allow(clippy::cast_possible_wrap)]
                    let step = i32::from(fec) - i32::from(fec ^ 3);
                    #[allow(clippy::cast_sign_loss)]
                    let stepped = last.wrapping_add(step as u32);
                    stepped
                } else {
                    decode_index(buffer, &mut at, last).ok_or(MeshoptError::Truncated)?
                };
                last = c;
                push_vertex(&mut vertex_fifo, &mut vertex_offset, c, true);
                c
            };

            out[i] = a;
            out[i + 1] = b;
            out[i + 2] = c;
            push_edge(&mut edge_fifo, &mut edge_offset, c, b);
            push_edge(&mut edge_fifo, &mut edge_offset, a, c);
        } else if code < 0xfe {
            // The common case for a fresh triangle: the auxiliary byte comes from the table.
            let aux = table[usize::from(code & 15)];
            let feb = usize::from(aux >> 4);
            let fec = usize::from(aux & 15);

            let a = next;
            next += 1;
            let b = if feb == 0 {
                let b = next;
                next += 1;
                b
            } else {
                vertex_fifo[(vertex_offset.wrapping_sub(feb)) & 15]
            };
            let c = if fec == 0 {
                let c = next;
                next += 1;
                c
            } else {
                vertex_fifo[(vertex_offset.wrapping_sub(fec)) & 15]
            };

            out[i] = a;
            out[i + 1] = b;
            out[i + 2] = c;
            push_vertex(&mut vertex_fifo, &mut vertex_offset, a, true);
            push_vertex(&mut vertex_fifo, &mut vertex_offset, b, feb == 0);
            push_vertex(&mut vertex_fifo, &mut vertex_offset, c, fec == 0);
            push_edge(&mut edge_fifo, &mut edge_offset, b, a);
            push_edge(&mut edge_fifo, &mut edge_offset, c, b);
            push_edge(&mut edge_fifo, &mut edge_offset, a, c);
        } else {
            // The auxiliary byte is spelled out rather than looked up.
            let aux = *buffer.get(at).ok_or(MeshoptError::Truncated)?;
            at += 1;
            let fea = if code == 0xfe { 0usize } else { 15 };
            let feb = usize::from(aux >> 4);
            let fec = usize::from(aux & 15);

            // An auxiliary byte of zero that was not a table entry is the encoder's reset.
            if aux == 0 {
                next = 0;
            }

            let mut a = if fea == 0 {
                let a = next;
                next += 1;
                a
            } else {
                0
            };
            let mut b = if feb == 0 {
                let b = next;
                next += 1;
                b
            } else {
                vertex_fifo[(vertex_offset.wrapping_sub(feb)) & 15]
            };
            let mut c = if fec == 0 {
                let c = next;
                next += 1;
                c
            } else {
                vertex_fifo[(vertex_offset.wrapping_sub(fec)) & 15]
            };

            if fea == 15 {
                a = decode_index(buffer, &mut at, last).ok_or(MeshoptError::Truncated)?;
                last = a;
            }
            if feb == 15 {
                b = decode_index(buffer, &mut at, last).ok_or(MeshoptError::Truncated)?;
                last = b;
            }
            if fec == 15 {
                c = decode_index(buffer, &mut at, last).ok_or(MeshoptError::Truncated)?;
                last = c;
            }

            out[i] = a;
            out[i + 1] = b;
            out[i + 2] = c;
            push_vertex(&mut vertex_fifo, &mut vertex_offset, a, true);
            push_vertex(
                &mut vertex_fifo,
                &mut vertex_offset,
                b,
                feb == 0 || feb == 15,
            );
            push_vertex(
                &mut vertex_fifo,
                &mut vertex_offset,
                c,
                fec == 0 || fec == 15,
            );
            push_edge(&mut edge_fifo, &mut edge_offset, b, a);
            push_edge(&mut edge_fifo, &mut edge_offset, c, b);
            push_edge(&mut edge_fifo, &mut edge_offset, a, c);
        }
    }

    // The codec's own end-of-stream check: the data cursor must land exactly on the auxiliary
    // table. Anything else means a triangle consumed the wrong number of bytes, and every index
    // after it is wrong.
    if at != safe_end {
        return Err(MeshoptError::Tail {
            actual: at,
            expected: safe_end,
        });
    }
    Ok(out)
}
